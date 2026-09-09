//! The `scope-is-a-filter-on-every-read-path` check.
//!
//! Not one of DESIGN §3.3's seven; see
//! [`scope_is_a_filter_on_every_read_path`] for what it asserts and
//! [`super::super::SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH`] for why it is
//! named outside them.

use std::str::FromStr;

use bigdecimal::BigDecimal;
use rust_decimal::Decimal;
use toolkit_odata::ast;
use uuid::Uuid;

use crate::contract::fixtures::{
    CONTRACT_METER_TYPE_ID, CONTRACT_RESOURCE_TYPE, CONTRACT_TENANT_ID, FIXTURE_EPOCH,
    contract_query_with_scope, fixture_record_for_tenant, violation,
};
use crate::contract::{ContractViolation, HARNESS_FAULT, SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH};
use crate::error::UsageCollectorPluginError;
use crate::models::{AggregationFold, IdempotencyKey, MeterTypeId, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::time_range::TimeRange;

/// The start of the covered period both entries carry, and the inclusive
/// lower bound of the range this check reads and folds over.
///
/// A hundred and fifty days past [`FIXTURE_EPOCH`], for the reason
/// [`WINDOW_SELECTION_FROM`](super::window_end_selection::WINDOW_SELECTION_FROM)
/// gives. It matters here because the fold half asserts a *value*: an entry
/// from another check inside this range would be added to the total and
/// read as a scope failure.
const SCOPE_WINDOW_FROM: time::OffsetDateTime =
    FIXTURE_EPOCH.saturating_add(time::Duration::days(150));

/// The exclusive upper bound of the covered period **both** entries carry.
///
/// One period, not two, and that is load-bearing rather than tidiness. The
/// range under test selects on the period end, so two entries an hour apart
/// give a backend a second way to tell them apart - one the scope has
/// nothing to do with. A backend that ignored `query.filter` entirely but
/// bounded its read short of the later entry would then answer the admitted
/// row and drop the excluded one, and the raw and fold halves would read
/// that as a scope honoured. Over one period there is no such reading: any
/// range that returns either entry returns both unless the scope withholds
/// one.
const SCOPE_PERIOD_END: time::OffsetDateTime =
    SCOPE_WINDOW_FROM.saturating_add(time::Duration::hours(1));

/// The exclusive upper bound of the range under test: an hour past
/// [`SCOPE_PERIOD_END`], so both entries are selected by their end.
const SCOPE_WINDOW_TO: time::OffsetDateTime =
    SCOPE_PERIOD_END.saturating_add(time::Duration::hours(1));

/// The tenant the scope under test does **not** admit.
///
/// Distinct from [`CONTRACT_TENANT_ID`], which is what the dispatched scope
/// pins. The two entries this check stores agree on the meter, the resource
/// and the covered period, and differ in three things: this tenant, the
/// idempotency key each submits under, and the quantity each carries. Only
/// the first is an attribute the dispatched scope names - it reads
/// `tenant_id` and `resource_type`, and both entries carry the same
/// `resource_type` - so `tenant_id` is the only thing the scope can decide
/// them on. The other two differ for reasons of their own:
/// [`SCOPE_EXCLUDED_QUANTITY`] says why the quantities do, and the keys are
/// role-named for the reason
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture)
/// spells out. Neither is visible to the scope, and neither is visible to
/// the range under test.
const SCOPE_EXCLUDED_TENANT_ID: Uuid = Uuid::from_u128(0xc047_c047_0000_4000_8000_0000_0000_0002);

/// A tenant the scope under test admits and that owns no entry.
///
/// It is the second disjunct, and it is what keeps the dispatched
/// expression the shape `authz::scope_to_odata_filter` actually compiles: a
/// grant over two tenants projects to an `Or` of two tenant-pinned
/// conjunctions, not to a single `Compare`.
///
/// Shape coverage is the whole of what it buys, and it is worth being exact
/// about which mis-handlings of an `Or` that does and does not reach. A
/// backend evaluating only the **last** disjunct fails the admitted half of
/// all three assertions: this tenant owns nothing, and the admitted entry
/// sits in the disjunct such a backend discarded. A backend evaluating only
/// the **first** passes everything here, because the admitted entry belongs
/// to it — this fixture does not catch that one. Nor does it catch a
/// backend admitting whatever any disjunct mentions, which is
/// indistinguishable from correct handling while this tenant owns no row.
const SCOPE_UNUSED_TENANT_ID: Uuid = Uuid::from_u128(0xc047_c047_0000_4000_8000_0000_0000_0003);

/// The admitted entry's quantity, and the whole of the total the fold must
/// report.
const SCOPE_ADMITTED_QUANTITY: &str = "3.5";

/// The excluded entry's quantity.
///
/// Distinct from [`SCOPE_ADMITTED_QUANTITY`] and large beside it, so the
/// two answers a backend can give are two different numbers:
/// `SCOPE_ADMITTED_QUANTITY` alone when the scope is enforced, and
/// `SCOPE_ADMITTED_QUANTITY + SCOPE_EXCLUDED_QUANTITY` when it is not.
const SCOPE_EXCLUDED_QUANTITY: &str = "500";

/// The read limit this check dispatches: twice the two entries it stores.
///
/// The margin is the assertion, for the reason
/// [`DEDUP_PAGE_LIMIT`](super::dedup_identity_over_window::DEDUP_PAGE_LIMIT)
/// gives. A limit of one would truncate a wrongly returned foreign-tenant
/// row away and turn the half of this check that catches an unenforced
/// scope into a pass.
const SCOPE_PAGE_LIMIT: u64 = 4;

/// The two entries this check stores, and the total the fold must report
/// over them.
struct ScopeFixtures {
    /// The entry the dispatched scope admits.
    admitted: UsageRecord,
    /// The entry it withholds. Same meter, same resource, same covered
    /// period; a different tenant.
    excluded: UsageRecord,
    /// [`Self::admitted`]'s quantity on the aggregate surface's carrier.
    expected_total: BigDecimal,
}

/// `scope-is-a-filter-on-every-read-path` — the compiled PDP scope decides
/// which stored entries a plugin may answer with, on all three read paths.
///
/// **Not one of DESIGN §3.3's seven checks.** DESIGN states the obligation
/// rather than tabulating a check for it: §3.3 gives the SPI's
/// `get_usage_record` the doc *"`scope` is the compiled PDP scope,
/// projected into a `toolkit_odata` filter. A row outside it is not
/// returned"*, and its consumer-surface twin *"The read runs under the
/// compiled scope, so an entry outside it is `NotFound`. This surface is
/// not an existence oracle."* §3.2 puts the same rule on the collection
/// paths: the Query Gateway composes the PDP constraints with the caller's
/// filters *"so the result can only narrow"*. With the point lookup's
/// in-process per-record attribution check retired, all three paths carry
/// the scope as a filter and nothing above the SPI re-checks the rows that
/// come back — so the whole guarantee is the plugin's, and until this check
/// existed nothing asserted it.
///
/// **Every assertion is a pair, and neither half is sufficient alone.** A
/// scope that admits no row is satisfied by a backend that answers nothing;
/// a scope that admits every row is satisfied by a backend that ignores it.
/// So two entries are stored over one covered period, and the scope
/// dispatched admits exactly one of them: each assertion requires the
/// admitted entry back *and* the excluded one withheld. `tenant_id` is the
/// only attribute the scope names that the two disagree on, and one covered
/// period is what leaves the range under test nothing to separate them by
/// either - see [`SCOPE_PERIOD_END`].
///
/// * **Point lookup.** The admitted entry answers `Ok`; the excluded one
///   answers [`UsageCollectorPluginError::UsageRecordNotFound`]. The
///   **variant** is asserted, not merely that the lookup failed: a backend
///   answering a distinguishable denial turns this surface into an
///   existence oracle, telling anyone who can guess a uuid that another
///   tenant holds an entry under it.
/// * **Raw path.** `list_usage_records` with the scope in `query.filter`
///   returns the admitted entry and not the excluded one.
/// * **Fold.** `query_aggregated_usage_records` over the same filter
///   reports the admitted entry's quantity — the **value**, so a backend
///   answering an empty bucket set fails rather than passing on a vacuous
///   "the excluded row contributed nothing".
///
/// The scope is a disjunction of tenant-pinned conjunctions, which is what
/// `authz::scope_to_odata_filter` projects, and each conjunction carries a
/// second predicate so a backend cannot pass by special-casing a bare
/// `tenant_id eq`. Both entries satisfy that second predicate, so
/// `tenant_id` is the only thing deciding them.
pub async fn scope_is_a_filter_on_every_read_path(
    plugin: &dyn UsageCollectorPluginV1,
) -> Vec<ContractViolation> {
    let fixtures = match scope_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own \
                     `{SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH}` fixtures, so nothing was submitted. \
                     This is a fault in the suite, not in the plugin under test: {detail}"
                ),
            )];
        }
    };

    // A refusal here stops the check. The two entries are one scenario:
    // asserting that a scope withholds a row that was never stored, or
    // admits one that was not, says nothing about enforcement.
    for (role, record) in [
        ("admitted", &fixtures.admitted),
        ("excluded", &fixtures.excluded),
    ] {
        if let Err(err) = plugin.create_usage_record(record.clone()).await {
            return vec![violation(
                SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH,
                format!(
                    "`create_usage_record` refused the {role} entry (record {id}, tenant \
                     {tenant}), so no read path could be asked whether the compiled scope decides \
                     which entries it answers with: {err}",
                    id = record.id,
                    tenant = record.tenant_id,
                ),
            )];
        }
    }

    let mut violations = point_lookup_reads_under_the_scope(plugin, &fixtures).await;
    violations.extend(raw_path_reads_under_the_scope(plugin, &fixtures).await);
    violations.extend(fold_reads_under_the_scope(plugin, &fixtures).await);
    violations
}

/// The point-lookup pair: the admitted entry comes back, the excluded one
/// reads as absent.
async fn point_lookup_reads_under_the_scope(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &ScopeFixtures,
) -> Vec<ContractViolation> {
    let scope = scope_under_test();
    let mut violations = Vec::new();

    let admitted = fixtures.admitted.id;
    match plugin.get_usage_record(admitted, &scope).await {
        Ok(entry) if entry.id == admitted => {}
        Ok(entry) => violations.push(violation(
            SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH,
            format!(
                "`get_usage_record` was asked for record {admitted} under a scope pinning its own \
                 tenant and answered record {observed} instead. The scope narrows which entries a \
                 lookup may answer with; it does not change which entry was asked for.",
                observed = entry.id,
            ),
        )),
        Err(err) => violations.push(violation(
            SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH,
            format!(
                "`get_usage_record` refused record {admitted} under a scope one of whose \
                 tenant-pinned disjuncts names that record's own tenant ({tenant}): {err}. A \
                 scope is a filter, and this row is inside it. Without this half the check would \
                 pass against a backend that answers nothing at all.",
                tenant = fixtures.admitted.tenant_id,
            ),
        )),
    }

    let excluded = fixtures.excluded.id;
    match plugin.get_usage_record(excluded, &scope).await {
        Err(UsageCollectorPluginError::UsageRecordNotFound { id }) if id == excluded => {}
        Err(UsageCollectorPluginError::UsageRecordNotFound { id }) => violations.push(violation(
            SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH,
            format!(
                "`get_usage_record` withheld record {excluded} as `UsageRecordNotFound` and named \
                 {id} rather than the id it was asked for. A caller cannot tell a withheld entry \
                 from an absent one, so the reported id has to be the one it asked about."
            ),
        )),
        Err(other) => violations.push(violation(
            SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH,
            format!(
                "`get_usage_record` was asked for record {excluded}, which exists and belongs to \
                 tenant {tenant} - a tenant no disjunct of the dispatched scope names - and \
                 refused it as `{other}`. The one refusal a row outside the scope earns is \
                 `UsageRecordNotFound`, the same answer an id that was never stored gets. \
                 Anything a caller can tell apart from an absent row makes this surface an \
                 existence oracle: guess a uuid, read the denial, learn that another tenant holds \
                 an entry under it.",
                tenant = fixtures.excluded.tenant_id,
            ),
        )),
        Ok(entry) => violations.push(violation(
            SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH,
            format!(
                "`get_usage_record` answered record {observed}, attributed to tenant {tenant}, \
                 under a scope whose every disjunct pins a different tenant. That is a \
                 cross-tenant read. The scope reaches this method as an argument for one reason - \
                 to decide which entries it may answer with - and nothing above the SPI re-checks \
                 the row that comes back.",
                observed = entry.id,
                tenant = entry.tenant_id,
            ),
        )),
    }

    violations
}

/// The raw-path pair: the admitted entry is in the page, the excluded one
/// is not.
async fn raw_path_reads_under_the_scope(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &ScopeFixtures,
) -> Vec<ContractViolation> {
    let returned = match scope_page(plugin).await {
        Ok(returned) => returned,
        Err(detail) => return vec![violation(SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH, detail)],
    };

    let mut violations = Vec::new();
    if !returned.contains(&fixtures.admitted.id) {
        violations.push(violation(
            SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH,
            format!(
                "`list_usage_records` over the range `[{from}, {to})` did not return record \
                 {admitted}, whose tenant ({tenant}) one disjunct of the dispatched \
                 `query.filter` pins. It answered the ids {returned:?}. Composing the compiled \
                 scope into the filter can only narrow the result; it cannot drop a row the scope \
                 admits. Without this half the check would pass against a backend that answers an \
                 empty page.",
                from = SCOPE_WINDOW_FROM,
                to = SCOPE_WINDOW_TO,
                admitted = fixtures.admitted.id,
                tenant = fixtures.admitted.tenant_id,
            ),
        ));
    }
    if returned.contains(&fixtures.excluded.id) {
        violations.push(violation(
            SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH,
            format!(
                "`list_usage_records` returned record {excluded}, attributed to tenant {tenant}, \
                 under a `query.filter` whose every disjunct pins a different tenant. It answered \
                 the ids {returned:?}. The compiled scope arrives on this path inside \
                 `query.filter` and nowhere else, so a backend that ignores that slot serves \
                 every tenant's rows to whoever asks.",
                excluded = fixtures.excluded.id,
                tenant = fixtures.excluded.tenant_id,
            ),
        ));
    }
    violations
}

/// The fold half: the admitted entry's quantity is the whole of the total.
///
/// Asserted as a **value** rather than as "the excluded entry contributed
/// nothing". A `SUM` that leaves out both rows is empty, and so is one from
/// a backend that computes no fold at all, so an assertion phrased over the
/// excluded row alone would be satisfied by a backend that answers nothing.
async fn fold_reads_under_the_scope(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &ScopeFixtures,
) -> Vec<ContractViolation> {
    match scope_sum(plugin).await {
        Ok(Some(total)) if total == fixtures.expected_total => Vec::new(),
        Ok(observed) => vec![violation(
            SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH,
            format!(
                "`SUM` over the range `[{from}, {to})` reported {observed}, and the admitted \
                 entry's own quantity `{expected}` is the whole of it. The range holds that entry \
                 and one of `{excluded}` attributed to a tenant no disjunct of the dispatched \
                 `query.filter` names. A fold runs over the authorized scope, so the second entry \
                 contributes nothing: a backend that ignores `query.filter` reports \
                 `{expected}` plus `{excluded}` instead. The comparison is against the admitted \
                 entry's value rather than against the excluded one contributing zero on purpose \
                 - an empty answer is what a backend that folds nothing at all reports.",
                from = SCOPE_WINDOW_FROM,
                to = SCOPE_WINDOW_TO,
                observed = observed
                    .as_ref()
                    .map_or_else(|| "no value at all".to_owned(), ToString::to_string),
                expected = SCOPE_ADMITTED_QUANTITY,
                excluded = SCOPE_EXCLUDED_QUANTITY,
            ),
        )],
        Err(detail) => vec![violation(SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH, detail)],
    }
}

/// The compiled PDP scope this check dispatches on all three paths.
///
/// A disjunction of tenant-pinned conjunctions, which is the shape
/// `authz::scope_to_odata_filter` projects for a grant carrying two
/// constraints: one `Or`, each side an `And` whose first conjunct pins
/// `tenant_id`. The second conjunct is `resource_type`, and both stored
/// entries carry the same one, so it narrows nothing between them - it is
/// there so a backend cannot satisfy this check by pattern-matching a bare
/// `tenant_id eq` and ignoring every other node.
fn scope_under_test() -> ast::Expr {
    ast::Expr::Or(
        Box::new(tenant_conjunct(CONTRACT_TENANT_ID)),
        Box::new(tenant_conjunct(SCOPE_UNUSED_TENANT_ID)),
    )
}

/// One disjunct: `tenant_id eq <tenant> and resource_type eq <type>`.
fn tenant_conjunct(tenant_id: Uuid) -> ast::Expr {
    ast::Expr::And(
        Box::new(ast::Expr::Compare(
            Box::new(ast::Expr::Identifier("tenant_id".to_owned())),
            ast::CompareOperator::Eq,
            Box::new(ast::Expr::Value(ast::Value::Uuid(tenant_id))),
        )),
        Box::new(ast::Expr::Compare(
            Box::new(ast::Expr::Identifier("resource_type".to_owned())),
            ast::CompareOperator::Eq,
            Box::new(ast::Expr::Value(ast::Value::String(
                CONTRACT_RESOURCE_TYPE.to_owned(),
            ))),
        )),
    )
}

/// Every `UsageRecord.id` the range under test comes back with on the raw
/// path, under the scope this check dispatches.
async fn scope_page(plugin: &dyn UsageCollectorPluginV1) -> Result<Vec<Uuid>, String> {
    let meter = MeterTypeId::new(CONTRACT_METER_TYPE_ID)
        .map_err(|err| format!("the check's own meter type id is invalid: {err}"))?;
    let range = TimeRange::new(SCOPE_WINDOW_FROM, SCOPE_WINDOW_TO)
        .map_err(|err| format!("the check could not build its own read range: {err}"))?;
    let query = contract_query_with_scope(SCOPE_PAGE_LIMIT, scope_under_test());
    let page = plugin
        .list_usage_records(meter, range, &query, &[])
        .await
        .map_err(|err| {
            format!(
                "`list_usage_records` failed over the range holding the two entries the scope \
                 decides between, so whether the raw path reads under the scope could not be \
                 decided: {err}"
            )
        })?;
    Ok(page.items.into_iter().map(|item| item.id).collect())
}

/// The `SUM` one bucket carries over the range under test, under the same
/// scope.
///
/// `group_by` is empty, which the aggregate surface fixes as the
/// no-grouping case: a single bucket with an empty key. Any other bucket
/// count is reported rather than picked from, since there would be no one
/// total to compare.
async fn scope_sum(plugin: &dyn UsageCollectorPluginV1) -> Result<Option<BigDecimal>, String> {
    let meter = MeterTypeId::new(CONTRACT_METER_TYPE_ID)
        .map_err(|err| format!("the check's own meter type id is invalid: {err}"))?;
    let range = TimeRange::new(SCOPE_WINDOW_FROM, SCOPE_WINDOW_TO)
        .map_err(|err| format!("the check could not build its own read range: {err}"))?;
    let query = contract_query_with_scope(SCOPE_PAGE_LIMIT, scope_under_test());
    let result = plugin
        .query_aggregated_usage_records(meter, range, AggregationFold::Sum, &query, &[], &[])
        .await
        .map_err(|err| {
            format!(
                "`query_aggregated_usage_records` failed over the range holding the two entries \
                 the scope decides between, so whether the fold runs over the authorized scope \
                 could not be decided: {err}"
            )
        })?;
    let count = result.buckets.len();
    let mut buckets = result.buckets.into_iter();
    match (buckets.next(), buckets.next()) {
        (Some(bucket), None) => Ok(bucket.value),
        _ => Err(format!(
            "`query_aggregated_usage_records` was dispatched with no grouping dimension, which \
             the aggregate surface fixes as the no-grouping case - a single bucket carrying an \
             empty key - and it answered {count} buckets. There is no one total to compare the \
             admitted entry's quantity against."
        )),
    }
}

/// Builds the entry the scope admits and the entry it withholds.
///
/// Both carry [`SCOPE_WINDOW_FROM`] to [`SCOPE_PERIOD_END`] - one period,
/// for the reason [`SCOPE_PERIOD_END`] gives.
///
/// Three guards keep the check from passing by construction, and all three
/// are the suite's own facts rather than the plugin's:
///
/// * The two tenants differ. If they did not, the scope would admit both
///   entries or neither, and nothing here would be discriminating.
/// * The two entries derive different ids. `tenant_id` is one of the five
///   identity attributes and the idempotency key is another, so two entries
///   over one period still derive two ids - but if they ever did not, the
///   second submission would be an idempotent replay of the first and the
///   check would be asserting two things about one row. The guard matters
///   more now that the period no longer separates them.
/// * The two quantities differ. If they did not, a fold that counted the
///   excluded entry would report a total this check could not tell from the
///   right one.
fn scope_fixtures() -> Result<ScopeFixtures, String> {
    if CONTRACT_TENANT_ID == SCOPE_EXCLUDED_TENANT_ID {
        return Err(format!(
            "the admitted and the excluded entry are attributed to the same tenant \
             ({CONTRACT_TENANT_ID}), so the dispatched scope admits both or neither and there is \
             nothing for this check to assert"
        ));
    }

    let admitted_value = Decimal::from_str(SCOPE_ADMITTED_QUANTITY).map_err(|err| {
        format!(
            "the check's own admitted quantity `{SCOPE_ADMITTED_QUANTITY}` does not parse: {err}"
        )
    })?;
    let excluded_value = Decimal::from_str(SCOPE_EXCLUDED_QUANTITY).map_err(|err| {
        format!(
            "the check's own excluded quantity `{SCOPE_EXCLUDED_QUANTITY}` does not parse: {err}"
        )
    })?;
    if admitted_value == excluded_value {
        return Err(format!(
            "the admitted and the excluded quantity are both `{SCOPE_ADMITTED_QUANTITY}`, so a \
             fold counting the entry outside the scope would report the same total as one \
             enforcing it and this check would pass by construction"
        ));
    }
    let expected_total = BigDecimal::from_str(SCOPE_ADMITTED_QUANTITY).map_err(|err| {
        format!(
            "the check's own admitted quantity `{SCOPE_ADMITTED_QUANTITY}` does not widen to the \
             aggregate carrier: {err}"
        )
    })?;

    let admitted = fixture_record_for_tenant(
        CONTRACT_TENANT_ID,
        &scope_key("admitted")?,
        admitted_value,
        SCOPE_WINDOW_FROM,
        SCOPE_PERIOD_END,
    )?;
    let excluded = fixture_record_for_tenant(
        SCOPE_EXCLUDED_TENANT_ID,
        &scope_key("excluded")?,
        excluded_value,
        SCOPE_WINDOW_FROM,
        SCOPE_PERIOD_END,
    )?;
    if admitted.id == excluded.id {
        return Err(format!(
            "the admitted and the excluded entry derive one id ({id}), so the second submission \
             would be an idempotent replay of the first and this check would be asserting two \
             things about one row",
            id = admitted.id,
        ));
    }

    Ok(ScopeFixtures {
        admitted,
        excluded,
        expected_total,
    })
}

/// The idempotency key one side of this check's fixture submits under.
///
/// Keyed on the role name for the reason
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture)
/// spells out: in a ledger with no delete path, an edited fixture must take
/// a fresh identity rather than inherit an accepted entry's.
fn scope_key(role: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}
