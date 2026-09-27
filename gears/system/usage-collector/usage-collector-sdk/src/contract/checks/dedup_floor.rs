//! The DESIGN §3.3 `dedup-floor` check.
//!
//! See [`dedup_floor`] for what it asserts; the module holds the three
//! identities it seeds, the retry and the two divergent submissions they
//! resolve, and the read-back that counts what the ledger actually holds.

use bigdecimal::BigDecimal;
use uuid::Uuid;

use crate::contract::fixtures::{
    CONTRACT_ACCEPTED_AT, CONTRACT_TENANT_ID, FIXTURE_EPOCH, check_meter, contract_query,
    fixture_record_on, violation,
};
use crate::contract::{ContractViolation, DEDUP_FLOOR, HARNESS_FAULT};
use crate::error::UsageCollectorPluginError;
use crate::models::{AggregationFold, IdempotencyKey, MeterTypeId, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::time_range::TimeRange;

/// The start of the covered period every entry of this check carries, and
/// the inclusive lower bound of the range it reads them back over.
///
/// Two hundred and forty days past [`FIXTURE_EPOCH`], for the reason
/// [`WINDOW_SELECTION_FROM`](super::window_end_selection::WINDOW_SELECTION_FROM)
/// gives, and clear of the day-0, 30, 60, 90, 120, 150, 180, 210, 270 and
/// 300 offsets the other checks take. It matters here because this check counts
/// the rows a range returns against the identities it seeded, so a stray
/// entry inside it would be read as a row no identity of this check accounts
/// for — which is exactly the shape of the failure the check exists to
/// report.
///
/// The offset is the second of two separations rather than the only one:
/// this check also reads over a meter of its own (see
/// [`dedup_floor_fixtures`]), and a range and a meter that no other check
/// writes to are independent reasons why nothing else can land in the page
/// this check counts.
const DEDUP_FLOOR_WINDOW_FROM: time::OffsetDateTime =
    FIXTURE_EPOCH.saturating_add(time::Duration::days(240));

/// The end of that covered period.
///
/// **Every entry of this check carries one period**, and that is deliberate.
/// The three identities are separated by their idempotency keys alone, so no
/// key of this check is ever submitted over a second period — which is what
/// keeps `contract_mutants`'s `Defect::DedupIgnoresThePeriod` out of this
/// check's matrix row. That subject keys an index on the derived identity
/// with the two period bounds struck out, and an index blind to the bounds
/// answers exactly as the full identity does for a key that only ever sees
/// one period. See [`dedup_floor_fixtures`] for the rest of that argument.
const DEDUP_FLOOR_WINDOW_END: time::OffsetDateTime =
    DEDUP_FLOOR_WINDOW_FROM.saturating_add(time::Duration::hours(1));

/// The exclusive upper bound of the range this check reads over, an hour past
/// [`DEDUP_FLOOR_WINDOW_END`].
///
/// The range holds **both** covered-period bounds, deliberately, for the
/// reason `server-field-round-trip`'s own upper bound gives: this check
/// asserts nothing about period selection, and a range beginning at
/// [`DEDUP_FLOOR_WINDOW_END`] would make both of its read halves unanswerable
/// for a backend selecting on `window_start`, coupling this check to
/// `window-end-selection`'s rule for no reason of its own.
const DEDUP_FLOOR_WINDOW_TO: time::OffsetDateTime =
    DEDUP_FLOOR_WINDOW_END.saturating_add(time::Duration::hours(1));

/// The quantity each identity's accepted entry carries.
///
/// Exactly representable in a binary float, which keeps the one subject that
/// rewrites a quantity on the way in — `contract_mutants`'s
/// `Defect::QuantityThroughFloat` — from changing anything this check can
/// see. Every assertion here compares what a plugin answered against the
/// fixture this module built, so a quantity that moved through that rewrite
/// would come back unequal to its fixture and be reported by this check for
/// a mistake that is `quantity-round-trip`'s rule rather than this one's.
const DEDUP_FLOOR_QUANTITY: &str = "5.5";

/// The quantity the two **divergent** submissions carry instead.
///
/// The divergence has to be in a caller-supplied field that is not one of the
/// six identity inputs, so that the submission derives the accepted entry's
/// own `id` and still fails [`UsageRecord::caller_supplied_eq`]. The quantity
/// is such a field: DESIGN §3.1's "Dedup identity" gives the identity as
/// `(tenant_id, gts_type_id, idempotency_key, window_start, window_end,
/// entry_type)` and "Identity derivation" derives `id` from *"all six
/// components in order"* and nothing else, while "Collision resolution"
/// compares *"exact equality of the caller-supplied fields"*, the quantity
/// among them.
///
/// Exactly representable in a binary float for the reason
/// [`DEDUP_FLOOR_QUANTITY`] gives, with a second reason of its own: a rewrite
/// that landed both quantities on one value would make a divergent submission
/// an ordinary retry, and the two conflict assertions would then hold a
/// conforming backend to an outcome it must not give. That the two are
/// distinct as the plugin is handed them is
/// [`DedupFloorFixtures::guards`]' business rather than this comment's.
const DEDUP_FLOOR_DIVERGENT_QUANTITY: &str = "7.25";

/// The read limit this check dispatches, on the ledger page and in the fold's
/// query alike: twice the three identities it expects.
///
/// The margin is the assertion, for the reason
/// [`DEDUP_PAGE_LIMIT`](super::dedup_identity_over_window::DEDUP_PAGE_LIMIT)
/// gives, and it carries more weight here than anywhere else in the suite:
/// **the row count is the whole of this check's first half**. A limit set to
/// the expected three would truncate a fourth row away, and the assertion
/// that one identity reads at most once would pass against a backend holding
/// two rows for one of them.
const DEDUP_FLOOR_PAGE_LIMIT: u64 = 6;

/// The three identities this check seeds, the submissions that resolve
/// against them, and the meter they are read back over.
struct DedupFloorFixtures {
    /// The meter every entry is written to and the only meter this check
    /// reads.
    meter: MeterTypeId,
    /// The range both read halves dispatch, built once.
    ///
    /// Built here rather than at each read so that a range the suite cannot
    /// construct is reported as [`HARNESS_FAULT`] along with every other
    /// fixture fault, rather than twice against the plugin under two
    /// different messages about a failure that is not the plugin's.
    range: TimeRange,
    /// The identity resolved across **separate calls**: submitted, retried
    /// verbatim, then submitted again with different content.
    converged: UsageRecord,
    /// The divergent submission against [`Self::converged`]: the same six
    /// identity inputs, a different quantity.
    divergent: UsageRecord,
    /// The identity resolved **inside one batch call** by an identical later
    /// entry. Submitted twice in one `create_usage_records`.
    batched_identical: UsageRecord,
    /// The earlier of the two entries of the identity resolved inside one
    /// batch call by a **divergent** later entry.
    ///
    /// A key of its own rather than [`Self::batched_identical`]'s: the two
    /// batch cases are separate identities so neither can contaminate the
    /// other, which is also what makes the expected row count three rather
    /// than two.
    batched_divergent: UsageRecord,
    /// The later of that pair: the same six identity inputs as
    /// [`Self::batched_divergent`], a different quantity.
    batched_divergent_later: UsageRecord,
}

/// One scenario's result: the identity it left on the ledger, and what it
/// reported.
///
/// The two are separate because a scenario can seed its identity **and**
/// report a violation — a retry refused over a record the backend did store,
/// say. The read half below then counts rows for the identities that were
/// actually seeded, so a refused seed is reported once, by the scenario that
/// met it, rather than a second time as a row the ledger does not hold.
struct Scenario {
    /// The id every entry of this scenario derives, when the backend admitted
    /// the first of them. `None` when it refused.
    seeded: Option<(&'static str, Uuid)>,
    /// What the scenario observed, and what the check required instead.
    violations: Vec<ContractViolation>,
}

/// `dedup-floor` — *"One identity reads, folds, and counts at most once.
/// Against a converged entry a retry returns the stored entry and a divergent
/// submission is `IdempotencyConflict`. Two same-identity entries in one batch
/// call resolve the later against the earlier on the same terms: absorbed when
/// identical, `IdempotencyConflict` when divergent."*
/// (DESIGN §3.3, "Plugin contract tests", line 1318.)
///
/// **The three sentences are three obligations, not one stated three ways.**
///
/// The first is the floor this check is named for, and it is a statement
/// about the **store** rather than about any answer a caller reads. DESIGN
/// §3.1's "Dedup identity" row states it: *"One identity yields at most one
/// entry on every read path, fold, reconciliation figure, and materialised
/// aggregate."* A backend can answer every individual call below correctly
/// and still have written the row twice — the outcome a caller reads is
/// decided by a lookup, and a ledger whose write does not repeat that lookup
/// inserts the duplicate anyway. Nothing about the outcomes would notice, so
/// this check reads the ledger back: a `list_usage_records` over its range
/// holds one row per identity, and a `COUNT` fold over the same range reports
/// the same number. That is why the check is named `dedup-floor` rather than
/// `dedup-outcomes`.
///
/// The second and third are the outcomes themselves, and DESIGN §3.1's
/// "Collision resolution" row is the authority on both: *"A collision on the
/// full identity, `entry_type` included, resolves by exact equality of the
/// caller-supplied fields. All equal, the entry is absorbed. Any field
/// differing, metadata alone included, is `IdempotencyConflict` once the
/// identity has converged … Two entries in one request sharing the full
/// identity resolve the same way at the gateway, the later against the
/// earlier."* The SPI repeats the batch half in
/// [`create_usage_records`](UsageCollectorPluginV1::create_usage_records):
/// *"Two same-identity entries in one call resolve later against earlier: the
/// later is absorbed when its caller-supplied fields equal the earlier
/// accepted entry's, and conflicts otherwise."*
///
/// Three identities are seeded, one per scenario, each under its own
/// idempotency key so that no outcome of one can be explained by another:
///
/// 1. **Separate calls against a converged entry.** The record is accepted, a
///    verbatim retry answers with the stored entry, and a submission carrying
///    the same six identity inputs under a different quantity is
///    `IdempotencyConflict` whose `existing` is the stored entry.
/// 2. **One batch call, identical later entry.** Two copies of one entry in
///    a single `create_usage_records`: the first accepted, the second
///    absorbed against it.
/// 3. **One batch call, divergent later entry.** The first accepted, the
///    second `IdempotencyConflict` naming the first.
/// 4. **The floor.** After all of it, the range holds exactly one row per
///    seeded identity and the `COUNT` over it reports exactly that many.
///
/// **The overlap with the rest of the suite is worth stating, because it is
/// what the matrix row reports.** Asserting a property of the store means
/// submitting an identity twice and counting rows, and two other checks
/// already do some of that:
/// [`dedup_identity_over_window`](super::dedup_identity_over_window()) counts
/// the rows carrying **one** id after a verbatim retry, and
/// [`record_and_invalidation_distinct_identity`](super::record_and_invalidation_distinct_identity())
/// counts a record and its withdrawal after a retry of the record. So the
/// subject built for this rule — `contract_mutants`'s
/// `Defect::LedgerHasNoUniqueConstraint` — fails all three, and that row
/// establishes that the three together notice such a backend rather than
/// which of them noticed.
///
/// What is this check's own is the **fold** half and the two identities a
/// batch call resolved: neither of the other two reads a fold at all, and
/// neither submits one identity twice inside a single call. The in-batch half
/// is also the only assertion here with a subject of its own —
/// `Defect::BatchResolvesAgainstThePreCallLedger`, which decides every row of
/// a batch against the ledger as it stood before the call and so is DESIGN
/// §3.3's third identity site with nothing enforcing it. That subject's row
/// names this check and `at-most-one-invalidation`, which sends a
/// same-identity pair of its own in one batch.
///
/// Which of these nine assertions any subject reaches was measured by
/// neutering each in turn, and the answer is recorded on the two defects
/// rather than here: three of the nine report against a subject today, and
/// the six that do not are accounted for one by one.
pub async fn dedup_floor(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    let fixtures = match dedup_floor_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own `{DEDUP_FLOOR}` fixtures, so \
                     nothing was submitted. This is a fault in the suite, not in the plugin under \
                     test: {detail}"
                ),
            )];
        }
    };

    let mut violations = Vec::new();
    let mut seeded: Vec<(&'static str, Uuid)> = Vec::new();
    for scenario in [
        the_converged_identity_resolves_both_ways(plugin, &fixtures).await,
        one_batch_call_absorbs_an_identical_later_entry(plugin, &fixtures).await,
        one_batch_call_conflicts_a_divergent_later_entry(plugin, &fixtures).await,
    ] {
        violations.extend(scenario.violations);
        if let Some(identity) = scenario.seeded {
            seeded.push(identity);
        }
    }

    violations.extend(the_ledger_holds_one_row_per_identity(plugin, &fixtures, &seeded).await);
    violations.extend(the_fold_counts_one_per_identity(plugin, &fixtures, &seeded).await);
    violations
}

/// Sentence two: against a converged entry, a verbatim retry is absorbed and a
/// divergent submission conflicts.
///
/// Both outcomes are compared against the **stored** entry rather than against
/// the submission in hand, which is the whole of what an absorb and a
/// conflict are for: each hands the caller the entry that won the identity.
async fn the_converged_identity_resolves_both_ways(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupFloorFixtures,
) -> Scenario {
    if let Err(err) = plugin.create_usage_record(fixtures.converged.clone()).await {
        return Scenario {
            seeded: None,
            violations: vec![violation(
                DEDUP_FLOOR,
                format!(
                    "`create_usage_record` refused the seed of this check's separate-call \
                     identity (record {id}, idempotency key `{key}`, period `{start}` to \
                     `{end}`): {err}. Nothing else this check submits shares its six identity \
                     inputs, so a conforming backend accepts it - or absorbs it, on a repeated \
                     run of this suite against a store that kept the first run's entries. A \
                     refusal is neither, and a retry and a divergent submission are both \
                     statements about a converged entry, of which there now is none.",
                    id = fixtures.converged.id,
                    key = fixtures.converged.idempotency_key.as_str(),
                    start = fixtures.converged.window_start,
                    end = fixtures.converged.window_end,
                ),
            )],
        };
    }

    let mut violations = Vec::new();
    match plugin.create_usage_record(fixtures.converged.clone()).await {
        Ok(stored) if is_the_stored(&stored, &fixtures.converged) => {}
        outcome => violations.push(violation(
            DEDUP_FLOOR,
            format!(
                "record {id} was re-delivered verbatim against a converged identity and \
                 `create_usage_record` answered {outcome:?}. A collision resolves by exact \
                 equality of the caller-supplied fields, and all of them are equal here, so the \
                 entry is absorbed and the stored entry comes back - under its own id and \
                 carrying the fields the caller supplied on it.",
                id = fixtures.converged.id,
            ),
        )),
    }

    match plugin.create_usage_record(fixtures.divergent.clone()).await {
        Err(UsageCollectorPluginError::IdempotencyConflict { ref existing, .. })
            if is_the_stored(existing, &fixtures.converged) => {}
        outcome => violations.push(violation(
            DEDUP_FLOOR,
            format!(
                "a submission carrying record {id}'s six identity inputs under the quantity \
                 `{sent}`, where the stored entry carries `{held}`, answered {outcome:?}. Any \
                 caller-supplied field differing is `IdempotencyConflict` once the identity has \
                 converged, and the `existing` it carries is the stored entry - which is how a \
                 caller learns what its key is already bound to. Absorbing this submission \
                 instead is the silent drop the gear does not admit: the divergent content would \
                 never be stored and the caller would be told it was.",
                id = fixtures.converged.id,
                sent = fixtures.divergent.quantity,
                held = fixtures.converged.quantity,
            ),
        )),
    }

    Scenario {
        seeded: Some(("the separate-call identity", fixtures.converged.id)),
        violations,
    }
}

/// Sentence three, first half: two identical entries of one identity in a
/// single `create_usage_records` call.
///
/// The later is **absorbed**, not refused, and the assertion is the shape of
/// the outcome alone: nothing here can tell which of the two copies the
/// backend answered with, because they carry the same content by
/// construction.
///
/// That bounds it, and the bound was measured rather than guessed. An absorb
/// and a second *acceptance* are indistinguishable on an identical pair —
/// both answer `Ok` carrying an entry equal in every caller-supplied field —
/// so `contract_mutants`'s `Defect::BatchResolvesAgainstThePreCallLedger`,
/// which has no in-batch dedup map at all, passes this assertion and is
/// caught by the divergent pair instead. What this assertion does catch is a
/// backend that **refuses** the later entry, reporting a conflict between a
/// caller and itself over content the two agree on; no subject in the module
/// does that today.
async fn one_batch_call_absorbs_an_identical_later_entry(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupFloorFixtures,
) -> Scenario {
    let entry = &fixtures.batched_identical;
    let outcomes = match dedup_floor_batch(plugin, entry.clone(), entry.clone(), "identical").await
    {
        Ok(outcomes) => outcomes,
        Err(detail) => {
            return Scenario {
                seeded: None,
                violations: vec![violation(DEDUP_FLOOR, detail)],
            };
        }
    };
    let (earlier, later) = outcomes;

    let mut violations = Vec::new();
    let accepted = matches!(&earlier, Ok(stored) if is_the_stored(stored, entry));
    if !accepted {
        violations.push(violation(
            DEDUP_FLOOR,
            format!(
                "the earlier of two identical entries of record {id} in one `create_usage_records` \
                 call answered {earlier:?}. It is decided against the ledger rather than against \
                 the entry behind it in the same call - a later entry resolves against the \
                 earlier, never the earlier against the later - so it is accepted, or absorbed \
                 on a repeated run of this suite, and either way answers with the stored entry.",
                id = entry.id,
            ),
        ));
    }
    if !matches!(&later, Ok(stored) if is_the_stored(stored, entry)) {
        violations.push(violation(
            DEDUP_FLOOR,
            format!(
                "the later of two identical entries of record {id} in one `create_usage_records` \
                 call answered {later:?}. Two same-identity entries in one call resolve the later \
                 against the earlier on the terms a collision across calls resolves on, so a \
                 later entry equal in every caller-supplied field is absorbed and answers with \
                 the accepted entry. Refusing it reports a conflict between a caller and itself, \
                 over content the two agree on.",
                id = entry.id,
            ),
        ));
    }

    Scenario {
        seeded: accepted.then_some(("the identical in-batch identity", entry.id)),
        violations,
    }
}

/// Sentence three, second half: two entries of one identity in a single
/// `create_usage_records` call, the later carrying different content.
async fn one_batch_call_conflicts_a_divergent_later_entry(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupFloorFixtures,
) -> Scenario {
    let earlier_entry = &fixtures.batched_divergent;
    let later_entry = &fixtures.batched_divergent_later;
    let outcomes = match dedup_floor_batch(
        plugin,
        earlier_entry.clone(),
        later_entry.clone(),
        "divergent",
    )
    .await
    {
        Ok(outcomes) => outcomes,
        Err(detail) => {
            return Scenario {
                seeded: None,
                violations: vec![violation(DEDUP_FLOOR, detail)],
            };
        }
    };
    let (earlier, later) = outcomes;

    let mut violations = Vec::new();
    let accepted = matches!(&earlier, Ok(stored) if is_the_stored(stored, earlier_entry));
    if !accepted {
        violations.push(violation(
            DEDUP_FLOOR,
            format!(
                "the earlier of two same-identity entries of record {id} in one \
                 `create_usage_records` call answered {earlier:?}. It is decided against the \
                 ledger rather than against the entry behind it in the same call, whatever that \
                 entry carries - a later entry resolves against the earlier, never the earlier \
                 against the later - so it is accepted, or absorbed on a repeated run of this \
                 suite, and either way answers with the stored entry.",
                id = earlier_entry.id,
            ),
        ));
    }
    if !matches!(
        &later,
        Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. })
            if is_the_stored(existing, earlier_entry)
    ) {
        violations.push(violation(
            DEDUP_FLOOR,
            format!(
                "the later of two same-identity entries of record {id} in one \
                 `create_usage_records` call, carrying the quantity `{sent}` where the earlier \
                 carries `{held}`, answered {later:?}. A later entry differing in any \
                 caller-supplied field is `IdempotencyConflict` whose `existing` is the earlier, \
                 accepted entry - the same resolution a divergent submission gets across calls, \
                 decided inside the call because the two arrived together. A backend with no \
                 in-batch dedup map accepts both instead, and the call reports two acceptances \
                 for one identity.",
                id = earlier_entry.id,
                sent = later_entry.quantity,
                held = earlier_entry.quantity,
            ),
        ));
    }

    Scenario {
        seeded: accepted.then_some(("the divergent in-batch identity", earlier_entry.id)),
        violations,
    }
}

/// Sentence one, on the ledger path: the range holds exactly one row per
/// seeded identity, and no row besides.
///
/// **This is the assertion no outcome above can make.** Every call this check
/// dispatched could be answered correctly by a backend that writes a second
/// row for each of them: the answer is decided by a lookup, and a ledger whose
/// write does not repeat that lookup - no unique constraint on the dedup
/// identity, no conflict target - stores the duplicate anyway. Such a backend
/// absorbs a retry, reports a conflict against the right entry, resolves a
/// batch pair the right way round, and holds six rows for three identities.
async fn the_ledger_holds_one_row_per_identity(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupFloorFixtures,
    seeded: &[(&'static str, Uuid)],
) -> Vec<ContractViolation> {
    let items = match dedup_floor_page(plugin, fixtures).await {
        Ok(items) => items,
        Err(detail) => return vec![violation(DEDUP_FLOOR, detail)],
    };

    let observed: Vec<String> = seeded
        .iter()
        .map(|(role, id)| {
            let rows = items.iter().filter(|item| item.id == *id).count();
            format!("{role} ({id}) on {rows} row(s)")
        })
        .collect();
    let each_once = seeded
        .iter()
        .all(|(_, id)| items.iter().filter(|item| item.id == *id).count() == 1);
    if each_once && items.len() == seeded.len() {
        return Vec::new();
    }

    vec![violation(
        DEDUP_FLOOR,
        format!(
            "this check seeded {identities} identities over the range `[{from}, {to})`, \
             re-delivered each of them, and every submission was resolved by the plugin's own \
             answer; `list_usage_records` then came back with {rows} row(s): {observed:?}. One \
             identity reads at most once. A backend can answer every submission above correctly \
             and still hold two rows for an identity, because the outcome a caller reads is \
             decided by a lookup and a write that does not repeat that lookup inserts the \
             duplicate anyway; that is what this assertion looks for, and nothing else in this \
             check would notice it.",
            from = DEDUP_FLOOR_WINDOW_FROM,
            to = DEDUP_FLOOR_WINDOW_TO,
            identities = seeded.len(),
            rows = items.len(),
        ),
    )]
}

/// Sentence one, on the fold: a `COUNT` over the same range reports one per
/// seeded identity.
///
/// A second read path rather than a restatement of the first. DESIGN §3.1
/// puts the obligation on *"every read path, fold, reconciliation figure, and
/// materialised aggregate"*, and a fold is not served from the ledger page: a
/// backend whose `COUNT` runs against a materialised aggregate, or against a
/// continuous aggregate refreshed on insert, can hold one row on the ledger
/// and count it twice.
///
/// `COUNT` rather than `SUM` because it reads no quantity: the two divergent
/// submissions differ from their targets exactly in the quantity, so a `SUM`
/// here would report a duplicate and a wrongly-resolved collision as the same
/// number and could not tell them apart.
async fn the_fold_counts_one_per_identity(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupFloorFixtures,
    seeded: &[(&'static str, Uuid)],
) -> Vec<ContractViolation> {
    let result = match plugin
        .query_aggregated_usage_records(
            fixtures.meter.clone(),
            fixtures.range,
            AggregationFold::Count,
            &contract_query(DEDUP_FLOOR_PAGE_LIMIT),
            &[],
            &[],
        )
        .await
    {
        Ok(result) => result,
        Err(err) => {
            return vec![violation(
                DEDUP_FLOOR,
                format!(
                    "the `COUNT` over the range holding this check's entries failed, so how many \
                     times one identity counts could not be decided: {err}"
                ),
            )];
        }
    };

    let expected = BigDecimal::from(u64::try_from(seeded.len()).unwrap_or(u64::MAX));
    let counted: Vec<Option<&BigDecimal>> = result
        .buckets
        .iter()
        .map(|bucket| bucket.value.as_ref())
        .collect();
    if counted.len() == 1 && counted.first().copied().flatten() == Some(&expected) {
        return Vec::new();
    }

    vec![violation(
        DEDUP_FLOOR,
        format!(
            "an ungrouped `COUNT` over the range `[{from}, {to})` reported {counted:?}, and this \
             check seeded {identities} identities there, none of them withdrawn. One identity \
             counts at most once. The ledger page is a different read path from the fold, and a \
             backend answering one of them correctly says nothing about the other: a duplicate \
             row the page shows once - because a caller looks the identity up by id - still adds \
             its own term here.",
            from = DEDUP_FLOOR_WINDOW_FROM,
            to = DEDUP_FLOOR_WINDOW_TO,
            identities = seeded.len(),
        ),
    )]
}

/// Whether `stored` is `expected` as the plugin would answer with it.
///
/// The comparison is the id **together with** every caller-supplied field
/// ([`UsageRecord::caller_supplied_eq`]) rather than either alone, and both
/// halves earn their place. The id alone would admit an answer carrying the
/// right identity and the wrong content — which is exactly the shape this
/// check's two divergent submissions have, so a backend answering an absorb
/// or a conflict with one of *them* rather than with the entry it stored
/// would go unnoticed. The fields alone would admit an answer carrying the
/// right content under some other entry's id, which is what three separate
/// identities on one meter exist to keep apart.
///
/// Whole-record equality is deliberately not used. It also reads `accepted_at`
/// and `origin`, which are server-assigned and no part of this check's rule -
/// a backend stamping its own instant would be reported here, under a message
/// about dedup, while the check built for that rule reported it too.
fn is_the_stored(stored: &UsageRecord, expected: &UsageRecord) -> bool {
    stored.id == expected.id && stored.caller_supplied_eq(expected)
}

/// Sends one two-entry batch and splits the outcomes.
///
/// `Err` carries a ready-to-report detail. Two shapes land there and neither
/// is an outcome this check can read: the whole call failing, which is
/// distinct from a per-entry refusal and is reported as such, and an answer
/// whose length is not the batch's — the SPI aligns outcomes with the input
/// order, so a shorter or longer vector leaves nothing to align against.
async fn dedup_floor_batch(
    plugin: &dyn UsageCollectorPluginV1,
    earlier: UsageRecord,
    later: UsageRecord,
    role: &str,
) -> Result<
    (
        Result<UsageRecord, UsageCollectorPluginError>,
        Result<UsageRecord, UsageCollectorPluginError>,
    ),
    String,
> {
    let id = earlier.id;
    let outcomes = plugin
        .create_usage_records(vec![earlier, later])
        .await
        .map_err(|err| {
            format!(
                "`create_usage_records` failed the whole batch carrying the {role} pair of record \
                 {id}: {err}. Outcomes are per entry, so a collision between two entries of one \
                 batch is reported on the entry it belongs to and never as a failure of the call."
            )
        })?;
    if outcomes.len() != 2 {
        return Err(format!(
            "`create_usage_records` answered {count} outcome(s) for the {role} pair of record \
             {id}, which carried two entries. Per-entry outcomes are aligned with the input \
             order, so a caller cannot tell which entry an answer belongs to unless there is one \
             per entry.",
            count = outcomes.len(),
        ));
    }
    let mut outcomes = outcomes.into_iter();
    match (outcomes.next(), outcomes.next()) {
        (Some(earlier), Some(later)) => Ok((earlier, later)),
        // Unreachable: the length was just established as two. A report
        // rather than a panic, because a check drives a plugin nobody here
        // wrote.
        _ => Err(format!(
            "`create_usage_records` answered two outcomes for the {role} pair of record {id} and \
             then yielded fewer than two"
        )),
    }
}

/// Every entry the range under test comes back with on the ledger path.
///
/// A `Vec` rather than a set, because the count is the assertion: a set would
/// collapse the duplicated row this check exists to find.
///
/// `Err` carries a ready-to-report detail.
async fn dedup_floor_page(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupFloorFixtures,
) -> Result<Vec<UsageRecord>, String> {
    let page = plugin
        .list_usage_records(
            fixtures.meter.clone(),
            fixtures.range,
            &contract_query(DEDUP_FLOOR_PAGE_LIMIT),
            &[],
        )
        .await
        .map_err(|err| {
            format!(
                "`list_usage_records` failed over the range holding this check's three \
                 identities, so how many times one identity reads could not be decided: {err}"
            )
        })?;
    Ok(page.items)
}

/// Builds the three identities and the two divergent submissions.
///
/// The meter is this check's own rather than the suite's shared one.
/// [`run_all`](crate::contract::run_all) dispatches every check against one
/// persistent backend that never removes an entry, and this check counts the
/// rows a range returns, so it reads a meter nothing else writes to.
///
/// **Each idempotency key is submitted over one covered period, and that is a
/// decision rather than an accident.** `contract_mutants`'s
/// `Defect::DedupIgnoresThePeriod` keys an index on
/// `(tenant, type, idempotency_key, entry_type)` - the derived identity with
/// its two period bounds struck out - and refuses a second entry that claims a
/// key it already holds. A check reusing one key across two periods would hand
/// that subject a collision it must refuse, and would appear in its matrix row
/// for a rule that is not this one's. Holding each key to one period makes
/// that index answer exactly as the full identity does for everything this
/// check submits, so the coupling does not arise and the row stays this
/// check's own.
fn dedup_floor_fixtures() -> Result<DedupFloorFixtures, String> {
    let meter = check_meter(DEDUP_FLOOR, "main")?;
    let quantity = UsageQuantity::parse(DEDUP_FLOOR_QUANTITY).map_err(|err| {
        format!("the check's own quantity `{DEDUP_FLOOR_QUANTITY}` does not parse: {err}")
    })?;
    let divergent_quantity =
        UsageQuantity::parse(DEDUP_FLOOR_DIVERGENT_QUANTITY).map_err(|err| {
            format!(
                "the check's own divergent quantity `{DEDUP_FLOOR_DIVERGENT_QUANTITY}` does not \
                 parse: {err}"
            )
        })?;

    let converged_key = dedup_floor_key("converged")?;
    let batched_divergent_key = dedup_floor_key("batched-divergent")?;
    let entry = |key: &IdempotencyKey, quantity: UsageQuantity| {
        fixture_record_on(
            meter.clone(),
            CONTRACT_TENANT_ID,
            key,
            quantity,
            CONTRACT_ACCEPTED_AT,
            DEDUP_FLOOR_WINDOW_FROM,
            DEDUP_FLOOR_WINDOW_END,
        )
    };

    let converged = entry(&converged_key, quantity)?;
    let divergent = entry(&converged_key, divergent_quantity)?;
    let batched_identical = entry(&dedup_floor_key("batched-identical")?, quantity)?;
    let batched_divergent = entry(&batched_divergent_key, quantity)?;
    let batched_divergent_later = entry(&batched_divergent_key, divergent_quantity)?;

    let range = TimeRange::new(DEDUP_FLOOR_WINDOW_FROM, DEDUP_FLOOR_WINDOW_TO)
        .map_err(|err| format!("the check could not build its own read range: {err}"))?;

    let fixtures = DedupFloorFixtures {
        meter,
        range,
        converged,
        divergent,
        batched_identical,
        batched_divergent,
        batched_divergent_later,
    };
    fixtures.guards()?;
    Ok(fixtures)
}

impl DedupFloorFixtures {
    /// The three facts this check's assertions read, established rather than
    /// assumed. All three are the suite's own rather than the plugin's, so all
    /// three are reported as [`HARNESS_FAULT`] rather than against the plugin.
    ///
    /// * **Each divergent submission derives its target's id.** The quantity
    ///   is not one of the six identity inputs, so a pair differing only there
    ///   is one identity submitted twice. Were it an input, the pair would be
    ///   two entries colliding on nothing, every backend would accept both,
    ///   and the two conflict assertions would pass while asserting nothing.
    /// * **Each divergent submission really diverges**, in a field
    ///   [`UsageRecord::caller_supplied_eq`] reads. A collision resolves by
    ///   exact equality of the caller-supplied fields, so a "divergent"
    ///   submission equal in all of them is an ordinary absorbed retry and a
    ///   conforming backend would answer `Ok` where this check requires
    ///   `IdempotencyConflict`.
    /// * **The three identities are three.** The read half counts one row per
    ///   identity and the fold counts one term per identity, so two scenarios
    ///   sharing an id would make the expected count wrong - and would put two
    ///   scenarios' submissions on one identity, where the second scenario's
    ///   seed would resolve against the first's rather than be accepted.
    ///
    /// The two **identical** submissions need no guard of their own. Each is a
    /// clone of the entry it re-delivers, so that it derives the same id and
    /// departs in no caller-supplied field is the language's fact rather than
    /// the builder's, and a guard would assert that a value equals itself.
    fn guards(&self) -> Result<(), String> {
        for (role, divergent, target) in [
            ("separate-call", &self.divergent, &self.converged),
            (
                "in-batch",
                &self.batched_divergent_later,
                &self.batched_divergent,
            ),
        ] {
            if divergent.id != target.id {
                return Err(format!(
                    "the {role} divergent submission derives {derived} and the entry it is meant \
                     to collide with derives {target_id}; the quantity is not one of the six \
                     inputs to the derived identity, so a submission differing only there is the \
                     same identity - and two entries under two ids collide on nothing, so every \
                     backend would accept both and the conflict this check requires would never \
                     be reachable",
                    derived = divergent.id,
                    target_id = target.id,
                ));
            }
            if divergent.caller_supplied_eq(target) {
                return Err(format!(
                    "the {role} divergent submission of record {id} equals the entry it is meant \
                     to collide with in every caller-supplied field; a collision resolves by \
                     exact equality of those fields, so this submission is an ordinary retry and \
                     a conforming backend absorbs it where this check requires \
                     `IdempotencyConflict`",
                    id = target.id,
                ));
            }
        }

        let identities = std::collections::BTreeSet::from([
            self.converged.id,
            self.batched_identical.id,
            self.batched_divergent.id,
        ]);
        if identities.len() != 3 {
            return Err(format!(
                "this check's three scenarios seed {count} distinct identities rather than three; \
                 the read half counts one row per identity and the fold one term per identity, \
                 and two scenarios sharing an identity would both make that count wrong and have \
                 the second scenario's seed resolve against the first's instead of being accepted",
                count = identities.len(),
            ));
        }
        Ok(())
    }
}

/// The idempotency key one scenario of this check submits under.
///
/// Keyed on the scenario name for the reason
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture) spells
/// out: in a ledger with no delete path, an edited fixture must take a fresh
/// identity rather than inherit an accepted entry's.
fn dedup_floor_key(role: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{DEDUP_FLOOR}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}
