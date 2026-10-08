//! The DESIGN §3.3 `dedup-floor` check.
//!
//! See [`dedup_floor`] for what it asserts; the module holds the identities
//! it seeds, the retry and divergent submissions they resolve, and the
//! read-back that counts what the ledger actually holds.

use bigdecimal::BigDecimal;
use uuid::Uuid;

use crate::contract::fixtures::{
    CONTRACT_ACCEPTED_AT, CONTRACT_TENANT_ID, check_meter, check_window_from, contract_query,
    fixture_record_on, seed_usage_record, violation,
};
use crate::contract::{ContractViolation, DEDUP_FLOOR, HARNESS_FAULT};
use crate::error::UsageCollectorPluginError;
use crate::models::{AggregationFold, IdempotencyKey};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::stored::{MeterRef, StoredUsageRecord};
use crate::time_range::TimeRange;

/// The start of the covered period every entry of this check carries, and
/// the inclusive lower bound of the range it reads them back over.
///
/// The offset [`check_window_from`] tables for this check, for the reason
/// that accessor gives. It matters here because this check counts the rows a
/// range returns against the identities it seeded, so a stray entry would
/// read as a row no identity accounts for — the shape of the failure the
/// check exists to report. It also reads over a meter of its own (see
/// [`dedup_floor_fixtures`]), so range and meter are independent reasons
/// nothing else can land in the page this check counts.
const DEDUP_FLOOR_WINDOW_FROM: time::OffsetDateTime = check_window_from(DEDUP_FLOOR, "main");

/// The end of that covered period.
///
/// **Every entry of this check carries one period**, deliberately: the
/// identities are separated by their idempotency keys alone, so no key is
/// ever submitted over a second period. That keeps `contract_mutants`'s
/// `Defect::DedupIgnoresThePeriod` — an index on the derived identity with
/// the period bounds struck out — out of this check's matrix row, since such
/// an index answers exactly as the full identity does here. See
/// [`dedup_floor_fixtures`].
const DEDUP_FLOOR_WINDOW_END: time::OffsetDateTime =
    DEDUP_FLOOR_WINDOW_FROM.saturating_add(time::Duration::hours(1));

/// The exclusive upper bound of the range this check reads over, an hour past
/// [`DEDUP_FLOOR_WINDOW_END`].
///
/// The range holds **both** covered-period bounds, for the reason
/// `server-field-round-trip`'s own upper bound gives: this check asserts
/// nothing about period selection, and a range beginning at
/// [`DEDUP_FLOOR_WINDOW_END`] would make its read halves unanswerable for a
/// backend selecting on `window_start`.
const DEDUP_FLOOR_WINDOW_TO: time::OffsetDateTime =
    DEDUP_FLOOR_WINDOW_END.saturating_add(time::Duration::hours(1));

/// The quantity each identity's accepted entry carries.
///
/// Exactly representable in a binary float, so `contract_mutants`'s
/// `Defect::QuantityThroughFloat` changes nothing this check can see. Every
/// assertion here compares a plugin's answer against a fixture this module
/// built, so a rewritten quantity would come back unequal and be reported
/// here for a mistake that is `quantity-round-trip`'s rule.
const DEDUP_FLOOR_QUANTITY: &str = "5.5";

/// The quantity the two **divergent** submissions carry instead.
///
/// The divergence has to be in a caller-supplied field that is not an
/// identity input, so the submission derives the accepted entry's own `id`
/// and still fails [`StoredUsageRecord::caller_supplied_eq`]. The quantity is
/// such a field: it is no input to [`crate::id::derive_usage_record_id`], and
/// DESIGN §3.1's "Collision resolution" compares *"exact equality of the
/// caller-supplied fields"*, the quantity among them.
///
/// Exactly representable in a binary float for the reason
/// [`DEDUP_FLOOR_QUANTITY`] gives, plus one of its own: a rewrite landing both
/// quantities on one value would make a divergent submission an ordinary
/// retry, and the conflict assertions would hold a conforming backend to an
/// outcome it must not give.
const DEDUP_FLOOR_DIVERGENT_QUANTITY: &str = "7.25";

/// The read limit this check dispatches, on the ledger page and in the fold's
/// query alike: twice the identities it expects.
///
/// The margin is the assertion, for the reason
/// [`DEDUP_PAGE_LIMIT`](super::dedup_identity_over_window::DEDUP_PAGE_LIMIT)
/// gives: **the row count is the whole of this check's first half**, so a
/// limit set to the expected count would truncate the extra row away and the
/// assertion would pass against a backend holding two rows for one identity.
const DEDUP_FLOOR_PAGE_LIMIT: u64 = 6;

/// The three identities this check seeds, the submissions that resolve
/// against them, and the meter they are read back over.
struct DedupFloorFixtures {
    /// The meter every entry is written to and the only meter this check
    /// reads.
    meter: MeterRef,
    /// The range both read halves dispatch, built once.
    ///
    /// Built here rather than at each read so a range the suite cannot
    /// construct is reported once as [`HARNESS_FAULT`], rather than once per
    /// read against the plugin.
    range: TimeRange,
    /// The identity resolved across **separate calls**: submitted, retried
    /// verbatim, then submitted again with different content.
    converged: StoredUsageRecord,
    /// The divergent submission against [`Self::converged`]: the same six
    /// identity inputs, a different quantity.
    divergent: StoredUsageRecord,
    /// The identity resolved **inside one batch call** by an identical later
    /// entry. Submitted twice in one `create_usage_records`.
    batched_identical: StoredUsageRecord,
    /// The earlier of the two entries of the identity resolved inside one
    /// batch call by a **divergent** later entry.
    ///
    /// A key of its own rather than [`Self::batched_identical`]'s, so the two
    /// batch cases are separate identities and neither can contaminate the
    /// other.
    batched_divergent: StoredUsageRecord,
    /// The later of that pair: the same six identity inputs as
    /// [`Self::batched_divergent`], a different quantity.
    batched_divergent_later: StoredUsageRecord,
}

/// One scenario's result: the identity it left on the ledger, and what it
/// reported.
///
/// Separate because a scenario can seed its identity **and** report a
/// violation — a retry refused over a record the backend did store, say. The
/// read half counts rows only for identities actually seeded, so a refused
/// seed is reported once rather than again as a missing row.
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
/// (DESIGN §3.3, "Plugin contract tests".)
///
/// **The row's sentences are separate obligations, not one stated three
/// ways.**
///
/// The first is the floor this check is named for, and it is a statement
/// about the **store** rather than about any answer a caller reads. DESIGN
/// §3.1's "Dedup identity" row states it: *"One identity yields at most one
/// entry on every read path, fold, reconciliation figure, and materialised
/// aggregate."* A backend can answer every individual call below correctly
/// and still have written the row twice — the outcome a caller reads is
/// decided by a lookup, and a ledger whose write does not repeat that lookup
/// inserts the duplicate anyway. So this check reads the ledger back, which
/// is why it is named `dedup-floor` rather than `dedup-outcomes`.
///
/// The rest are the outcomes themselves, and DESIGN §3.1's "Collision
/// resolution" row is the authority: *"A collision on the full identity,
/// `entry_type` included, resolves by exact equality of the caller-supplied
/// fields. All equal, the entry is absorbed. Any field differing, metadata
/// alone included, is `IdempotencyConflict` once the identity has converged …
/// Two entries in one request sharing the full identity resolve the same way
/// at the gateway, the later against the earlier."*
/// [`create_usage_records`](UsageCollectorPluginV1::create_usage_records)
/// repeats the batch half.
///
/// One identity is seeded per scenario, each under its own idempotency key so
/// that no outcome of one can be explained by another:
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
/// **The overlap with the rest of the suite is what the matrix row reports.**
/// [`dedup_identity_over_window`](super::dedup_identity_over_window()) counts
/// the rows carrying one id after a verbatim retry, and
/// [`record_and_invalidation_distinct_identity`](super::record_and_invalidation_distinct_identity())
/// counts a record and its withdrawal after a retry — so the subject built
/// for this rule, `contract_mutants`'s `Defect::LedgerHasNoUniqueConstraint`,
/// fails all of them, and that row establishes that they together notice such
/// a backend rather than which noticed.
///
/// This check's own are the **fold** half and the identities a batch call
/// resolved: neither neighbour reads a fold or submits one identity twice
/// inside a single call. The in-batch half is also the only assertion here
/// with a subject of its own — `Defect::BatchResolvesAgainstThePreCallLedger`,
/// which decides every row of a batch against the ledger as it stood before
/// the call.
///
/// Which assertions a subject reaches was measured by neutering each in turn,
/// and recorded on the two defects rather than here.
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
/// Both outcomes are compared against the **stored** entry rather than the
/// submission in hand, which is what an absorb and a conflict are for: each
/// hands the caller the entry that won the identity.
///
/// **Separate one-entry calls, and that is the sentence.** The rule here is
/// cross-call resolution against a *converged* identity; two submissions
/// inside one `create_usage_records` would be resolved by the SPI's
/// intra-batch "later against earlier" rule instead, which is the next two
/// scenarios' subject over identities of their own.
async fn the_converged_identity_resolves_both_ways(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupFloorFixtures,
) -> Scenario {
    if let Err(err) = seed_usage_record(plugin, &fixtures.meter, fixtures.converged.clone()).await {
        return Scenario {
            seeded: None,
            violations: vec![violation(
                DEDUP_FLOOR,
                format!(
                    "`create_usage_records` refused the seed of this check's separate-call \
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
    match seed_usage_record(plugin, &fixtures.meter, fixtures.converged.clone()).await {
        Ok(stored) if is_the_stored(&stored, &fixtures.converged) => {}
        outcome => violations.push(violation(
            DEDUP_FLOOR,
            format!(
                "record {id} was re-delivered verbatim against a converged identity and \
                 `create_usage_records` answered {outcome:?}. A collision resolves by exact \
                 equality of the caller-supplied fields, and all of them are equal here, so the \
                 entry is absorbed and the stored entry comes back - under its own id and \
                 carrying the fields the caller supplied on it.",
                id = fixtures.converged.id,
            ),
        )),
    }

    match seed_usage_record(plugin, &fixtures.meter, fixtures.divergent.clone()).await {
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
/// the outcome alone: the two copies carry the same content by construction,
/// so nothing here can tell which the backend answered with.
///
/// That bounds it, measured rather than guessed: an absorb and a second
/// *acceptance* are indistinguishable on an identical pair, so
/// `contract_mutants`'s `Defect::BatchResolvesAgainstThePreCallLedger`, which
/// has no in-batch dedup map at all, passes here and is caught by the
/// divergent pair. What this does catch is a backend that **refuses** the
/// later entry — a conflict between a caller and itself — which no subject in
/// the module does today.
async fn one_batch_call_absorbs_an_identical_later_entry(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupFloorFixtures,
) -> Scenario {
    let entry = &fixtures.batched_identical;
    let outcomes = match dedup_floor_batch(
        plugin,
        &fixtures.meter,
        entry.clone(),
        entry.clone(),
        "identical",
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
        &fixtures.meter,
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
/// **This is the assertion no outcome above can make.** Every call could be
/// answered correctly by a backend that writes a second row for each: the
/// answer is decided by a lookup, and a ledger whose write does not repeat
/// that lookup — no unique constraint on the dedup identity, no conflict
/// target — stores the duplicate anyway, while absorbing retries and
/// reporting conflicts against the right entries throughout.
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
/// A second read path rather than a restatement of the first: DESIGN §3.1
/// puts the obligation on *"every read path, fold, reconciliation figure, and
/// materialised aggregate"*, and a fold is not served from the ledger page, so
/// a backend whose `COUNT` runs against a materialised aggregate can hold one
/// row and count it twice.
///
/// `COUNT` rather than `SUM` because it reads no quantity: the divergent
/// submissions differ from their targets exactly in the quantity, so a `SUM`
/// could not tell a duplicate from a wrongly-resolved collision.
async fn the_fold_counts_one_per_identity(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupFloorFixtures,
    seeded: &[(&'static str, Uuid)],
) -> Vec<ContractViolation> {
    let result = match plugin
        .query_aggregated_usage_records(
            &fixtures.meter,
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
/// The id **together with** every caller-supplied field
/// ([`StoredUsageRecord::caller_supplied_eq`]) rather than either alone. The
/// id alone would admit an answer carrying the right identity and the wrong
/// content — the shape this check's divergent submissions have — and the
/// fields alone would admit the right content under some other entry's id,
/// which separate identities on one meter exist to keep apart.
///
/// Whole-record equality is deliberately not used: it also reads
/// `accepted_at` and `origin`, which are server-assigned and no part of this
/// check's rule, so a backend stamping its own instant would be reported here
/// as well as by the check built for that rule.
fn is_the_stored(stored: &StoredUsageRecord, expected: &StoredUsageRecord) -> bool {
    stored.id == expected.id && stored.caller_supplied_eq(expected)
}

/// Sends one two-entry batch and splits the outcomes.
///
/// `Err` carries a ready-to-report detail, for the two shapes this check
/// cannot read: the whole call failing, which is distinct from a per-entry
/// refusal, and an answer whose length is not the batch's — the SPI aligns
/// outcomes with the input order, so another length leaves nothing to align
/// against.
async fn dedup_floor_batch(
    plugin: &dyn UsageCollectorPluginV1,
    meter: &MeterRef,
    earlier: StoredUsageRecord,
    later: StoredUsageRecord,
    role: &str,
) -> Result<
    (
        Result<StoredUsageRecord, UsageCollectorPluginError>,
        Result<StoredUsageRecord, UsageCollectorPluginError>,
    ),
    String,
> {
    let id = earlier.id;
    let outcomes = plugin
        .create_usage_records(vec![(meter.clone(), earlier), (meter.clone(), later)])
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
/// collapse the duplicated row this check exists to find. `Err` carries a
/// ready-to-report detail.
async fn dedup_floor_page(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupFloorFixtures,
) -> Result<Vec<StoredUsageRecord>, String> {
    let page = plugin
        .list_usage_records(
            &fixtures.meter,
            fixtures.range,
            &contract_query(DEDUP_FLOOR_PAGE_LIMIT),
            &[],
            None,
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
/// The meter is this check's own rather than the suite's shared one:
/// [`run_all`](crate::contract::run_all) dispatches every check against one
/// persistent backend that never removes an entry, and this check counts the
/// rows a range returns.
///
/// **Each idempotency key is submitted over one covered period**, which is a
/// decision. `contract_mutants`'s `Defect::DedupIgnoresThePeriod` keys an
/// index on `(tenant, type, idempotency_key, entry_type)` — the derived
/// identity with its period bounds struck out — and refuses a second entry
/// claiming a key it already holds. Holding each key to one period makes that
/// index answer exactly as the full identity does here, so this check stays
/// out of that subject's row.
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
            &meter,
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
    /// The facts this check's assertions read, established rather than
    /// assumed. All are the suite's own rather than the plugin's, so all are
    /// reported as [`HARNESS_FAULT`].
    ///
    /// * **Each divergent submission derives its target's id.** The quantity
    ///   is no identity input, so a pair differing only there is one identity
    ///   submitted twice. Were it an input, the pair would collide on nothing,
    ///   every backend would accept both, and the conflict assertions would
    ///   pass while asserting nothing.
    /// * **Each divergent submission really diverges**, in a field
    ///   [`StoredUsageRecord::caller_supplied_eq`] reads. A "divergent"
    ///   submission equal in all of them is an ordinary absorbed retry, where
    ///   a conforming backend answers `Ok` and this check requires
    ///   `IdempotencyConflict`.
    /// * **The identities are distinct.** The read half counts one row per
    ///   identity and the fold one term per identity, so two scenarios sharing
    ///   an id would make the expected count wrong and have the second
    ///   scenario's seed resolve against the first's rather than be accepted.
    ///
    /// The **identical** submissions need no guard: each is a clone of the
    /// entry it re-delivers, so a guard would assert that a value equals
    /// itself.
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
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture) gives:
/// in a ledger with no delete path, an edited fixture must take a fresh
/// identity rather than inherit an accepted entry's.
fn dedup_floor_key(role: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{DEDUP_FLOOR}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}
