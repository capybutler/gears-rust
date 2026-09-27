//! The DESIGN §3.3 `record-and-invalidation-distinct-identity` check.
//!
//! See [`record_and_invalidation_distinct_identity`] for what it asserts;
//! the module holds the record, the invalidation that repeats its key, and
//! the retry of the record submitted after both.

use std::collections::BTreeSet;

use uuid::Uuid;

use crate::contract::fixtures::{
    CONTRACT_ACCEPTED_AT, CONTRACT_TENANT_ID, FIXTURE_EPOCH, check_meter, contract_query,
    fixture_invalidation, fixture_record_on, violation,
};
use crate::contract::{
    ContractViolation, HARNESS_FAULT, RECORD_AND_INVALIDATION_DISTINCT_IDENTITY,
};
use crate::models::{IdempotencyKey, MeterTypeId, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::time_range::TimeRange;

/// The start of the covered period this check's two entries share, and the
/// inclusive lower bound of the range it reads them back over.
///
/// A hundred and eighty days past [`FIXTURE_EPOCH`], for the reason
/// [`WINDOW_SELECTION_FROM`](super::window_end_selection::WINDOW_SELECTION_FROM)
/// gives, and clear of the day-0, 30, 60, 90, 120, 150, 210, 240 and 270
/// offsets the other checks take. It matters here because this check counts
/// the rows a range returns, so a stray entry inside it would be read as a
/// third entry.
///
/// The offset is the second of two separations rather than the only one:
/// this check also reads over a meter of its own (see
/// [`distinct_identity_fixtures`]), and a range and a meter that no other
/// check writes to are independent reasons why nothing else can land in the
/// page this check counts.
const DISTINCT_IDENTITY_WINDOW_FROM: time::OffsetDateTime =
    FIXTURE_EPOCH.saturating_add(time::Duration::days(180));

/// The end of that covered period. Both entries carry it: an invalidation
/// repeats its target's period, which is what makes the two a same-period
/// pair rather than two entries a range could separate.
const DISTINCT_IDENTITY_WINDOW_END: time::OffsetDateTime =
    DISTINCT_IDENTITY_WINDOW_FROM.saturating_add(time::Duration::hours(1));

/// The exclusive upper bound of the range this check reads over, an hour
/// past [`DISTINCT_IDENTITY_WINDOW_END`] so both entries are selected by
/// their period end.
const DISTINCT_IDENTITY_WINDOW_TO: time::OffsetDateTime =
    DISTINCT_IDENTITY_WINDOW_END.saturating_add(time::Duration::hours(1));

/// The quantity the record carries, and the quantity its invalidation
/// echoes rather than negates
/// (`cpt-cf-usage-collector-adr-append-only-invalidation`).
///
/// Its value is not asserted: this check counts entries and never folds
/// them. Where it is read at all is the retry comparison - an absorbed
/// retry is decided on every caller-supplied field, quantity included - and
/// the retry is a clone of the record, so the two agree whatever this
/// literal says.
const DISTINCT_IDENTITY_QUANTITY: &str = "5.5";

/// The read limit this check dispatches: twice the two entries it expects.
///
/// The margin is the assertion, for the reason
/// [`DEDUP_PAGE_LIMIT`](super::dedup_identity_over_window::DEDUP_PAGE_LIMIT)
/// gives and `invalidation_excluded_from_fold` repeats over its own three
/// entries. A limit set to the expected two would truncate a third row away,
/// and the half of this check that catches a backend storing something
/// extra - an invalidation admitted twice, say, or a retry stored as a
/// second row - would silently pass.
const DISTINCT_IDENTITY_PAGE_LIMIT: u64 = 4;

/// The two entries this check submits, and the meter it reads them back
/// over.
struct DistinctIdentityFixtures {
    /// The meter both entries are written to and the only meter this check
    /// reads.
    meter: MeterTypeId,
    /// The ordinary measurement, submitted first and retried last.
    record: UsageRecord,
    /// The invalidation of [`Self::record`], repeating its idempotency key
    /// and its covered period and departing from it in `entry_type` and the
    /// reason code alone.
    invalidation: UsageRecord,
}

/// `record-and-invalidation-distinct-identity` — *"A record, then its
/// invalidation under the same key and period, then a retry of the record:
/// all accepted, the retry absorbed, and a read returns exactly two
/// entries."*
///
/// **The row's phrase "under the same key" is the whole of the check.**
/// DESIGN §3.1's "Dedup identity" row gives the identity as
/// `(tenant_id, gts_type_id, idempotency_key, window_start, window_end,
/// entry_type)` and says of it: *"A record and its invalidation share the
/// first five components and differ in `entry_type`, so the key alone does
/// not tell them apart."* §3.3's plugin obligation names the mistake that
/// follows: *"Enforce the six-part identity, `entry_type` included. A
/// plugin that deduplicates on the other five components alone treats every
/// invalidation as a collision with its target."*
///
/// So the fixture is deliberately the hardest pair a plugin can be handed:
/// one idempotency key, one covered period, one tenant, one meter, and two
/// entries. Nothing but the sixth identity input separates them, and a
/// backend whose unique constraint, conflict target or in-batch dedup map
/// leaves that input out sees a collision where the gear sees two entries.
///
/// Four properties are asserted, each reported on its own so they fail
/// independently:
///
/// 1. **The record is accepted.**
/// 2. **Its invalidation is accepted.** This is the assertion DESIGN's
///    obligation is about: a plugin deduplicating on the other five
///    components refuses here, believing the invalidation a collision with
///    the record it names.
/// 3. **A retry of the record is absorbed**, answering with the stored
///    entry. Submitted *after* the invalidation rather than before it,
///    which is the order the row gives and the order that discriminates: a
///    backend that resolved the retry against the invalidation - the other
///    entry sharing its five components - would answer with an entry the
///    caller never sent. This is the one property here no other check in
///    the suite makes, and `contract_mutants`'s
///    `Defect::ConflictReadBackIgnoresTheEntryType` is the subject built to
///    break it: it admits on all six identity inputs and reads the colliding
///    entry back on five, which is the second of the three places DESIGN
///    §3.3 obliges `entry_type` to appear.
/// 4. **A read returns exactly two entries**, both of them the ones
///    submitted, and the invalidation among them still naming its target.
///
/// The count is asserted at two rather than at "at least two" on purpose.
/// Two is what separates a plugin that tells the pair apart from one that
/// tells them apart *and* absorbs the retry: a backend storing the retry as
/// a third row satisfies every other property here and fails only this one.
pub async fn record_and_invalidation_distinct_identity(
    plugin: &dyn UsageCollectorPluginV1,
) -> Vec<ContractViolation> {
    let fixtures = match distinct_identity_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own \
                     `{RECORD_AND_INVALIDATION_DISTINCT_IDENTITY}` fixtures, so nothing was \
                     submitted. This is a fault in the suite, not in the plugin under test: \
                     {detail}"
                ),
            )];
        }
    };
    let mut violations = Vec::new();

    // Property one. A refusal here is reported and the check stops: the
    // retry and the read are both statements about a stored record, and
    // asserting either over an entry the backend never admitted would say
    // nothing about the identity rule.
    if let Err(err) = plugin.create_usage_record(fixtures.record.clone()).await {
        violations.push(violation(
            RECORD_AND_INVALIDATION_DISTINCT_IDENTITY,
            format!(
                "`create_usage_record` refused the ordinary measurement this check withdraws \
                 (record {id}, idempotency key `{key}`, period `{start}` to `{end}`): {err}. It \
                 is the first entry of its identity on this meter, so there is nothing for it to \
                 collide with and nothing left to assert about the invalidation that follows it.",
                id = fixtures.record.id,
                key = fixtures.record.idempotency_key.as_str(),
                start = fixtures.record.window_start,
                end = fixtures.record.window_end,
            ),
        ));
        return violations;
    }

    // Property two, and the one DESIGN's obligation names. Reported and
    // carried past rather than returned on: properties three and four are
    // about the record, which is stored, and each answers a question this
    // refusal does not.
    if let Err(err) = plugin
        .create_usage_record(fixtures.invalidation.clone())
        .await
    {
        violations.push(violation(
            RECORD_AND_INVALIDATION_DISTINCT_IDENTITY,
            format!(
                "`create_usage_record` refused the invalidation ({invalidation}) of record \
                 {record}: {err}. The two repeat one idempotency key over one covered period and \
                 differ in `entry_type`, which is the sixth input to the derived id - so they are \
                 two entries rather than one, and both are accepted. A plugin that deduplicates \
                 on the other five components alone treats every invalidation as a collision \
                 with its target and refuses exactly here; everything keyed on identity must \
                 include `entry_type` or key on `id`, which covers all six inputs.",
                invalidation = fixtures.invalidation.id,
                record = fixtures.record.id,
            ),
        ));
    }

    violations.extend(the_retry_is_absorbed(plugin, &fixtures).await);
    violations.extend(the_read_returns_the_pair(plugin, &fixtures).await);
    violations
}

/// Property three: resubmitting the record after its invalidation is an
/// idempotent replay answering with the stored record.
///
/// The comparison is on the id **and** on every caller-supplied field
/// ([`UsageRecord::caller_supplied_eq`]), not on the id alone. A backend
/// that answered the invalidation here would be caught by the id; one that
/// answered a differently-shaped entry under the right id would not be, and
/// an absorbed retry's whole promise is that the caller is handed what is
/// stored.
async fn the_retry_is_absorbed(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DistinctIdentityFixtures,
) -> Vec<ContractViolation> {
    match plugin.create_usage_record(fixtures.record.clone()).await {
        Ok(stored)
            if stored.id == fixtures.record.id && stored.caller_supplied_eq(&fixtures.record) =>
        {
            Vec::new()
        }
        outcome => vec![violation(
            RECORD_AND_INVALIDATION_DISTINCT_IDENTITY,
            format!(
                "resubmitting record {record} verbatim, after its invalidation ({invalidation}) \
                 had been accepted, answered {outcome:?}. A re-delivery of an accepted entry is \
                 an idempotent replay: the stored record comes back, field for field, and it is \
                 the record rather than the invalidation - the two share five identity \
                 components, so a backend resolving the retry against the invalidation hands the \
                 caller an entry it never sent.",
                record = fixtures.record.id,
                invalidation = fixtures.invalidation.id,
            ),
        )],
    }
}

/// Property four: the range holds exactly the two entries, and the
/// invalidation among them still names its target.
///
/// The target reference is reported separately and only when the entry came
/// back at all. Its absence from the page is already reported by the count,
/// and reporting it twice would read as two defects rather than one.
async fn the_read_returns_the_pair(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DistinctIdentityFixtures,
) -> Vec<ContractViolation> {
    let items = match distinct_identity_page(plugin, &fixtures.meter).await {
        Ok(items) => items,
        Err(detail) => return vec![violation(RECORD_AND_INVALIDATION_DISTINCT_IDENTITY, detail)],
    };

    let mut violations = Vec::new();
    let returned: BTreeSet<Uuid> = items.iter().map(|item| item.id).collect();
    let expected = BTreeSet::from([fixtures.record.id, fixtures.invalidation.id]);
    if items.len() != expected.len() || returned != expected {
        violations.push(violation(
            RECORD_AND_INVALIDATION_DISTINCT_IDENTITY,
            format!(
                "a record ({record}), its invalidation ({invalidation}) and a verbatim retry of \
                 the record were submitted over the range `[{from}, {to})`, and \
                 `list_usage_records` answered a row count of {count} over the ids {returned:?}. \
                 Exactly two entries are stored: the record and the invalidation are two \
                 identities because `entry_type` is the sixth input to the derived id, and the \
                 retry is neither a third entry nor a replacement for the first. Fewer than two \
                 is a plugin deduplicating on five components; more than two is a retry stored \
                 rather than absorbed.",
                record = fixtures.record.id,
                invalidation = fixtures.invalidation.id,
                from = DISTINCT_IDENTITY_WINDOW_FROM,
                to = DISTINCT_IDENTITY_WINDOW_TO,
                count = items.len(),
            ),
        ));
    }

    if let Some(entry) = items
        .iter()
        .find(|item| item.id == fixtures.invalidation.id)
    {
        let target = entry.invalidation.as_ref().map(|inv| inv.target);
        if target != Some(fixtures.record.id) {
            violations.push(violation(
                RECORD_AND_INVALIDATION_DISTINCT_IDENTITY,
                format!(
                    "the invalidation entry ({invalidation}) came back from `list_usage_records` \
                     naming {observed} as the entry it withdraws, and it withdraws {record}. The \
                     two entries are only a withdrawn pair because that reference says so - it is \
                     what marks this entry an invalidation at all, and without it a reader of the \
                     two rows sees two unrelated measurements under one idempotency key.",
                    invalidation = fixtures.invalidation.id,
                    observed = target.map_or_else(
                        || "no entry at all".to_owned(),
                        |target| format!("`{target}`")
                    ),
                    record = fixtures.record.id,
                ),
            ));
        }
    }

    violations
}

/// Every entry the range under test comes back with on the raw path.
///
/// A `Vec` rather than a set, because the count is an assertion: a set
/// would collapse a duplicated row away. `Err` carries a ready-to-report
/// detail.
async fn distinct_identity_page(
    plugin: &dyn UsageCollectorPluginV1,
    meter: &MeterTypeId,
) -> Result<Vec<UsageRecord>, String> {
    let range = TimeRange::new(DISTINCT_IDENTITY_WINDOW_FROM, DISTINCT_IDENTITY_WINDOW_TO)
        .map_err(|err| format!("the check could not build its own read range: {err}"))?;
    let page = plugin
        .list_usage_records(
            meter.clone(),
            range,
            &contract_query(DISTINCT_IDENTITY_PAGE_LIMIT),
            &[],
        )
        .await
        .map_err(|err| {
            format!(
                "`list_usage_records` failed over the range holding the record and its \
                 invalidation, so how many entries the pair is could not be decided: {err}"
            )
        })?;
    Ok(page.items)
}

/// Builds the record and the invalidation that repeats its key.
///
/// The meter is this check's own rather than the suite's shared one.
/// [`run_all`](crate::contract::run_all) dispatches every check against one
/// persistent backend that never removes an entry, and this check counts
/// the rows a range returns, so it reads a meter nothing else writes to.
///
/// Two guards keep the check from passing by construction, and both are the
/// suite's own facts rather than the plugin's — so both are reported as
/// [`HARNESS_FAULT`] rather than against the plugin:
///
/// * The two **derive different ids**. If they did not, "a read returns
///   exactly two entries" would be asserting that one identity reads twice,
///   which is the opposite rule, and the invalidation would collide with
///   the record rather than stand beside it as a second entry.
/// * The two **share an idempotency key**. The row says the invalidation
///   goes in *under the same key*, and that is the whole of what makes this
///   check discriminate: a pair under two keys collides on nothing, so
///   every backend passes and the check asserts nothing at all.
///
/// Their shared covered period is not guarded separately.
/// [`fixture_invalidation`] repeats every caller-supplied field of its
/// target and departs from it in `entry_type` and the reason code alone, so
/// the period and the key alike are the record's own by construction — and
/// the key is the one of the two the guard above names, because it is the
/// one the row calls out and the one a change to that builder would be
/// silently wrong about.
fn distinct_identity_fixtures() -> Result<DistinctIdentityFixtures, String> {
    let meter = check_meter(RECORD_AND_INVALIDATION_DISTINCT_IDENTITY, "main")?;
    let key = IdempotencyKey::new(RECORD_AND_INVALIDATION_DISTINCT_IDENTITY)
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))?;
    let quantity = UsageQuantity::parse(DISTINCT_IDENTITY_QUANTITY).map_err(|err| {
        format!("the check's own quantity `{DISTINCT_IDENTITY_QUANTITY}` does not parse: {err}")
    })?;

    let record = fixture_record_on(
        meter.clone(),
        CONTRACT_TENANT_ID,
        &key,
        quantity,
        CONTRACT_ACCEPTED_AT,
        DISTINCT_IDENTITY_WINDOW_FROM,
        DISTINCT_IDENTITY_WINDOW_END,
    )?;
    let invalidation = fixture_invalidation(&record)?;

    if invalidation.id == record.id {
        return Err(format!(
            "the record and its invalidation both derive the id {id}; `entry_type` is the sixth \
             input to the derived identity and it is the only one of the six they differ in, so \
             a shared id would make the invalidation collide with the record instead of standing \
             beside it, and this check would assert the opposite of its rule",
            id = record.id,
        ));
    }
    if invalidation.idempotency_key != record.idempotency_key {
        return Err(format!(
            "the record submits under the idempotency key `{record_key}` and its invalidation \
             under `{invalidation_key}`; the check's rule is about an invalidation under the \
             same key as its target, and a pair under two keys shares no identity component to \
             be confused over, so every backend would pass",
            record_key = record.idempotency_key.as_str(),
            invalidation_key = invalidation.idempotency_key.as_str(),
        ));
    }

    Ok(DistinctIdentityFixtures {
        meter,
        record,
        invalidation,
    })
}
