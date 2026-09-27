//! The DESIGN §3.3 `converged-target-lookup` check.
//!
//! See [`converged_target_lookup`] for what it asserts; the module holds the
//! entry it acknowledges and reads straight back, the entry it stores
//! outside the scope it dispatches, and the identifier it derives so that no
//! fixture can ever carry it.

use uuid::Uuid;

use crate::contract::fixtures::{
    CONTRACT_ACCEPTED_AT, CONTRACT_TENANT_ID, FIXTURE_EPOCH, SCOPE_EXCLUDED_TENANT_ID, check_meter,
    contract_scope, fixture_record_on, violation,
};
use crate::contract::{CONVERGED_TARGET_LOOKUP, ContractViolation, DedupLevel, HARNESS_FAULT};
use crate::error::UsageCollectorPluginError;
use crate::id::USAGE_RECORD_ID_NAMESPACE;
use crate::models::{IdempotencyKey, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;

/// The start of the covered period both entries of this check carry.
///
/// Two hundred and seventy days past [`FIXTURE_EPOCH`], for the reason
/// [`WINDOW_SELECTION_FROM`](super::window_end_selection::WINDOW_SELECTION_FROM)
/// gives, and clear of the day-0, 30, 60, 90, 120, 150, 180, 210, 240,
/// 300, 330 and 360 offsets the other checks take.
///
/// It buys less here than in any check that reads a range, and saying so is
/// the point: **this check dispatches no range at all.**
/// [`feed_snapshot_and_replay`](super::feed_snapshot_and_replay()) is the
/// other check the offset buys little for, and for the same reason. Every read it makes is
/// a point lookup by `id`, so no other check's entry can be miscounted into
/// a page of this one's and none can crowd one of this one's off a bounded
/// page either. The offset is kept because the separation runs the *other*
/// way: `run_all` dispatches every check against one backend that never
/// removes an entry, so these two entries are there for every check that
/// does read a range, and a period no other check's range covers is what
/// keeps them out of those reads. The meter is this check's own
/// ([`converged_lookup_fixtures`]) and is the stronger half of the same
/// separation; the offset holds on its own if a later check ever shares the
/// meter.
const CONVERGED_LOOKUP_WINDOW_FROM: time::OffsetDateTime =
    FIXTURE_EPOCH.saturating_add(time::Duration::days(270));

/// The end of that covered period. Both entries carry it: they are separated
/// by their tenant and their idempotency key, and nothing here asks a range
/// to tell them apart.
const CONVERGED_LOOKUP_WINDOW_END: time::OffsetDateTime =
    CONVERGED_LOOKUP_WINDOW_FROM.saturating_add(time::Duration::hours(1));

/// The quantity both entries of this check carry.
///
/// Its value is asserted nowhere — every assertion here reads an outcome's
/// *shape* and, where an entry comes back, its `id`. It is exactly
/// representable in a binary float all the same, which keeps
/// `contract_mutants`'s `Defect::QuantityThroughFloat` from changing
/// anything on the way in and so from reaching this check for a rule that
/// belongs to `quantity-round-trip`.
const CONVERGED_LOOKUP_QUANTITY: &str = "5.5";

/// The two entries this check stores, and the identifier it asks about that
/// names neither.
struct ConvergedLookupFixtures {
    /// The entry the check acknowledges and then looks up straight away,
    /// under the tenant the dispatched scope pins.
    acknowledged: UsageRecord,
    /// An entry stored under a tenant the dispatched scope does **not**
    /// admit. It exists, and the lookup must answer for it exactly as it
    /// answers for [`Self::never_stored`].
    out_of_scope: UsageRecord,
    /// An identifier no entry carries, derived in a way that cannot collide
    /// with one — see [`never_stored_id`].
    never_stored: Uuid,
}

/// `converged-target-lookup` — *"The converged-only lookup obligation,
/// including a lagging read pool straight after acknowledgement, an
/// identifier that never existed, and an out-of-scope entry, which answers
/// as an absent one."* (DESIGN §3.3, "Plugin contract tests", line 1316.)
///
/// **That row is a pointer rather than the rule.** The obligation itself is
/// in §3.3's plugin-obligations list, under "Decide converged-only lookups":
/// *"A lookup with `converged_only` applies `scope` first. It returns the
/// survivor once converged, never reports an acknowledged, retained entry
/// missing, and answers `UsageRecordNotConverged` only until it can decide:
/// within the convergence bound plus the published query-path lag bound it
/// returns the survivor or `UsageRecordNotFound`."* The SPI restates it for
/// implementors on
/// [`get_usage_record`](UsageCollectorPluginV1::get_usage_record), which
/// also says who sets the flag: *"The gateway passes `true` when it resolves
/// an invalidation's target and `false` on the caller-facing point read."*
///
/// **The flag exists because convergence is a property of the store rather
/// than of elapsed time.** DESIGN §3.1's "Dedup level" row defines it: an
/// identity *"converges once no earlier write can still become visible, the
/// survivor is visible to every dedup check the plugin runs, and no persist
/// call that missed it is still to return; the plugin establishes this from
/// its commit or replication state, never from elapsed time, within a
/// declared convergence bound"*. So this check takes the declared
/// [`DedupLevel`], as `at-most-one-invalidation` does, and the last clause
/// of the obligation is bounded by it: `linearizable` declares a zero bound
/// and decides every write as it commits, `eventual` declares one and may
/// answer undecided until it has passed.
///
/// Four probes, each reported on its own so they fail independently:
///
/// 1. **Straight after acknowledgement.** An entry is submitted and looked
///    up immediately with `converged_only = true`. Under
///    [`DedupLevel::Linearizable`] the survivor comes back. Under
///    [`DedupLevel::Eventual`] an undecided answer is admissible, and the
///    check then sleeps the declared bound and reads again, where the
///    survivor must come back.
///    [`UsageCollectorPluginError::UsageRecordNotFound`] is a violation at
///    either level: the obligation says the lookup *"never reports an
///    acknowledged, retained entry missing"*, and that is the clause a
///    backend reading a lagging replica breaks. This is what the row calls
///    "a lagging read pool straight after acknowledgement", and the cost of
///    breaking it is the gateway's: the gear resolves an invalidation's
///    target through this flag, so a target reported missing is a withdrawal
///    refused over a record that is sitting in the ledger.
/// 2. **An identifier that never existed.** `UsageRecordNotFound`, never
///    `UsageRecordNotConverged` — the latter leaves a caller retrying
///    forever for a row that will never arrive, which is the failure the
///    obligation's "only until it can decide" forbids.
/// 3. **An out-of-scope entry.** Stored under a tenant the dispatched scope
///    does not admit, and read under it. `UsageRecordNotFound`, the **same**
///    answer probe 2 got. The row says it *"answers as an absent one"*, and
///    telling the two apart makes the surface an existence oracle for other
///    tenants' entries.
/// 4. **`scope` is applied first, independent of the flag.** The same
///    out-of-scope read at `converged_only = false` answers identically. The
///    flag governs convergence and never authorization, which is the
///    obligation's first sentence and the one probe the DESIGN row does not
///    name. A backend that consults the scope only on the converged path
///    passes probe 3 and leaks on the other.
///
/// **Which of the four any subject reaches was measured by neutering each in
/// turn**, and the answer bounds what this check's matrix rows establish:
///
/// * **Probe 1 is individually load-bearing.** Neutering it leaves
///   `contract_mutants`'s
///   `Defect::AnswersNotConvergedForAnAcknowledgedEntry` reported by nothing.
/// * **Probes 3 and 4 are load-bearing together and neither alone.**
///   `Defect::IgnoresScopeOnThePointRead` answers the withheld row on both
///   flags, so neutering either leaves the other reporting and that subject's
///   row unchanged; neutering both takes this check out of it.
/// * **Probe 2 is reached by no subject**, and a subject added for it would
///   not change that. The reason is DESIGN's own, and it is the same sentence
///   probe 3 rests on: the obligation has the lookup apply `scope` *first*,
///   so an identifier that was never stored and a row the scope withholds
///   leave a conforming backend in one state — its lookup selected no row.
///   Any backend wrong about how it answers that state is wrong about it in
///   both probes, so no subject wrong in exactly one way reaches probe 2
///   without reaching probe 3, and probe 3 already reports. The only subject
///   that could separate them would have to answer the two differently, which
///   means consulting the row before the scope — the existence oracle this
///   surface forbids, and a mistake about authorization rather than about
///   convergence.
///
///   It stays, because the pairing is the row's own demand: probe 3 says the
///   withheld entry answers *as an absent one*, and probe 2 is where an
///   absent one is actually asked. A recorded gap rather than a closed one.
///
/// **What this check does not reach.** The obligation's bound is *"the
/// convergence bound plus the published query-path lag bound"*, and nothing
/// here measures either: probe 1 waits out the declared convergence bound
/// and then requires a decided answer, which is the weaker claim that the
/// backend decides *at all*. A backend that converges later than it declared
/// is a conformance defect DESIGN names and this check cannot time.
pub async fn converged_target_lookup(
    plugin: &dyn UsageCollectorPluginV1,
    level: DedupLevel,
) -> Vec<ContractViolation> {
    let fixtures = match converged_lookup_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own `{CONVERGED_TARGET_LOOKUP}` \
                     fixtures, so nothing was submitted. This is a fault in the suite, not in \
                     the plugin under test: {detail}"
                ),
            )];
        }
    };

    // A refusal stops the check. Three of the four probes are statements
    // about a stored entry, and the fourth - the identifier that never
    // existed - says nothing on its own: a backend that stored nothing
    // answers `UsageRecordNotFound` to everything and would pass it.
    for (role, record) in [
        ("acknowledged", &fixtures.acknowledged),
        ("out-of-scope", &fixtures.out_of_scope),
    ] {
        if let Err(err) = plugin.create_usage_record(record.clone()).await {
            return vec![violation(
                CONVERGED_TARGET_LOOKUP,
                format!(
                    "`create_usage_record` refused the {role} entry (record {id}, tenant \
                     {tenant}), so the converged-only lookup could not be asked what it answers \
                     for an entry that was acknowledged and retained: {err}",
                    id = record.id,
                    tenant = record.tenant_id,
                ),
            )];
        }
    }

    let mut violations = the_acknowledged_entry_is_never_missing(plugin, &fixtures, level).await;
    violations.extend(an_identifier_that_never_existed_is_absent(plugin, &fixtures).await);
    violations.extend(an_out_of_scope_entry_answers_as_an_absent_one(plugin, &fixtures).await);
    violations
}

/// Probe one: the entry just acknowledged reads back under
/// `converged_only = true`, and is never reported missing.
///
/// The undecided answer is admissible exactly once and only under
/// [`DedupLevel::Eventual`]: the check then sleeps the declared convergence
/// bound and reads again, and the second read has to decide. Under
/// [`DedupLevel::Linearizable`] the declared bound is zero and every write
/// is decided as it commits, so the first read already has to.
///
/// The comparison is on the `id` alone, deliberately. What the obligation
/// requires back is *"the survivor"*, and the survivor is named by its
/// identity; the fields it carries are `server-field-round-trip`'s rule, and
/// comparing them here would report that check's subjects under this check's
/// name.
async fn the_acknowledged_entry_is_never_missing(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &ConvergedLookupFixtures,
    level: DedupLevel,
) -> Vec<ContractViolation> {
    let id = fixtures.acknowledged.id;
    let scope = contract_scope();
    let first = plugin.get_usage_record(id, &scope, true).await;

    if let (
        Err(UsageCollectorPluginError::UsageRecordNotConverged { .. }),
        DedupLevel::Eventual { convergence_bound },
    ) = (&first, level)
    {
        toolkit::tokio::time::sleep(convergence_bound).await;
        let second = plugin.get_usage_record(id, &scope, true).await;
        return the_survivor_came_back(
            &second,
            fixtures,
            level,
            ConvergedLookupPhase::AfterTheBound,
        );
    }

    the_survivor_came_back(&first, fixtures, level, ConvergedLookupPhase::Immediately)
}

/// Which of probe one's two reads an outcome came from.
///
/// Only the report differs, and it differs because the two are different
/// things to tell a plugin author: one lookup was answered the instant the
/// acknowledgement returned, the other after every moment the plugin asked
/// for. The second exists only under [`DedupLevel::Eventual`] and only after
/// an undecided first answer, which is what makes
/// [`the_survivor_came_back`]'s `Eventual` message — *the declared bound has
/// already passed* — true wherever it is reached.
#[derive(Debug, Clone, Copy)]
enum ConvergedLookupPhase {
    /// The read dispatched straight after the acknowledgement returned.
    Immediately,
    /// The read dispatched after the declared convergence bound had passed.
    AfterTheBound,
}

impl ConvergedLookupPhase {
    /// How the report names when the read happened.
    fn as_str(self) -> &'static str {
        match self {
            Self::Immediately => "straight after `create_usage_record` returned for that entry",
            Self::AfterTheBound => {
                "after an undecided first answer and the whole of the declared convergence bound"
            }
        }
    }
}

/// The one assertion probe one makes: this outcome is the survivor.
///
/// Every other outcome is a violation, and the report says what each one
/// means rather than printing the debug shape and leaving the reading to
/// whoever opens the file. The three a plugin actually gives are
/// distinguished: an entry under the wrong `id`, a missing entry, and an
/// undecided answer the obligation no longer admits.
fn the_survivor_came_back(
    outcome: &Result<UsageRecord, UsageCollectorPluginError>,
    fixtures: &ConvergedLookupFixtures,
    level: DedupLevel,
    phase: ConvergedLookupPhase,
) -> Vec<ContractViolation> {
    let id = fixtures.acknowledged.id;
    let reading = match outcome {
        Ok(entry) if entry.id == id => return Vec::new(),
        Ok(entry) => format!(
            "answered record {observed}. A scope narrows which entries a lookup may answer \
             with; it does not change which entry was asked for, and the survivor of an \
             identity is named by that identity.",
            observed = entry.id,
        ),
        Err(UsageCollectorPluginError::UsageRecordNotFound { .. }) => format!(
            "reported it missing. The obligation is that a converged-only lookup never reports \
             an acknowledged, retained entry missing, and this is the clause a backend reading \
             a lagging replica breaks: the write went to the primary, the read went to a pool \
             that has not seen it, and `no row` was answered as `no entry`. Nothing has removed \
             this one - the suite retains everything it writes - and the gateway resolves an \
             invalidation's target through this flag, so a target reported missing is a \
             withdrawal refused over a record sitting in the ledger. The entry was \
             acknowledged under tenant {tenant}, which the dispatched scope pins.",
            tenant = fixtures.acknowledged.tenant_id,
        ),
        Err(UsageCollectorPluginError::UsageRecordNotConverged { .. }) => match level {
            DedupLevel::Linearizable => "answered that the identity has not converged. A \
                 `linearizable` declaration is a zero convergence bound and every write decided \
                 as it commits, so an acknowledgement already means converged and there is \
                 nothing left for this lookup to wait for."
                .to_owned(),
            DedupLevel::Eventual { convergence_bound } => format!(
                "answered that the identity has not converged, and the declared bound \
                 ({convergence_bound:?}) has already passed. Undecided is admissible only until \
                 the plugin can decide: within the convergence bound plus the published \
                 query-path lag bound the lookup returns the survivor or \
                 `UsageRecordNotFound`. A lookup that never leaves this answer is a caller \
                 retrying forever."
            ),
        },
        Err(err) => format!(
            "failed as `{err}`. The entry is stored, it is inside the dispatched scope, and \
             the only answers this lookup has for it are the survivor or - once it can decide \
             that the entry is not there - `UsageRecordNotFound`."
        ),
    };

    vec![violation(
        CONVERGED_TARGET_LOOKUP,
        format!(
            "`get_usage_record(converged_only = true)` was asked for record {id} {when}, and it \
             {reading}",
            when = phase.as_str(),
        ),
    )]
}

/// Probe two: an identifier no entry ever carried is absent, not undecided.
///
/// The variant is the assertion. `UsageRecordNotConverged` here is a caller
/// told to come back later about a row that will never arrive, and the
/// gateway lifts that variant to a *retryable* conflict, so the caller
/// really does come back.
async fn an_identifier_that_never_existed_is_absent(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &ConvergedLookupFixtures,
) -> Vec<ContractViolation> {
    let id = fixtures.never_stored;
    let outcome = plugin.get_usage_record(id, &contract_scope(), true).await;
    if reads_as_absent(&outcome, id) {
        return Vec::new();
    }
    vec![violation(
        CONVERGED_TARGET_LOOKUP,
        format!(
            "`get_usage_record(converged_only = true)` was asked for {id}, which no entry of \
             this suite carries and which is not a derived entry identity at all, and answered \
             {outcome:?}. The one answer it has for an identifier it holds nothing under is \
             `UsageRecordNotFound` naming that identifier. `UsageRecordNotConverged` is a \
             caller told to come back later about a row that will never arrive - the gateway \
             lifts it to a retryable conflict, so the caller really does come back - and \
             undecided is admissible only until the plugin can decide."
        ),
    )]
}

/// Probes three and four: a stored entry the scope withholds answers exactly
/// as an entry that does not exist, whatever the flag says.
///
/// Two reads rather than one, and the second is the half DESIGN's row does
/// not name. The obligation's first sentence is that the lookup *"applies
/// `scope` first"* — before the convergence question, and therefore
/// independently of it. A backend that consults the scope only on the
/// converged path passes the first read here and serves another tenant's
/// entry on the second, which is the caller-facing one: the SPI has the
/// gateway pass `false` on the point read a consumer reaches.
///
/// Each read is reported on its own. They fail for different reasons and a
/// plugin author fixing one is not thereby told about the other.
async fn an_out_of_scope_entry_answers_as_an_absent_one(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &ConvergedLookupFixtures,
) -> Vec<ContractViolation> {
    let id = fixtures.out_of_scope.id;
    let tenant = fixtures.out_of_scope.tenant_id;
    let scope = contract_scope();
    let mut violations = Vec::new();

    let converged = plugin.get_usage_record(id, &scope, true).await;
    if !reads_as_absent(&converged, id) {
        violations.push(violation(
            CONVERGED_TARGET_LOOKUP,
            format!(
                "`get_usage_record(converged_only = true)` was asked for record {id}, which is \
                 stored and belongs to tenant {tenant} - a tenant the dispatched scope does not \
                 name - and answered {converged:?}. A converged-only lookup applies `scope` \
                 first, and an entry outside it answers exactly as an absent one: \
                 `UsageRecordNotFound` naming the id asked for, which is what {absent} got. \
                 Anything a caller can tell apart from that - the entry itself, a \
                 distinguishable denial, or `UsageRecordNotConverged` for a row this caller \
                 will never be shown - makes the surface an existence oracle: guess a uuid, \
                 read the answer, learn that another tenant holds an entry under it.",
                absent = fixtures.never_stored,
            ),
        ));
    }

    let caller_facing = plugin.get_usage_record(id, &scope, false).await;
    if !reads_as_absent(&caller_facing, id) {
        violations.push(violation(
            CONVERGED_TARGET_LOOKUP,
            format!(
                "`get_usage_record` was asked for record {id} twice under one scope that does \
                 not name its tenant ({tenant}), and the two flags answered differently: \
                 `converged_only = true` gave {converged:?} and `converged_only = false` gave \
                 {caller_facing:?}. The flag governs convergence and never authorization - the \
                 scope is applied first, before the convergence question and so independently \
                 of it - so both are `UsageRecordNotFound`. This is the caller-facing read of \
                 the two: the gateway passes `false` on the point read a consumer reaches and \
                 `true` only when it resolves an invalidation's target, so a backend that \
                 consults the scope on the converged path alone leaks exactly where it is \
                 read."
            ),
        ));
    }

    violations
}

/// Whether an outcome is the one answer a lookup has for a row it may not
/// answer with: [`UsageCollectorPluginError::UsageRecordNotFound`] naming
/// the identifier that was asked about.
///
/// The reported `id` is part of it. A caller cannot tell a withheld entry
/// from an absent one, so the identifier in the refusal has to be the one it
/// asked about - a refusal naming some other entry is a fact about the store
/// the caller was not meant to learn.
fn reads_as_absent(
    outcome: &Result<UsageRecord, UsageCollectorPluginError>,
    asked_for: Uuid,
) -> bool {
    matches!(
        outcome,
        Err(UsageCollectorPluginError::UsageRecordNotFound { id }) if *id == asked_for
    )
}

/// An identifier no entry can carry.
///
/// A `UUIDv5` under the gear's own entry-identity namespace over this
/// check's name. **No entry can derive it by being built differently**, which
/// is a stronger statement than "no entry happens to carry it":
/// [`derive_usage_record_id`](crate::id::derive_usage_record_id) hashes the
/// six identity inputs joined by the ASCII unit separator, so every image it
/// hashes carries exactly five of that byte, and this one carries none —
/// the check name is `[a-z-]` throughout. There is no tenant, meter, key,
/// period or entry type whose derivation is handed this image.
///
/// That bounds it at the hash rather than at the naming: two different
/// images could still collide, since a `UUIDv5` is a truncated SHA-1. The
/// guard in [`converged_lookup_fixtures`] is what covers the remainder, and
/// it covers it against the two entries this check actually stores rather
/// than against an argument.
///
/// Derived rather than written as a literal for the reason the row gives -
/// *"an identifier that never existed"* - held against how this suite runs:
/// [`run_all`](crate::contract::run_all) dispatches every check against one
/// persistent backend that never removes an entry, so "no entry carries
/// this" has to hold across every check and every repeated run, not just at
/// the moment the literal was chosen.
fn never_stored_id() -> Uuid {
    Uuid::new_v5(
        &USAGE_RECORD_ID_NAMESPACE,
        CONVERGED_TARGET_LOOKUP.as_bytes(),
    )
}

/// Builds the acknowledged entry, the entry outside the scope, and the
/// identifier that names neither.
///
/// The meter is this check's own rather than the suite's shared one.
/// [`SCOPE_EXCLUDED_TENANT_ID`] already owns entries — it is the tenant
/// `scope-is-a-filter-on-every-read-path` withholds from its own reads — so
/// this check's out-of-scope entry has to be told apart from those. For this
/// check's own reads the `id` does that on its own: every read here is a
/// point lookup, and the two entries the other check stores derive ids of
/// their own from their own keys. The meter is what carries the separation
/// in the direction no assertion here can see — `run_all` dispatches every
/// check against one backend that never removes an entry, so an entry of
/// this check's landing on the shared meter would be an entry some *other*
/// check's page or fold has to account for, and one of them is under a
/// tenant that check's scope excludes.
///
/// Three guards keep the check from passing by construction, and all three
/// are the suite's own facts rather than the plugin's, so all three are
/// reported as [`HARNESS_FAULT`]:
///
/// * The two tenants differ. If they did not, the dispatched scope would
///   admit both entries, and probes three and four would be asserting that a
///   lookup withholds an entry it is entitled to answer with.
/// * The two entries derive different ids. The tenant is one of the six
///   identity inputs and the idempotency key another, so they do - but if
///   they ever did not, probe one and probes three and four would be making
///   opposite demands of one row.
/// * The identifier that never existed is neither of those ids. It cannot be
///   ([`never_stored_id`] says why), and the guard is what turns that
///   argument into a fact the suite keeps: probe two asserts an answer about
///   an entry that is not there, and an identifier that named a stored entry
///   would have it assert the opposite of probe one.
fn converged_lookup_fixtures() -> Result<ConvergedLookupFixtures, String> {
    if CONTRACT_TENANT_ID == SCOPE_EXCLUDED_TENANT_ID {
        return Err(format!(
            "the acknowledged and the out-of-scope entry are attributed to the same tenant \
             ({CONTRACT_TENANT_ID}), so the dispatched scope admits both and the two probes \
             about an entry outside it have nothing to assert"
        ));
    }

    let meter = check_meter(CONVERGED_TARGET_LOOKUP, "main")?;
    let quantity = UsageQuantity::parse(CONVERGED_LOOKUP_QUANTITY).map_err(|err| {
        format!("the check's own quantity `{CONVERGED_LOOKUP_QUANTITY}` does not parse: {err}")
    })?;
    let entry = |tenant_id: Uuid, key: &IdempotencyKey| {
        fixture_record_on(
            meter.clone(),
            tenant_id,
            key,
            quantity,
            CONTRACT_ACCEPTED_AT,
            CONVERGED_LOOKUP_WINDOW_FROM,
            CONVERGED_LOOKUP_WINDOW_END,
        )
    };

    let acknowledged = entry(CONTRACT_TENANT_ID, &converged_lookup_key("acknowledged")?)?;
    let out_of_scope = entry(
        SCOPE_EXCLUDED_TENANT_ID,
        &converged_lookup_key("out-of-scope")?,
    )?;
    if acknowledged.id == out_of_scope.id {
        return Err(format!(
            "the acknowledged and the out-of-scope entry derive one id ({id}), so probe one \
             would require the lookup to answer with the very row probes three and four require \
             it to withhold",
            id = acknowledged.id,
        ));
    }

    let never_stored = never_stored_id();
    if never_stored == acknowledged.id || never_stored == out_of_scope.id {
        return Err(format!(
            "the identifier this check asks about as one that never existed ({never_stored}) is \
             the derived identity of one of its own stored entries ({acknowledged}, \
             {out_of_scope}); the probe that reads it asserts an answer about an entry that is \
             not there, and it would be asserting the opposite of the probe beside it",
            acknowledged = acknowledged.id,
            out_of_scope = out_of_scope.id,
        ));
    }

    Ok(ConvergedLookupFixtures {
        acknowledged,
        out_of_scope,
        never_stored,
    })
}

/// The idempotency key one of this check's two entries submits under.
///
/// Keyed on the role name for the reason
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture) spells
/// out: in a ledger with no delete path, an edited fixture must take a fresh
/// identity rather than inherit an accepted entry's.
fn converged_lookup_key(role: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{CONVERGED_TARGET_LOOKUP}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}
