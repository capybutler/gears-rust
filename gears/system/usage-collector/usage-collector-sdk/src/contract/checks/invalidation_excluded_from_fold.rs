//! The DESIGN §3.3 `invalidation-excluded-from-fold` check.
//!
//! See [`invalidation_excluded_from_fold`] for what it asserts. The module
//! holds **two ranges**: one carrying a withdrawn pair with a live entry
//! beside it, and one carrying nothing but a withdrawn pair. The fold, the
//! ledger read and the ungrouped empty-selection rule each read one of the
//! two; the grouped half reads both, because what it asserts over the second
//! means nothing without what it asserts over the first.

use std::collections::BTreeSet;
use std::str::FromStr;

use bigdecimal::BigDecimal;
use uuid::Uuid;

use crate::contract::fixtures::{
    CONTRACT_METER_TYPE_ID, CONTRACT_TENANT_ID, check_window_from, contract_query,
    fixture_invalidation, fixture_record, violation,
};
use crate::contract::{ContractViolation, HARNESS_FAULT, INVALIDATION_EXCLUDED_FROM_FOLD};
use crate::models::{
    AggregationBucket, AggregationDimension, AggregationFold, IdempotencyKey, MeterTypeId,
    UsageRecord,
};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::time_range::TimeRange;

/// The start of the live entry's covered period, and the inclusive lower
/// bound of the range this check folds and reads over.
///
/// The offset [`check_window_from`] tables for this check, for the reason
/// that accessor gives. It matters here because this check counts the rows
/// a range returns, so a stray entry from another check inside it would be
/// read as a fourth entry.
const FOLD_WINDOW_FROM: time::OffsetDateTime =
    check_window_from(INVALIDATION_EXCLUDED_FROM_FOLD, "main");

/// The exclusive upper bound of that range, an hour past the end of the
/// withdrawn pair's period so all three entries are selected by their end.
const FOLD_WINDOW_TO: time::OffsetDateTime =
    FOLD_WINDOW_FROM.saturating_add(time::Duration::hours(3));

/// The live entry's quantity, and the whole of the total the fold must
/// report.
///
/// Distinct from [`FOLD_WITHDRAWN_QUANTITY`] and small beside it, so the
/// three answers a backend can give are three different numbers:
/// `FOLD_LIVE_QUANTITY` alone when the withdrawn pair is excluded,
/// `FOLD_LIVE_QUANTITY + FOLD_WITHDRAWN_QUANTITY` when only the record is,
/// and `FOLD_LIVE_QUANTITY + 2 × FOLD_WITHDRAWN_QUANTITY` when neither is.
/// Stated as arithmetic over the two constants rather than as the literal
/// sums, so editing either quantity cannot leave the numbers here behind.
const FOLD_LIVE_QUANTITY: &str = "7.25";

/// The withdrawn record's quantity, and the quantity the invalidation that
/// withdraws it carries.
///
/// An invalidation **echoes** the quantity it withdraws rather than
/// negating it (`cpt-cf-usage-collector-adr-append-only-invalidation`),
/// which is exactly why leaving out only the record double-counts instead
/// of cancelling: the echoed term stays in the sum with nothing left to
/// pair it against.
const FOLD_WITHDRAWN_QUANTITY: &str = "1000";

/// The read limit this check dispatches: twice the three entries it
/// expects.
///
/// The margin is the assertion, for the reason
/// [`DEDUP_PAGE_LIMIT`](super::dedup_identity_over_window::DEDUP_PAGE_LIMIT)
/// gives. A limit set to the expected three would truncate a fourth row
/// away, and
/// the half of this check that catches a backend storing something extra
/// would pass.
const FOLD_PAGE_LIMIT: u64 = 6;

/// The inclusive lower bound of the second range this check folds over: the
/// one holding **nothing but a withdrawn pair**.
///
/// The second offset [`check_window_from`] tables for this check, under the
/// role `empty`. It is a window of its own rather than a corner of
/// [`FOLD_WINDOW_FROM`]'s because what is asserted over it is that nothing
/// survives the fold there, and the live entry over there is precisely
/// something that does. Taking a row of its own is also what puts it under
/// the table's collision guard.
const FOLD_EMPTY_WINDOW_FROM: time::OffsetDateTime =
    check_window_from(INVALIDATION_EXCLUDED_FROM_FOLD, "empty");

/// The exclusive upper bound of that range, an hour past the withdrawn
/// pair's period end so both of its entries are selected by that end.
const FOLD_EMPTY_WINDOW_TO: time::OffsetDateTime =
    FOLD_EMPTY_WINDOW_FROM.saturating_add(time::Duration::hours(2));

/// What each fold owes from the one ungrouped bucket of a range whose every
/// entry is a withdrawn pair, as a whole number or absent.
///
/// The split is DESIGN §3.3's plugin obligations verbatim: *"`SUM` and
/// `COUNT` are defined over an empty selection and report `0`; `MAX`, `MIN`
/// and `LATEST` are not and report absent."* All five are dispatched because
/// the obligation enumerates all five, and a backend can answer one family
/// correctly while getting the other backwards — which is the whole of what
/// this table is here to separate.
///
/// The `Some` side is only ever zero, and it is written as a number rather
/// than as a marker so the assertion compares a value the plugin returned
/// against a value this table states.
const EMPTY_SELECTION_ANSWERS: &[(AggregationFold, Option<u32>)] = &[
    (AggregationFold::Sum, Some(0)),
    (AggregationFold::Count, Some(0)),
    (AggregationFold::Max, None),
    (AggregationFold::Min, None),
    (AggregationFold::Latest, None),
];

/// The fold the **grouped** half of the empty-selection rule is dispatched
/// under: `SUM`, over each of the two ranges, rather than once per fold over
/// each.
///
/// A grouping is decided before any fold runs — the groups are keyed from
/// the surviving rows, and whether a group with no surviving row is keyed at
/// all is one decision a backend makes once for every fold. A dispatch under
/// a second fold would therefore measure that one decision twice. `SUM` is
/// the one chosen because it is where the ungrouped and the grouped answers
/// diverge most widely: over the same range, under the same fold, the
/// ungrouped bucket stays and carries `0` while the grouped one is never
/// formed.
const FOLD_EMPTY_GROUPED_FOLD: AggregationFold = AggregationFold::Sum;

/// The five entries this check submits, and the total the fold must report
/// over the three of them that share a range.
struct FoldFixtures {
    /// The surviving measurement, and the only entry the fold may count.
    live: UsageRecord,
    /// The measurement the invalidation withdraws.
    withdrawn: UsageRecord,
    /// The invalidation entry naming [`Self::withdrawn`].
    invalidation: UsageRecord,
    /// The measurement withdrawn over [`FOLD_EMPTY_WINDOW_FROM`]'s range,
    /// which holds it, [`Self::empty_invalidation`] and nothing else.
    empty_withdrawn: UsageRecord,
    /// The invalidation entry naming [`Self::empty_withdrawn`].
    empty_invalidation: UsageRecord,
    /// [`Self::live`]'s quantity on the aggregate surface's carrier.
    expected_total: BigDecimal,
}

/// `invalidation-excluded-from-fold` — *"A withdrawn pair folds to nothing
/// while both entries stay readable. Excluding only the record
/// double-counts the withdrawn measurement. An ungrouped range holding
/// nothing but a withdrawn pair reports `0` under `SUM` and `COUNT` and
/// absent under `MAX`, `MIN` and `LATEST`; a group nothing survives in
/// yields no bucket."*
///
/// **The first two sentences are one rule, and the second is the easy half
/// to lose.** They are two obligations rather than one conditional
/// (`cpt-cf-usage-collector-adr-append-only-invalidation`): the fold leaves
/// out the invalidation *and* the record it names, and the raw path leaves
/// out neither.
///
/// * Excluding the record but folding the invalidation double-counts the
///   withdrawn measurement, because the invalidation echoes the quantity it
///   withdraws rather than negating it. There is no term that cancels.
/// * Excluding both from the **raw** path instead would make the ledger
///   unauditable. The append-only correction model exists precisely so a
///   withdrawal is *visible* rather than a deletion, which is why
///   `get_usage_record` and `list_usage_records` return a withdrawn pair as
///   persisted and the exclusion lives in the fold alone.
///
/// The fixture over [`FOLD_WINDOW_FROM`]'s range is one live entry with a
/// distinctive quantity, plus a second entry and the invalidation that
/// withdraws it.
///
/// **The `SUM` over that range is asserted against the surviving entry's
/// value, never against zero.** Three totals are reachable over it and they
/// are three different numbers, so only a value tells them apart. An
/// assertion phrased as "the withdrawn pair contributes nothing" would be
/// satisfied by a backend that answers nothing at all — which is what the
/// noop plugin does, returning no buckets whatsoever — so the live entry is
/// what makes the comparison discriminate: the fold must report
/// [`FOLD_LIVE_QUANTITY`] exactly, not zero, and not that value plus one or
/// two [`FOLD_WITHDRAWN_QUANTITY`]s.
///
/// The raw half then asserts three entries come back over the same range
/// and that the invalidation among them still names its target. The two
/// halves fail independently: a backend that hides the withdrawn pair from
/// `list_usage_records` folds correctly and fails only the second, and one
/// that folds the invalidation in returns all three rows and does not fail
/// it.
///
/// # The third sentence: what an empty selection answers
///
/// The row's third sentence is asserted over a **second range**,
/// [`FOLD_EMPTY_WINDOW_FROM`]'s, which holds one withdrawn pair and no
/// surviving entry at all. DESIGN §3.3's plugin obligations are sharper
/// there than the row is, and they are the normative source:
///
/// > **An empty selection still answers.** `SUM` and `COUNT` are defined
/// > over an empty selection and report `0`; `MAX`, `MIN` and `LATEST` are
/// > not and report absent. This reaches the ungrouped bucket of a query
/// > matching no entry, and the ungrouped bucket of a range whose every
/// > entry is a withdrawn pair — the fold excludes the pair, which empties
/// > the selection rather than removing the bucket. A grouped query yields
/// > no bucket for a group nothing survives in.
///
/// **Ungrouped and grouped diverge on that one range, and that divergence
/// is the property.** Ungrouped, the bucket stays and what it carries
/// splits by fold. Grouped by `tenant_id`, no surviving row keys a group,
/// so there is no bucket to emit. A backend holding one answer for "nothing
/// survived" gets one of the two wrong whichever answer it holds — which is
/// why both halves are asserted here rather than either alone.
/// `tenant_id` is the grouping because DESIGN §3.1's `AggregationDimension`
/// row admits it and because the two dimensions that would be degenerate on
/// this path — `entry_type` and `invalidates` — are not in that set at all.
/// `AggregationBucket.key` then carries it as `Uuid::to_string()`, the
/// encoding §3.3 fixes, which is what lets the grouped control below compare
/// a returned key against a value rather than only counting buckets.
///
/// **The new half asserts zero, and the old half still must not.** They are
/// different assertions over different ranges. On the range above, the live
/// entry is what discriminates: a backend that folded the withdrawn pair in
/// reports a different number, and a backend computing no fold reports
/// nothing, so the comparison has to be against the survivor. On the empty
/// range there is no survivor to compare against and zero *is* the required
/// answer — what discriminates there is the **split** between the two fold
/// families, which a backend answering absent under every fold fails on
/// `SUM` and `COUNT` while a backend answering zero under every fold fails
/// on `MAX`, `MIN` and `LATEST`. Neither half alone catches both mistakes.
///
/// # What the subjects reach, and what they do not
///
/// Measured by neutering each assertion of this check in turn and running
/// the discrimination matrix against every subject `contract_mutants`
/// carries:
///
/// * The **ungrouped `SUM`** assertion and the **grouped assertion over the
///   empty range** are each isolated by one subject, and no other assertion
///   of this check can stand in for either.
/// * The **`MAX`, `MIN` and `LATEST`** assertions are reached as one clause
///   — one subject collapses all three at once, so neutering any single one
///   of the three changes nothing. DESIGN states them as one clause too, and
///   a subject per fold would be three subjects for one rule, which is not a
///   trade this matrix makes.
/// * The **ungrouped `COUNT`** assertion is reached by no subject, and the
///   gap is recorded in `contract_mutants`' header with what it would cost
///   to close.
/// * The **`SUM` over [`FOLD_WINDOW_FROM`]'s range** is reached by no
///   subject **any more**, and that is this widening's own doing. Its one
///   subject folds invalidations in, and over the empty range below that
///   same defect shows up under every fold — so the check still reports it
///   with this assertion neutered. The assertion stays because it is the
///   only one here that can catch an **under**-count: a backend that
///   excluded the live entry along with the pair answers `0` over the range
///   below, correctly, and answers the wrong thing only here. No subject
///   runs that way, because every plausible one excludes rows by a predicate
///   wide enough to reach several checks at once, which is a wide matrix row
///   rather than an isolating one — the reasoning `contract_mutants`' header
///   already applies to `invalidates` having no subject.
/// * The **two ledger assertions** are reached by no subject and were
///   reached by none before this widening either. The entry-reference one is
///   the `invalidates` gap that header records; the row count would need a
///   backend that withholds a withdrawn pair from `list_usage_records`, and
///   there is none.
/// * The **grouped control** over [`FOLD_WINDOW_FROM`]'s range is reached by
///   the same subject as the `SUM` above it and isolated by none, and it is
///   kept for what it establishes about the assertion beside it rather than
///   for a defect it catches. Without it, "no bucket over the empty range"
///   is answered correctly by a backend that answers no grouped bucket ever,
///   the noop plugin among them, and by any backend whose grouped path is a
///   stub. No subject models that, and none should: this matrix carries the
///   mistake a porter makes, not the backend that has not been written, and
///   the rest of the suite already reports a stub outright.
pub async fn invalidation_excluded_from_fold(
    plugin: &dyn UsageCollectorPluginV1,
) -> Vec<ContractViolation> {
    let fixtures = match fold_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own \
                     `{INVALIDATION_EXCLUDED_FROM_FOLD}` fixtures, so nothing was submitted. This \
                     is a fault in the suite, not in the plugin under test: {detail}"
                ),
            )];
        }
    };

    // A refusal here is reported and the check stops. Unlike the fixture
    // rows of `window-end-selection`, these five are one scenario: a fold
    // asserted over a pair that was never admitted, or over a live entry
    // that was not, would report a total that says nothing about the
    // exclusion rule. The last two are the sharper case of the same thing -
    // an empty-selection rule asserted over a range that is empty because
    // nothing was ever stored in it would pass against any backend at all.
    for (role, record) in [
        ("live", &fixtures.live),
        ("withdrawn", &fixtures.withdrawn),
        ("invalidation", &fixtures.invalidation),
        ("withdrawn-alone", &fixtures.empty_withdrawn),
        ("withdrawn-alone invalidation", &fixtures.empty_invalidation),
    ] {
        if let Err(err) = plugin.create_usage_record(record.clone()).await {
            return vec![violation(
                INVALIDATION_EXCLUDED_FROM_FOLD,
                format!(
                    "`create_usage_record` refused the {role} entry (record {id}), so neither the \
                     fold nor the ledger read could be asserted over a withdrawn pair: {err}",
                    id = record.id,
                ),
            )];
        }
    }

    let mut violations = fold_excludes_the_pair(plugin, &fixtures).await;
    violations.extend(ledger_keeps_the_pair(plugin, &fixtures).await);
    violations.extend(an_empty_selection_still_answers(plugin).await);
    violations.extend(a_grouping_buckets_only_what_survives(plugin, &fixtures).await);
    violations
}

/// The fold half: the withdrawn pair contributes nothing, and the live
/// entry's quantity is the whole of the total.
async fn fold_excludes_the_pair(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FoldFixtures,
) -> Vec<ContractViolation> {
    match fold_sum(plugin).await {
        Ok(Some(total)) if total == fixtures.expected_total => Vec::new(),
        Ok(observed) => vec![violation(
            INVALIDATION_EXCLUDED_FROM_FOLD,
            format!(
                "`SUM` over the range `[{from}, {to})` reported {observed}, and the live entry's \
                 own quantity `{expected}` is the whole of it. The range holds that entry, a \
                 second entry of `{withdrawn}`, and the invalidation withdrawing it - which \
                 carries the same `{withdrawn}` rather than its negation. An invalidation entry \
                 contributes nothing to any fold, and so does the record an accepted \
                 invalidation names: leaving out only the record leaves the echoed `{withdrawn}` \
                 in the total and double-counts the withdrawn measurement, and leaving out \
                 neither counts it twice over. The comparison is against the surviving entry \
                 rather than against zero on purpose - a fold over the withdrawn pair alone is \
                 empty whether a backend excluded it correctly or computed no fold at all.",
                from = FOLD_WINDOW_FROM,
                to = FOLD_WINDOW_TO,
                observed = observed
                    .as_ref()
                    .map_or_else(|| "no value at all".to_owned(), ToString::to_string),
                expected = FOLD_LIVE_QUANTITY,
                withdrawn = FOLD_WITHDRAWN_QUANTITY,
            ),
        )],
        Err(detail) => vec![violation(INVALIDATION_EXCLUDED_FROM_FOLD, detail)],
    }
}

/// The ledger half: all three entries stay readable on the raw path, and
/// the invalidation among them still names its target.
///
/// Both are what make the correction model auditable rather than a
/// deletion. A consumer folding entries it read here has to be able to see
/// the pair *and* to tell which record was withdrawn, and the target
/// reference is the only thing that says so — an entry known to be an
/// invalidation still has to name the record it withdrew before anything
/// can be left out.
async fn ledger_keeps_the_pair(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FoldFixtures,
) -> Vec<ContractViolation> {
    let items = match fold_page(plugin).await {
        Ok(items) => items,
        Err(detail) => return vec![violation(INVALIDATION_EXCLUDED_FROM_FOLD, detail)],
    };

    let mut violations = Vec::new();
    let returned: BTreeSet<Uuid> = items.iter().map(|item| item.id).collect();
    let expected = BTreeSet::from([
        fixtures.live.id,
        fixtures.withdrawn.id,
        fixtures.invalidation.id,
    ]);
    if items.len() != expected.len() || returned != expected {
        violations.push(violation(
            INVALIDATION_EXCLUDED_FROM_FOLD,
            format!(
                "the range `[{from}, {to})` holds a live entry ({live}), a withdrawn entry \
                 ({withdrawn}) and the invalidation withdrawing it ({invalidation}), and \
                 `list_usage_records` answered a row count of {count} over the ids {returned:?}. \
                 A withdrawn pair is returned as persisted on the ledger paths - both entries - \
                 because the append-only correction model exists so a withdrawal is visible \
                 rather than a deletion. Withholding one destroys the audit trail; the exclusion \
                 belongs to the fold alone.",
                from = FOLD_WINDOW_FROM,
                to = FOLD_WINDOW_TO,
                live = fixtures.live.id,
                withdrawn = fixtures.withdrawn.id,
                invalidation = fixtures.invalidation.id,
                count = items.len(),
            ),
        ));
    }

    // Only asserted when the entry came back at all: its absence is
    // already reported above, and reporting it twice would read as two
    // defects.
    if let Some(entry) = items
        .iter()
        .find(|item| item.id == fixtures.invalidation.id)
    {
        let target = entry.invalidation.as_ref().map(|inv| inv.target);
        if target != Some(fixtures.withdrawn.id) {
            violations.push(violation(
                INVALIDATION_EXCLUDED_FROM_FOLD,
                format!(
                    "the invalidation entry ({invalidation}) came back from \
                     `list_usage_records` naming {observed} as the entry it withdraws, and it \
                     withdraws {withdrawn}. The target reference is what makes the pair \
                     auditable: it is what marks this entry an invalidation at all, and it is \
                     the only thing that says which record was withdrawn, so a consumer folding \
                     entries it read here cannot leave the pair out without it.",
                    invalidation = fixtures.invalidation.id,
                    observed = target.map_or_else(
                        || "no entry at all".to_owned(),
                        |target| format!("`{target}`")
                    ),
                    withdrawn = fixtures.withdrawn.id,
                ),
            ));
        }
    }

    violations
}

/// The ungrouped half of the empty-selection rule: over a range holding
/// nothing but a withdrawn pair the bucket survives, and what it carries
/// splits by fold.
///
/// One dispatch per row of [`EMPTY_SELECTION_ANSWERS`], and one violation
/// per dispatch: a result carrying any number of buckets but one is a
/// different mistake from a bucket carrying the wrong answer, so the two
/// read as two messages rather than as two violations about one fold.
///
/// A fold that could not be dispatched is reported and the loop continues.
/// The five are independent questions of the plugin, so a refusal under one
/// says nothing about the other four, and stopping would hide them.
async fn an_empty_selection_still_answers(
    plugin: &dyn UsageCollectorPluginV1,
) -> Vec<ContractViolation> {
    let mut violations = Vec::new();
    for (fold, expected) in EMPTY_SELECTION_ANSWERS {
        let observed = match empty_range_ungrouped_value(plugin, *fold).await {
            Ok(observed) => observed,
            Err(detail) => {
                violations.push(violation(INVALIDATION_EXCLUDED_FROM_FOLD, detail));
                continue;
            }
        };
        let wanted = expected.map(BigDecimal::from);
        if observed != wanted {
            violations.push(violation(
                INVALIDATION_EXCLUDED_FROM_FOLD,
                format!(
                    "`{fold}` over the range `[{from}, {to})`, which holds one withdrawn pair and \
                     no surviving entry at all, answered {observed} from its one ungrouped bucket \
                     and DESIGN fixes it at {wanted}. The fold leaves out the invalidation and \
                     the record it names, which **empties** that bucket's selection rather than \
                     removing the bucket, and what an empty selection answers splits by fold: \
                     `SUM` and `COUNT` are defined over one and report `0`, `MAX`, `MIN` and \
                     `LATEST` are not and report absent. A backend answering absent under every \
                     fold fails the first two; one answering zero under every fold fails the \
                     other three.",
                    from = FOLD_EMPTY_WINDOW_FROM,
                    to = FOLD_EMPTY_WINDOW_TO,
                    observed = rendered(observed.as_ref()),
                    wanted = rendered(wanted.as_ref()),
                ),
            ));
        }
    }
    violations
}

/// The grouped half of the same rule: a group nothing survives in yields no
/// bucket, and a group something survives in yields one.
///
/// **Both dispatches are one assertion, and the second is a control the
/// first cannot do without.** "No bucket" is also what a backend with no
/// grouped path at all answers — the noop plugin among them — so asserting
/// it alone over the empty range would be the very trap the ungrouped `SUM`
/// above is written to avoid. The control runs the same fold, under the same
/// dimension, over [`FOLD_WINDOW_FROM`]'s range, where one entry does
/// survive: one bucket, keyed by the tenant, carrying the survivor's
/// quantity. A backend that answers that and then answers nothing over the
/// empty range has shown the emptiness is the grouping's doing.
///
/// Dispatched under [`FOLD_EMPTY_GROUPED_FOLD`], for the reason that
/// constant gives. `tenant_id` is the dimension: DESIGN §3.1's
/// `AggregationDimension` row admits it, every entry carries one, and the
/// one bucket a backend forming this group would emit is therefore keyed
/// rather than dropped for a missing value — so a bucket that comes back
/// over the empty range is the defect this half is about and not the
/// unrelated rule that a row carrying no value at a selected dimension is
/// excluded from the grouping. The key is compared against
/// `Uuid::to_string()`, the encoding DESIGN §3.3 fixes for this dimension.
async fn a_grouping_buckets_only_what_survives(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FoldFixtures,
) -> Vec<ContractViolation> {
    let mut violations = the_surviving_group_gets_its_bucket(plugin, fixtures).await;
    let buckets = match fold_buckets(
        plugin,
        FOLD_EMPTY_WINDOW_FROM,
        FOLD_EMPTY_WINDOW_TO,
        FOLD_EMPTY_GROUPED_FOLD,
        &[AggregationDimension::TenantId],
    )
    .await
    {
        Ok(buckets) => buckets,
        Err(detail) => {
            violations.push(violation(INVALIDATION_EXCLUDED_FROM_FOLD, detail));
            return violations;
        }
    };
    if buckets.is_empty() {
        return violations;
    }
    let keys: Vec<&[String]> = buckets.iter().map(|bucket| bucket.key.as_slice()).collect();
    violations.push(violation(
        INVALIDATION_EXCLUDED_FROM_FOLD,
        format!(
            "`{FOLD_EMPTY_GROUPED_FOLD}` over the range `[{FOLD_EMPTY_WINDOW_FROM}, \
             {FOLD_EMPTY_WINDOW_TO})` grouped by `tenant_id` answered buckets under the keys \
             {keys:?}, and a group nothing survives in yields none. That range holds one \
             withdrawn pair and nothing else; the fold leaves out both of its entries, so no \
             surviving row keys a group and there is no group to fold over. This is where a \
             grouped query and an ungrouped one diverge over the same range: the ungrouped \
             bucket stays and is emptied, this one is never formed. A backend holding one \
             answer for `nothing survived` gets one of the two wrong whichever answer it holds.",
        ),
    ));
    violations
}

/// The control: over the range that holds a survivor, the same grouped
/// dispatch answers one bucket, keyed by the tenant, carrying that
/// survivor's quantity.
///
/// It is the grouped reading of the rule the ungrouped half already asserts
/// — the withdrawn pair contributes to no fold, under a grouping as much as
/// without one — and the only assertion here that requires a backend to form
/// a grouped bucket at all.
async fn the_surviving_group_gets_its_bucket(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FoldFixtures,
) -> Vec<ContractViolation> {
    let buckets = match fold_buckets(
        plugin,
        FOLD_WINDOW_FROM,
        FOLD_WINDOW_TO,
        FOLD_EMPTY_GROUPED_FOLD,
        &[AggregationDimension::TenantId],
    )
    .await
    {
        Ok(buckets) => buckets,
        Err(detail) => return vec![violation(INVALIDATION_EXCLUDED_FROM_FOLD, detail)],
    };
    let wanted = vec![CONTRACT_TENANT_ID.to_string()];
    let matched = match buckets.as_slice() {
        [bucket] => bucket.key == wanted && bucket.value.as_ref() == Some(&fixtures.expected_total),
        _ => false,
    };
    if matched {
        return Vec::new();
    }
    vec![violation(
        INVALIDATION_EXCLUDED_FROM_FOLD,
        format!(
            "`{FOLD_EMPTY_GROUPED_FOLD}` over the range `[{FOLD_WINDOW_FROM}, {FOLD_WINDOW_TO})` \
             grouped by `tenant_id` answered {buckets:?}, and one bucket keyed {wanted:?} and \
             carrying the live entry's `{FOLD_LIVE_QUANTITY}` is what that range owes. Every \
             entry in it belongs to the one tenant, and one of the three survives the fold, so \
             the grouping forms exactly one group and folds the survivor alone in it - a backend \
             that counted the withdrawn pair reports a larger number in the same bucket. This \
             is the control the empty range's assertion rests on: `no bucket` there means the \
             grouping dropped an empty group only if the grouping emits a bucket when something \
             survives, and a backend serving no grouped query at all answers nothing to both."
        ),
    )]
}

/// A fold value as a message renders it, absence included.
///
/// One helper rather than a `map_or_else` at each site, because the
/// empty-selection message renders two of them in one sentence and they
/// have to read the same way.
fn rendered(value: Option<&BigDecimal>) -> String {
    value.map_or_else(|| "absent".to_owned(), |value| format!("`{value}`"))
}

/// The value the one bucket an ungrouped fold answers with carries, over the
/// range holding nothing but a withdrawn pair.
///
/// A result carrying any other number of buckets is reported rather than
/// picked from, and the detail says why one is owed: the fold empties that
/// bucket's selection rather than removing the bucket. `Err` carries a
/// ready-to-report detail.
async fn empty_range_ungrouped_value(
    plugin: &dyn UsageCollectorPluginV1,
    fold: AggregationFold,
) -> Result<Option<BigDecimal>, String> {
    let buckets = fold_buckets(
        plugin,
        FOLD_EMPTY_WINDOW_FROM,
        FOLD_EMPTY_WINDOW_TO,
        fold,
        &[],
    )
    .await?;
    let count = buckets.len();
    let mut found = buckets.into_iter();
    match (found.next(), found.next()) {
        (Some(bucket), None) => Ok(bucket.value),
        _ => Err(format!(
            "`query_aggregated_usage_records` was dispatched under `{fold}` with no grouping \
             dimension over the range `[{FOLD_EMPTY_WINDOW_FROM}, {FOLD_EMPTY_WINDOW_TO})`, and \
             it answered {count} buckets. The no-grouping case is one bucket carrying an empty \
             key, and that range holds one withdrawn pair and nothing else: the fold leaves \
             both of its entries out, which empties the bucket's selection rather than removing \
             the bucket. So one bucket is owed here too, and what it carries is what splits by \
             fold.",
        )),
    }
}

/// Every bucket one fold answers with over one of this check's two ranges,
/// under a caller-supplied grouping.
///
/// `Err` carries a ready-to-report detail.
async fn fold_buckets(
    plugin: &dyn UsageCollectorPluginV1,
    from: time::OffsetDateTime,
    to: time::OffsetDateTime,
    fold: AggregationFold,
    group_by: &[AggregationDimension],
) -> Result<Vec<AggregationBucket>, String> {
    let meter = MeterTypeId::new(CONTRACT_METER_TYPE_ID)
        .map_err(|err| format!("the check's own meter type id is invalid: {err}"))?;
    let range = TimeRange::new(from, to)
        .map_err(|err| format!("the check could not build its own read range: {err}"))?;
    let grouping = if group_by.is_empty() {
        "no grouping dimension"
    } else {
        "a grouping on `tenant_id`"
    };
    let result = plugin
        .query_aggregated_usage_records(
            meter,
            range,
            fold,
            &contract_query(FOLD_PAGE_LIMIT),
            &[],
            group_by,
        )
        .await
        .map_err(|err| {
            format!(
                "`query_aggregated_usage_records` failed under `{fold}` with {grouping} over the \
                 range `[{from}, {to})`, so what that fold answers there could not be decided: \
                 {err}"
            )
        })?;
    Ok(result.buckets)
}

/// The `SUM` one bucket carries over the range under test.
///
/// `group_by` is empty, which the aggregate surface fixes as the
/// no-grouping case: a single bucket with an empty key. A result carrying
/// any other number of buckets is reported rather than picked from, since
/// there would be no one total to compare. `Err` carries a ready-to-report
/// detail.
async fn fold_sum(plugin: &dyn UsageCollectorPluginV1) -> Result<Option<BigDecimal>, String> {
    let meter = MeterTypeId::new(CONTRACT_METER_TYPE_ID)
        .map_err(|err| format!("the check's own meter type id is invalid: {err}"))?;
    let range = TimeRange::new(FOLD_WINDOW_FROM, FOLD_WINDOW_TO)
        .map_err(|err| format!("the check could not build its own read range: {err}"))?;
    let result = plugin
        .query_aggregated_usage_records(
            meter,
            range,
            AggregationFold::Sum,
            &contract_query(FOLD_PAGE_LIMIT),
            &[],
            &[],
        )
        .await
        .map_err(|err| {
            format!(
                "`query_aggregated_usage_records` failed over the range holding the withdrawn \
                 pair, so the exclusion could not be decided: {err}"
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
             live entry's quantity against."
        )),
    }
}

/// Every entry the range under test comes back with on the raw path.
///
/// A `Vec` rather than a set, because the count is an assertion: a set
/// would collapse a duplicated row away.
async fn fold_page(plugin: &dyn UsageCollectorPluginV1) -> Result<Vec<UsageRecord>, String> {
    let meter = MeterTypeId::new(CONTRACT_METER_TYPE_ID)
        .map_err(|err| format!("the check's own meter type id is invalid: {err}"))?;
    let range = TimeRange::new(FOLD_WINDOW_FROM, FOLD_WINDOW_TO)
        .map_err(|err| format!("the check could not build its own read range: {err}"))?;
    let page = plugin
        .list_usage_records(meter, range, &contract_query(FOLD_PAGE_LIMIT), &[])
        .await
        .map_err(|err| {
            format!(
                "`list_usage_records` failed over the range holding the withdrawn pair, so \
                 whether both its entries stay readable could not be decided: {err}"
            )
        })?;
    Ok(page.items)
}

/// Builds the live entry, the entry withdrawn from under it, the
/// invalidation that withdraws it, and the second withdrawn pair that has a
/// range to itself.
///
/// Three guards keep the check from passing by construction, and all three
/// are the suite's own facts rather than the plugin's:
///
/// * The two quantities differ. If they did not, a fold that counted the
///   withdrawn pair would report the same total as one that excluded it.
/// * The withdrawn quantity is not zero. A zero one would make the
///   empty-selection half pass against a backend that folded the pair in:
///   the `SUM` it owes over that range is `0`, and so is the `SUM` of two
///   zero-quantity entries.
/// * The five entries derive five distinct ids. The ledger half counts rows
///   under three of them, and two fixtures sharing an id would collapse
///   into an idempotent replay rather than into two entries.
fn fold_fixtures() -> Result<FoldFixtures, String> {
    let live_value = UsageQuantity::parse(FOLD_LIVE_QUANTITY).map_err(|err| {
        format!("the check's own live quantity `{FOLD_LIVE_QUANTITY}` does not parse: {err}")
    })?;
    let withdrawn_value = UsageQuantity::parse(FOLD_WITHDRAWN_QUANTITY).map_err(|err| {
        format!(
            "the check's own withdrawn quantity `{FOLD_WITHDRAWN_QUANTITY}` does not parse: {err}"
        )
    })?;
    if live_value == withdrawn_value {
        return Err(format!(
            "the live and the withdrawn quantity are both `{FOLD_LIVE_QUANTITY}`, so a fold \
             counting the withdrawn pair would report the same total as one excluding it and this \
             check would pass by construction"
        ));
    }
    if withdrawn_value.as_decimal().is_zero() {
        return Err(format!(
            "the withdrawn quantity is `{FOLD_WITHDRAWN_QUANTITY}`, and a `SUM` over two entries \
             carrying it is then the same `0` the empty-selection half requires of a fold that \
             leaves both of them out, so that half would pass by construction against a backend \
             folding the withdrawn pair in"
        ));
    }
    let expected_total = BigDecimal::from_str(FOLD_LIVE_QUANTITY).map_err(|err| {
        format!(
            "the check's own live quantity `{FOLD_LIVE_QUANTITY}` does not widen to the aggregate \
             carrier: {err}"
        )
    })?;

    let live_end = FOLD_WINDOW_FROM.saturating_add(time::Duration::hours(1));
    let pair_end = FOLD_WINDOW_FROM.saturating_add(time::Duration::hours(2));

    let live = fixture_record(&fold_key("live")?, live_value, FOLD_WINDOW_FROM, live_end)?;
    let withdrawn = fixture_record(&fold_key("withdrawn")?, withdrawn_value, live_end, pair_end)?;
    // The invalidation carries its target's covered period, which is the
    // shape the gateway admits and the shape the SPI reasons about: both
    // entries carry one covered period, so no `time_range` selects one of
    // the pair without the other and no placement of the invalidation
    // changes a result. It carries the target's quantity too, echoed
    // rather than negated, and the target's idempotency key, which is why
    // the builder takes the entry rather than its id.
    let invalidation = fixture_invalidation(&withdrawn)?;

    // The second pair, over a range of its own with nothing else in it. It
    // carries the same quantity as the first: whatever a backend that folds
    // a withdrawn pair in would report over here, it is not the `0` DESIGN
    // requires, and the guard above is what holds that.
    let empty_pair_end = FOLD_EMPTY_WINDOW_FROM.saturating_add(time::Duration::hours(1));
    let empty_withdrawn = fixture_record(
        &fold_key("withdrawn-alone")?,
        withdrawn_value,
        FOLD_EMPTY_WINDOW_FROM,
        empty_pair_end,
    )?;
    let empty_invalidation = fixture_invalidation(&empty_withdrawn)?;

    let ids = BTreeSet::from([
        live.id,
        withdrawn.id,
        invalidation.id,
        empty_withdrawn.id,
        empty_invalidation.id,
    ]);
    if ids.len() != 5 {
        return Err(format!(
            "the live entry ({live}), the withdrawn entry ({withdrawn}), the invalidation \
             ({invalidation}), the entry withdrawn over the empty range ({empty_withdrawn}) and \
             the invalidation withdrawing that one ({empty_invalidation}) do not derive five \
             distinct ids, so two of them would collapse into an idempotent replay and the row \
             count this check asserts would be meaningless",
            live = live.id,
            withdrawn = withdrawn.id,
            invalidation = invalidation.id,
            empty_withdrawn = empty_withdrawn.id,
            empty_invalidation = empty_invalidation.id,
        ));
    }

    Ok(FoldFixtures {
        live,
        withdrawn,
        invalidation,
        empty_withdrawn,
        empty_invalidation,
        expected_total,
    })
}

/// The idempotency key one role of this check's fixture submits under.
///
/// Keyed on the role name for the reason
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture)
/// spells out: in a ledger with no delete path, an edited fixture must take
/// a fresh identity rather than inherit an accepted entry's.
fn fold_key(role: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{INVALIDATION_EXCLUDED_FROM_FOLD}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}
