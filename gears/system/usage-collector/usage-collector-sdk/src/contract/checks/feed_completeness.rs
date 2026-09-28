//! The DESIGN §3.3 `feed-completeness` check.
//!
//! See [`feed_completeness`] for what it asserts. The module holds the nine
//! entries it writes — six records and the three corrections that withdraw
//! three of them — the two meters it names, and the two rounds of
//! concurrent callers that ingest them. The walk it reads them back with is
//! [`super::super::feed_walk`]'s, shared with
//! [`feed_snapshot_and_replay`](super::feed_snapshot_and_replay()).
//!
//! **It is the only check whose rule is the feed's own order and that
//! drives concurrency to reach it**, and the only one that reads an
//! invalidation and its target off one feed and asserts where they fell.
//! Neither is this module's taste: the row asks for *"concurrent ingestion
//! of records and invalidations through several gateway replicas"*.
//!
//! Two other checks come close enough to be worth telling apart.
//! `dedup-concurrent` races two submissions and reads a feed page, but its
//! racers are **one** identity and what it counts is how many writes of it
//! survived, which is a dedup rule. `server-field-round-trip` delivers a
//! record and its invalidation off a feed page too, and finds each by `id`:
//! it asserts the fields they carry, never where they fell.

use uuid::Uuid;

use crate::contract::feed_walk::{FeedWalk, WalkStop, feed_walk, first_divergence, ids};
use crate::contract::fixtures::{
    CONTRACT_ACCEPTED_AT, CONTRACT_TENANT_ID, check_meter, check_window_from, contract_scope,
    fixture_invalidation_on, fixture_record_on, violation,
};
use crate::contract::{ContractViolation, FEED_COMPLETENESS, HARNESS_FAULT};
use crate::error::UsageCollectorPluginError;
use crate::feed::{FeedPage, FeedPosition, FeedStart};
use crate::models::{IdempotencyKey, MeterTypeId, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;

/// The start of the covered period every entry of this check carries.
///
/// The offset [`check_window_from`] tables for this check, for the reason
/// that accessor gives.
///
/// It buys as little here as it does for
/// [`feed_snapshot_and_replay`](super::feed_snapshot_and_replay()), and for
/// the same reason: **this check dispatches no range at all.** Every read it
/// makes is a feed page, and a feed page selects by subscription rather than
/// by covered period, so the separation that does the work is
/// [`check_meter`]. The offset is kept because the separation runs the other
/// way as well: `super::super::run_all` dispatches every check against one
/// backend that never removes an entry, so these nine entries are there for
/// every check that *does* read a range, and a period no other check's range
/// covers is what keeps them out of those reads.
const FEED_COMPLETENESS_WINDOW_FROM: time::OffsetDateTime =
    check_window_from(FEED_COMPLETENESS, "main");

/// The end of that covered period. All nine entries carry it: a correction
/// repeats its target's covered period by construction (DESIGN §3.1,
/// Faithful copy), and nothing here asks a covered period to tell two
/// entries apart.
const FEED_COMPLETENESS_WINDOW_END: time::OffsetDateTime =
    FEED_COMPLETENESS_WINDOW_FROM.saturating_add(time::Duration::hours(1));

/// The acceptance instant this check's **corrections** carry: an hour
/// *before* the instant their targets carry.
///
/// **This is the one fixture choice in the module that is built for a
/// defect**, and DESIGN describes both the defect and this exact skew.
/// §3.10's deployment-guide item 3 obliges a plugin to publish *"how it
/// realises the feed's order"* and then rules one realisation out: *"Feed
/// order MUST NOT rest on gateway-stamped `accepted_at` alone: replica clock
/// skew can stamp an invalidation earlier than its target."* §3.1's Feed
/// order invariant says the same thing from the other side — *"No other
/// ordering is claimed, acceptance-instant order included"* — and then
/// requires *"an invalidation follows its target"* regardless.
///
/// A correction stamped at or after its target's instant would leave the
/// correction-order assertion below true of a backend whose feed is ordered
/// by the acceptance instant, which is the one order DESIGN names as
/// forbidden. The skew is what makes the assertion say something. That it
/// really is a skew rather than a coincidence is
/// [`FeedCompletenessFixtures::guards`]' business rather than this comment's.
const FEED_COMPLETENESS_CORRECTED_AT: time::OffsetDateTime =
    CONTRACT_ACCEPTED_AT.saturating_sub(time::Duration::hours(1));

/// The quantity every entry of this check carries.
///
/// Its value is asserted nowhere: the assertions compare identities and
/// compare two of this backend's own answers against each other. It is
/// exactly representable in a binary float all the same, which keeps
/// `contract_mutants`'s `Defect::QuantityThroughFloat` from changing
/// anything on the way in and so from reaching this check for a rule that
/// belongs to `quantity-round-trip`.
const FEED_COMPLETENESS_QUANTITY: &str = "1.5";

/// How many records this check ingests before it corrects any of them.
///
/// Six, which is two per replica under the round-robin deal below. Three
/// would give each caller one entry, and three callers submitting one entry
/// each have no order of their own to interleave: the ledger's order would
/// then be decided entirely by which caller the runtime polled first. With
/// two each, every caller has an internal order and the ledger's order is
/// the interleaving of three of them, which is the shape the row's *"several
/// gateway replicas"* describes. More than six changes nothing about that
/// and only lengthens a report.
const FEED_COMPLETENESS_RECORDS: usize = 6;

/// How many concurrent callers each round of ingestion is dispatched
/// through.
///
/// Three rather than two, because *"several gateway replicas"* is DESIGN's
/// phrase and two is the number `dedup-concurrent` already drives. The value
/// is not a parameter of the rule: nothing below counts replicas, and a
/// conforming backend owes the same order whether one caller or ten produced
/// the interleaving.
const FEED_COMPLETENESS_REPLICAS: usize = 3;

/// The page limit every walk in this check dispatches: **two**.
///
/// Two is chosen against this check's own ledger rather than in the
/// abstract, and both bounds do work. It is greater than one, because a page
/// carrying at most one entry is the shape `feed-snapshot-and-replay`
/// already walks and a second check walking it again would measure that
/// check's page arithmetic over this check's ledger. It is strictly less
/// than the nine entries this check writes, so every walk here really pages
/// and every page but the last stops **in front of** an entry it did not
/// carry — which is the point at which a page decides whether that entry is
/// still ahead of its cursor.
///
/// **The same limit is dispatched by every read this check compares**, which
/// is the point rather than an economy. A page limit changes where a page
/// boundary falls, so two walks taken at two limits can differ for a reason
/// that is about the limit; the comparison in
/// [`a_quiet_meter_changes_nothing_and_is_never_refused`] is meant to be
/// about the subscription alone.
const FEED_COMPLETENESS_PAGE_LIMIT: u64 = 2;

/// The nine entries this check writes and the two meters it reads them
/// over.
struct FeedCompletenessFixtures {
    /// The meter every entry is written to and the meter whose feed the
    /// completeness and correction-order probes walk.
    busy: MeterTypeId,
    /// The meter this check subscribes to and never writes to.
    ///
    /// DESIGN's row asks for *"a subscription that stays quiet while its
    /// consumer keeps reading"*, and a meter derived for this check's
    /// exclusive use is the only way to have one: `run_all` dispatches every
    /// check against one backend that never removes an entry, so any meter
    /// another check writes to is quiet only by luck of dispatch order.
    quiet: MeterTypeId,
    /// The six records, in the order they are handed to the replicas.
    records: Vec<UsageRecord>,
    /// The corrections, each paired with the index of the record it
    /// withdraws.
    ///
    /// The index rather than the target's `id`, because the assertion in
    /// [`corrections_follow_their_targets`] needs the record itself to find
    /// its place in a delivered order, and an `id` would have to be looked
    /// up again to get it.
    corrections: Vec<(usize, UsageRecord)>,
}

impl FeedCompletenessFixtures {
    /// Every entry this check submits, records first and corrections after,
    /// which is the order the two rounds submit them in.
    fn acknowledged(&self) -> Vec<&UsageRecord> {
        let mut entries: Vec<&UsageRecord> = self.records.iter().collect();
        entries.extend(self.corrections.iter().map(|(_, entry)| entry));
        entries
    }

    /// The corrections, each beside the record it withdraws.
    fn pairs(&self) -> Vec<(&UsageRecord, &UsageRecord)> {
        self.corrections
            .iter()
            .filter_map(|(index, correction)| {
                self.records.get(*index).map(|target| (target, correction))
            })
            .collect()
    }
}

/// `feed-completeness` — *"The Feed order invariant under concurrent
/// ingestion of records and invalidations through several gateway replicas,
/// including a subscription that stays quiet while its consumer keeps
/// reading, which is never refused."* (DESIGN §3.3, "Plugin contract tests",
/// line 1323.)
///
/// **That row is a pointer rather than the rule.** The rule is DESIGN §3.1's
/// "Feed order" invariant (line 600), and this check rests on the whole of
/// it: *"One deterministic order over a subscription, realised by the plugin
/// through `FeedPosition`. No other ordering is claimed, acceptance-instant
/// order included. **Completeness**: a page carries only settled entries —
/// converged, with nothing more able to become visible before them — so no
/// entry the read's compiled scope admits ever becomes visible behind a
/// returned cursor, whatever the concurrency or commit order. Completeness
/// binds the scope each read ran under; what a changed scope admits or
/// excludes is the §3.3 cursor contract, not an ordering matter. A page
/// reaching the settled head returns its cursor at the head, so a cursor's
/// age reflects its consumer's progress, not the last arrival. **Correction
/// order**: an invalidation follows its target."*
///
/// Two clauses are this check's and the rest are
/// [`feed_snapshot_and_replay`](super::feed_snapshot_and_replay())'s:
/// **completeness**, *"whatever the concurrency or commit order"*, and
/// **correction order**, which that check writes no invalidation to reach.
///
/// # A replica is modelled as a concurrent task, and why that is faithful
///
/// DESIGN's row ingests *"through several gateway replicas"*. The reference
/// backend is a single in-process instance behind a mutex, and this check
/// reaches it the only way the SPI offers: several futures over one
/// `&dyn UsageCollectorPluginV1` handle, driven together by
/// `toolkit::tokio::join!`.
///
/// **`dedup-concurrent` makes that argument in full and it carries over
/// whole** — see
/// [`dedup_concurrent`](super::dedup_concurrent())'s "A replica is modelled
/// as a concurrent task, and why that is faithful". Its shape is: every rule
/// the row rests on is a property of the *store's own serialisation*, none
/// of them mentions the caller, and a plugin cannot see how many processes
/// its submissions came from or key on it if it could.
///
/// **The argument has to be made again here because the rule is a different
/// one**, and a reader entitled to check it should not have to assume it
/// transfers. It does, and this is why. Completeness is stated over *"the
/// concurrency or commit order"* — the store's commit order, the same object
/// `dedup-concurrent`'s clauses are about — and correction order is stated
/// over a target and the invalidation that names it, which is a relation
/// between two rows of one ledger. Neither says anything about callers.
/// Two interleaved submissions are two interleaved submissions whether they
/// arrive from two tasks or two replicas, and the page a backend then serves
/// owes the same order either way.
///
/// What the reduction does not buy is the same thing it does not buy there:
/// it does not exercise a plugin's own cross-process coordination, because
/// nothing dispatched from one process can. A backend whose feed order holds
/// within a process and fails across one passes this check, and the
/// obligation it breaks is the one DESIGN §3.10 puts on a plugin to publish
/// *"how it keeps every page to settled entries under concurrent writers and
/// out-of-order commits"*.
///
/// # Two rounds, and why that is not a weakening
///
/// Records are ingested first, concurrently; the corrections that withdraw
/// three of them are ingested second, concurrently. Two rounds rather than
/// one because an invalidation names a target that must already be stored,
/// and a backend is entitled to refuse one whose target it has not seen.
///
/// **The rounds are what make the correction-order assertion about the feed
/// rather than about this check's submission order.** Every correction is
/// submitted strictly after every record, so a backend that delivered its
/// feed in submission order would satisfy the assertion trivially — which is
/// exactly why the assertion is read off the *delivered* sequence and why
/// [`FEED_COMPLETENESS_CORRECTED_AT`] stamps each correction an hour before
/// its target. A backend ordering its feed by anything the corrections sort
/// *earlier* under is caught; a backend ordering it by its own commit order
/// is not.
///
/// # What is asserted
///
/// 1. **Nothing is delivered that was not written, and nothing twice.**
///    Every identity the walk delivered is one this check wrote, and none is
///    delivered more than once.
/// 2. **Nothing is missing.** Every entry this check's replicas had
///    acknowledged before the walk began is delivered by it. This is
///    completeness read off a paginated walk: an entry that is settled, in
///    the subscription and inside the read's scope, and that never arrives,
///    is an entry that became visible behind a returned cursor — or never
///    became visible at all.
/// 3. **Each correction follows the record it withdraws**, in the order the
///    pages delivered them.
/// 4. **A subscription naming the quiet meter alongside the busy one
///    delivers exactly what the busy meter alone delivers.**
/// 5. **A subscription naming only the quiet meter is served**, twice over:
///    a first read from the oldest retained position and a second from the
///    continuation the first handed back. Each must answer, deliver nothing,
///    and carry a continuation.
///
/// Assertion 5 is DESIGN's *"a subscription that stays quiet while its
/// consumer keeps reading, which is never refused"*, split into the three
/// things that phrase forbids: a refusal, an invented entry, and a page that
/// tells the consumer it has finished. The third is the one a reader is most
/// likely to pass over. [`FeedPage::next`] enumerates two dispositions —
/// *"`Some` on every page of a live read, short pages included; `None` once
/// a bounded replay has reached its `until`"* — so a live read of a quiet
/// subscription that answered `None` would be telling a consumer that a
/// stream it is still following has ended, and that consumer has no position
/// left to resume from.
///
/// # Surviving a repeated run
///
/// `super::super::run_all` is dispatched against one persistent backend that
/// writes entries and never removes them, and
/// `the_reference_backend_conforms_to_a_repeated_run` requires a second
/// dispatch to be as green as the first. This check is the most exposed
/// thing in the suite to that, because it combines the two shapes that are
/// hardest to repeat: a paginated walk, and fixtures that are ingested
/// concurrently. The four disciplines
/// [`feed_snapshot_and_replay`](super::feed_snapshot_and_replay()) states
/// are all in force here, and one of them needs restating in this check's
/// own terms.
///
/// **On a later run the concurrency is no longer ingestion.** Every identity
/// is keyed on its role, so a repeated run re-delivers the same nine
/// identities and a conforming backend absorbs all nine; the two rounds then
/// race *absorbs* rather than inserts, and nothing new settles. That is
/// designed for rather than tolerated: **no assertion here is about the
/// ingestion being fresh.** Each is a statement about the ledger the
/// acknowledgements describe — every acknowledged entry delivered once, each
/// correction after its target, the quiet subscription served — and all
/// three are as true of a ledger that settled on the first run as of one
/// settling now. What a repeated run costs is the *strength* of the
/// evidence, not its validity: run two exercises the absorb path under
/// concurrency and run one exercises the insert path, and it is run one that
/// establishes the rule.
///
/// Nothing here names a ledger length, a page count, a position value, or
/// where in the delivered order an entry falls. Correction order is the
/// closest to an exception and is not one: it fixes two entries' order
/// *relative to each other* and says nothing about where either sits, which
/// is a property of the backend's own feed order and therefore the same on
/// every run.
///
/// # Which assertions a subject reaches, measured
///
/// Every assertion below was inverted and confirmed to fire against the
/// reference backend, so none of the seven is dead. What each *catches* was
/// then measured by neutering it and running the discrimination matrix, and
/// the result bounds what this check's matrix rows establish. Four subjects
/// reach this check, all four in `contract_mutants`:
///
/// * **`Defect::AFeedPageDropsTheEntryAtItsLimit`** — the subject built for
///   this check's own rule — is reported by assertion 2 **alone**.
///   Neutering it leaves that subject's row empty, so that assertion is
///   individually load-bearing.
/// * **`Defect::FeedOrdersByTheAcceptanceInstant`** is reported by assertion
///   3 **alone**, and neutering it leaves that subject's row empty too. It
///   is the only subject in the suite that reaches a correction-order
///   assertion, and this is the only correction-order assertion there is.
/// * **`Defect::AFeedPageRedeliversTheEntryAtItsCursor`** is reported by
///   assertion 1, and by `feed-snapshot-and-replay` as well. **Assertion 1
///   is therefore not individually load-bearing**: neutering it narrows that
///   subject's row to the other check rather than emptying it. It stays
///   because an entry delivered twice and an entry appearing where a scan
///   had already been are two things to tell a plugin author about one
///   backend, and because the overlap is real rather than a subject wrong
///   twice — `contract_tests`' `DISCRIMINATION_MATRIX` carries that
///   argument.
/// * **`Defect::DedupIgnoresTheEntryType`** refuses every correction this
///   check submits, so the second ingestion round reports and the check
///   stops before any assertion below runs. **No assertion here reaches it**,
///   which is why neutering all seven leaves that row unchanged. The overlap
///   is DESIGN's and unavoidable — every fixture set that writes an
///   invalidation at all hands an entry-type-blind index a collision — and
///   the matrix carries the argument for the four checks that were in that
///   row before this one joined it.
///
/// # Four assertions no subject reaches, and why each stays
///
/// Neutering any of these four changed no row of the matrix, which is the
/// measurement rather than an expectation.
///
/// * **Assertion 4.** A subject that reached it would have to answer
///   differently for two subscriptions that admit the same entries, which is
///   a backend reading its subscription's *shape* rather than evaluating it.
///   The shape-reading defect that *is* plausible — a page limit applied
///   once per subscribed meter and the results merged — needs a second
///   meter with entries in it, and this check's second meter is empty by
///   definition of the clause it exists for. **A recorded gap with that
///   reasoning.**
/// * **Assertion 5's three parts** — refused, invented, closed — had an
///   owner waiting rather than a missing subject, and **the owner has
///   landed**.
///   [`FEED_BOOTSTRAP_POSITION`](crate::contract::FEED_BOOTSTRAP_POSITION)'s
///   row is *"A subscription retaining no entries returns an empty page
///   carrying a head cursor"*, which is those same three obligations read
///   off the `FeedStart::Oldest` read, and that check now asserts them over
///   a meter of its own. What it did not bring is a subject: its three
///   subjects were measured against these three assertions and none reaches
///   them, so **the gaps are still open and they are now recorded in two
///   places rather than one**. The reasoning is that check's to carry, and
///   [`feed_bootstrap_position`](super::feed_bootstrap_position()) carries
///   it, including the one candidate subject that is named rather than
///   built.
///
///   What the landing moved is the **ownership of the rule**, not the
///   reads. The `Oldest` read below stays because a consumer has to get its
///   first position from somewhere; it is a means here and the rule there.
///   What is this check's own is the **second** read, from the continuation
///   the first handed back, which is the only one of the two that is about
///   a consumer that keeps reading.
/// * Assertion 5b has a second reason to be a gap rather than a subject: a
///   feed that ignored its subscription and answered every meter's entries
///   is already pinned, by `feed-snapshot-and-replay`, whose walk would be
///   handed everything every other check left on the suite's shared meter. A
///   subject here would be a second one for a site that has one, and it
///   would widen that check's row.
///
/// # What this check does not reach
///
/// It writes under one tenant and reads under a grant that admits it, so
/// *"Completeness binds the scope each read ran under"* is not exercised
/// here: no entry of this check is ever withheld from a read.
/// `feed-snapshot-and-replay` is the check whose ledger alternates an
/// admitted tenant with a withheld one, and it is where a cursor's meaning
/// under a grant is asserted.
///
/// It dispatches no bounded replay, so [`FeedPage::next`]'s second
/// disposition is asserted here only in the negative — that a live page
/// never takes it.
pub async fn feed_completeness(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    let fixtures = match feed_completeness_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own `{FEED_COMPLETENESS}` fixtures, \
                     so nothing was submitted. This is a fault in the suite, not in the plugin \
                     under test: {detail}"
                ),
            )];
        }
    };

    // A refusal in either round stops the check. Every probe below is a
    // statement about a feed over entries that are there; over an empty
    // subscription they all hold vacuously, and a check that passes because
    // nothing was stored is the one outcome worse than a failing one.
    let records: Vec<UsageRecord> = fixtures.records.clone();
    let mut violations = ingest_through_replicas(plugin, &records, "record").await;
    if !violations.is_empty() {
        return violations;
    }
    let corrections: Vec<UsageRecord> = fixtures
        .corrections
        .iter()
        .map(|(_, entry)| entry.clone())
        .collect();
    violations.extend(ingest_through_replicas(plugin, &corrections, "correction").await);
    if !violations.is_empty() {
        return violations;
    }

    let busy = [fixtures.busy.clone()];
    let walk = match feed_walk(
        plugin,
        &busy,
        &contract_scope(),
        FeedStart::Oldest,
        FEED_COMPLETENESS_PAGE_LIMIT,
        WalkStop::AtTheHead,
    )
    .await
    {
        Ok(walk) => walk,
        Err(detail) => return vec![violation(FEED_COMPLETENESS, detail)],
    };

    violations.extend(the_feed_delivered_what_was_acknowledged(
        &walk.delivered,
        &fixtures,
    ));
    violations.extend(corrections_follow_their_targets(&walk.delivered, &fixtures));
    violations
        .extend(a_quiet_meter_changes_nothing_and_is_never_refused(plugin, &fixtures, &walk).await);
    violations
}

/// Assertions one and two: the walk delivered every acknowledged entry, once
/// each, and nothing else.
///
/// Two reports rather than one, because they are two things to tell a plugin
/// author. An entry delivered twice is a consumer charging one measurement
/// twice; an entry never delivered is a consumer charging it not at all, and
/// the second is the failure completeness exists to forbid — *"no entry the
/// read's compiled scope admits ever becomes visible behind a returned
/// cursor, whatever the concurrency or commit order"*.
fn the_feed_delivered_what_was_acknowledged(
    delivered: &[UsageRecord],
    fixtures: &FeedCompletenessFixtures,
) -> Vec<ContractViolation> {
    let mut violations = Vec::new();
    let seen = ids(delivered);
    let acknowledged: Vec<Uuid> = fixtures
        .acknowledged()
        .into_iter()
        .map(|entry| entry.id)
        .collect();

    let strays: Vec<Uuid> = seen
        .iter()
        .filter(|id| !acknowledged.contains(id))
        .copied()
        .collect();
    let repeats: Vec<Uuid> = acknowledged
        .iter()
        .filter(|id| seen.iter().filter(|other| *other == *id).count() > 1)
        .copied()
        .collect();
    if !strays.is_empty() || !repeats.is_empty() {
        violations.push(violation(
            FEED_COMPLETENESS,
            format!(
                "a paginated walk over this check's own meter, taken after every entry had been \
                 acknowledged, delivered {seen:?}. Delivered but written by nothing here, or \
                 attributed to a tenant this read's scope does not admit: {strays:?}. Delivered \
                 more than once: {repeats:?}. One deterministic order over a subscription hands \
                 each settled entry to a consumer once: a repeat is a page resuming at the entry \
                 its own start position names rather than after it, and a consumer following \
                 that cursor rates the same measurement twice."
            ),
        ));
    }

    let missing: Vec<Uuid> = acknowledged
        .iter()
        .filter(|id| !seen.contains(id))
        .copied()
        .collect();
    if !missing.is_empty() {
        violations.push(violation(
            FEED_COMPLETENESS,
            format!(
                "{missing:?} were acknowledged by this check's concurrent callers and a \
                 paginated walk from the oldest retained position to the settled head did not \
                 deliver them. A page carries only settled entries, so no entry the read's \
                 compiled scope admits ever becomes visible behind a returned cursor, whatever \
                 the concurrency or commit order. An entry that is settled, inside the \
                 subscription and inside the scope, and that no page ever carries, is behind a \
                 cursor its consumer has already been handed: nothing will deliver it again, and \
                 the usage it records is charged to nobody. This is the failure a feed-order \
                 column assigned before commit produces - a writer that takes its position \
                 first and commits second is passed over by a reader that runs in between, and \
                 the reader's cursor moves past it. It delivered {seen:?}."
            ),
        ));
    }

    violations
}

/// Assertion three: each correction is delivered after the record it
/// withdraws.
///
/// *"**Correction order**: an invalidation follows its target."* The
/// comparison is on the **delivered** sequence rather than on the order this
/// check submitted in, which is the whole point of reading it off a walk:
/// the submission order already puts every correction last, so a check
/// asserting its own order would assert nothing.
///
/// A pair either of whose halves is missing is passed over. The assertion
/// above already reports it, and reporting it again would read as two
/// defects rather than one. A pair delivered more than once is compared on
/// its **first** delivery, for the same reason: a repeat is that assertion's
/// business, and a correction that follows its target's first delivery has
/// met this clause.
fn corrections_follow_their_targets(
    delivered: &[UsageRecord],
    fixtures: &FeedCompletenessFixtures,
) -> Vec<ContractViolation> {
    let seen = ids(delivered);
    let mut violations = Vec::new();
    for (target, correction) in fixtures.pairs() {
        let (Some(at_target), Some(at_correction)) = (
            seen.iter().position(|id| *id == target.id),
            seen.iter().position(|id| *id == correction.id),
        ) else {
            continue;
        };
        if at_correction > at_target {
            continue;
        }
        violations.push(violation(
            FEED_COMPLETENESS,
            format!(
                "a feed read over this check's own meter delivered the invalidation {correction} \
                 at position {at_correction} and the record {target} it withdraws at position \
                 {at_target}, so a consumer met the correction before the thing it corrects. An \
                 invalidation follows its target. The two were submitted in separate rounds, \
                 every record before every correction, so nothing about this check's own order \
                 put them this way round: what did is the order the feed realises. The \
                 correction carries the earlier acceptance instant of the two, which is the skew \
                 DESIGN names - feed order must not rest on a gateway-stamped `accepted_at`, \
                 because replica clock skew can stamp an invalidation earlier than its target. A \
                 consumer handed them in this order withdraws a charge it has not made, and then \
                 makes it.",
                correction = correction.id,
                target = target.id,
            ),
        ));
    }
    violations
}

/// Assertions four and five: the quiet meter changes nothing when it is
/// named alongside the busy one, and is served when it is named alone.
///
/// The wide walk is dispatched at [`FEED_COMPLETENESS_PAGE_LIMIT`], the same
/// limit the busy walk ran at, so the comparison is about the subscription
/// and not about where a page boundary fell.
async fn a_quiet_meter_changes_nothing_and_is_never_refused(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FeedCompletenessFixtures,
    busy: &FeedWalk,
) -> Vec<ContractViolation> {
    let both = [fixtures.busy.clone(), fixtures.quiet.clone()];
    let mut violations = match feed_walk(
        plugin,
        &both,
        &contract_scope(),
        FeedStart::Oldest,
        FEED_COMPLETENESS_PAGE_LIMIT,
        WalkStop::AtTheHead,
    )
    .await
    {
        Ok(wide) => the_quiet_meter_added_nothing(&busy.delivered, &wide.delivered),
        Err(detail) => vec![violation(FEED_COMPLETENESS, detail)],
    };
    violations.extend(the_quiet_subscription_keeps_its_consumers_place(plugin, fixtures).await);
    violations
}

/// Assertion four: naming a meter nothing was ever written to does not
/// change what the feed carries.
fn the_quiet_meter_added_nothing(
    busy: &[UsageRecord],
    wide: &[UsageRecord],
) -> Vec<ContractViolation> {
    let Some(divergence) = first_divergence(busy, wide) else {
        return Vec::new();
    };
    vec![violation(
        FEED_COMPLETENESS,
        format!(
            "a subscription naming this check's busy meter and a second meter nothing has ever \
             been written to changed what the feed carried: {divergence}. Both reads were taken \
             at one page limit with nothing submitted in between, so a page boundary cannot \
             account for it. A meter with nothing under it contributes nothing to a \
             subscription's order, and a feed whose answer moves when one is named is answering \
             the subscription's shape rather than the ledger."
        ),
    )]
}

/// Assertion five: a subscription naming only the quiet meter answers,
/// carries nothing, and hands back a position — on the first read and again
/// on the read that resumes from it.
///
/// **The second read is the one the row is about.** *"A subscription that
/// stays quiet while its consumer keeps reading"* describes a consumer that
/// already holds a position and comes back to it, which is `FeedStart::After`
/// rather than `FeedStart::Oldest`. The first read is here because a
/// consumer has to get its first position from somewhere, and
/// [`FEED_BOOTSTRAP_POSITION`](crate::contract::FEED_BOOTSTRAP_POSITION)
/// has landed and owns that read's rule: DESIGN gives it *"A subscription
/// retaining no entries returns an empty page carrying a head cursor"*, and
/// [`feed_bootstrap_position`](super::feed_bootstrap_position()) asserts it
/// over a meter of its own.
///
/// **The read stays here, and the three reports on it stay with it.** They
/// are not a second claim on that rule; they are what says *why the second
/// read was not taken* when the first one fails, and a probe that dropped
/// them would report a quiet subscription as unexamined rather than as
/// refused. A backend that breaks the rule is reported by both checks, which
/// is right: it has broken a consumer's bootstrap and a consumer's
/// resumption, and those are two consumers.
async fn the_quiet_subscription_keeps_its_consumers_place(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FeedCompletenessFixtures,
) -> Vec<ContractViolation> {
    let quiet = [fixtures.quiet.clone()];
    let scope = contract_scope();
    let first = plugin
        .read_feed_page(
            &quiet,
            &scope,
            FeedStart::Oldest,
            None,
            FEED_COMPLETENESS_PAGE_LIMIT,
        )
        .await;
    let mut violations = a_quiet_page_was_served(&first, "from the oldest retained position");

    let Ok(FeedPage {
        next: Some(position),
        ..
    }) = first
    else {
        return violations;
    };
    let again = plugin
        .read_feed_page(
            &quiet,
            &scope,
            FeedStart::After(position),
            None,
            FEED_COMPLETENESS_PAGE_LIMIT,
        )
        .await;
    violations.extend(a_quiet_page_was_served(
        &again,
        "from the position that read had just handed back",
    ));
    violations
}

/// The three things one read of a quiet subscription must be: answered,
/// empty, and resumable.
///
/// One helper over two reads rather than six assertions written out, because
/// the three obligations are the same ones both times and a report that said
/// them differently would read as three rules rather than one applied twice.
/// `role` is what tells the two reads apart in a report.
fn a_quiet_page_was_served(
    outcome: &Result<FeedPage<FeedPosition>, UsageCollectorPluginError>,
    role: &str,
) -> Vec<ContractViolation> {
    let page = match outcome {
        Ok(page) => page,
        Err(err) => {
            return vec![violation(
                FEED_COMPLETENESS,
                format!(
                    "a feed read {role} over a subscription naming only this check's quiet \
                     meter, which nothing has ever been written to, was refused: {err}. A \
                     subscription that stays quiet while its consumer keeps reading is never \
                     refused. A meter goes quiet whenever the resource it measures is idle, and \
                     a consumer whose reads start failing when that happens has no way to tell a \
                     quiet meter from a broken feed."
                ),
            )];
        }
    };

    let mut violations = Vec::new();
    if !page.entries.is_empty() {
        violations.push(violation(
            FEED_COMPLETENESS,
            format!(
                "a feed read {role} over a subscription naming only this check's quiet meter \
                 delivered {delivered:?}. Nothing has ever been written to that meter, and a \
                 feed page carries the entries of the subscription it was asked for: an entry \
                 here is one this consumer is not subscribed to and would rate anyway.",
                delivered = ids(&page.entries),
            ),
        ));
    }
    if page.next.is_none() {
        violations.push(violation(
            FEED_COMPLETENESS,
            format!(
                "a feed read {role} over a subscription naming only this check's quiet meter \
                 carried no continuation. A live read carries one on every page, short and empty \
                 pages included; an absent one is reserved for a bounded replay reaching its \
                 `until`, and this read was unbounded. A consumer whose meter goes quiet keeps \
                 its place rather than being told the stream has ended: a page with no `next` \
                 leaves it holding no position at all, so when the meter comes back it either \
                 starts again from the oldest retained entry and re-rates everything, or gives \
                 up on the subscription."
            ),
        ));
    }
    violations
}

/// Submits `entries` through [`FEED_COMPLETENESS_REPLICAS`] concurrent
/// callers, reporting any refusal.
///
/// **The entries are dealt round-robin rather than cut into blocks**, so
/// adjacent entries go to different callers. A block split would have one
/// caller submit a contiguous run, which is the interleaving a single caller
/// already produces; dealing them means every pair of neighbours in this
/// check's own order is a pair two callers raced to store.
///
/// A refusal is reported against the check rather than as a harness fault:
/// the submission is well formed, and a backend that will not accept it has
/// broken something this check cannot then go on to observe. `role` names
/// the round in the report, because a refused correction and a refused
/// record are different failures.
async fn ingest_through_replicas(
    plugin: &dyn UsageCollectorPluginV1,
    entries: &[UsageRecord],
    role: &str,
) -> Vec<ContractViolation> {
    let mut dealt: [Vec<UsageRecord>; FEED_COMPLETENESS_REPLICAS] =
        [Vec::new(), Vec::new(), Vec::new()];
    for (index, entry) in entries.iter().enumerate() {
        if let Some(replica) = dealt.get_mut(index % FEED_COMPLETENESS_REPLICAS) {
            replica.push(entry.clone());
        }
    }
    let [first, second, third] = dealt;

    let (first, second, third) = toolkit::tokio::join!(
        one_replica(plugin, first, role),
        one_replica(plugin, second, role),
        one_replica(plugin, third, role),
    );
    let mut violations = first;
    violations.extend(second);
    violations.extend(third);
    violations
}

/// One caller's share of a round, submitted in order.
///
/// Every entry is submitted whatever the ones before it did, so a round
/// whose first entry is refused still delivers the rest: the probes below
/// read a ledger all nine entries belong to, and a round that stopped at its
/// first refusal would leave a shorter ledger than the report describes.
async fn one_replica(
    plugin: &dyn UsageCollectorPluginV1,
    entries: Vec<UsageRecord>,
    role: &str,
) -> Vec<ContractViolation> {
    let mut violations = Vec::new();
    for entry in entries {
        if let Err(err) = plugin.create_usage_record(entry.clone()).await {
            violations.push(violation(
                FEED_COMPLETENESS,
                format!(
                    "`create_usage_record` refused a {role} entry (entry {id}, tenant {tenant}) \
                     dispatched alongside this check's other concurrent callers, so the feed \
                     this check reads had nothing to deliver at that point in the ledger: {err}",
                    id = entry.id,
                    tenant = entry.tenant_id,
                ),
            ));
        }
    }
    violations
}

/// Builds the six records, the three corrections that withdraw three of
/// them, and the two meters.
///
/// The busy meter is this check's own, which is the separation that matters
/// here: a feed read selects by subscription, so an entry of this check's on
/// the suite's shared meter would be an entry some *other* check's page has
/// to account for, and an entry of some other check's on this meter would be
/// one this check's walk delivers and cannot name.
///
/// **Three of the six records are withdrawn rather than all of them**, so
/// the delivered order holds records with a correction and records without,
/// and a backend that moved *every* invalidation would still be moving them
/// past records that stay put. The three are taken at even indices, so the
/// records the corrections name are dealt to different replicas: with
/// [`FEED_COMPLETENESS_REPLICAS`] callers and a round-robin deal, indices 0,
/// 2 and 4 land on callers 0, 2 and 1.
///
/// Four guards keep the check from passing by construction, and all four are
/// the suite's own facts rather than the plugin's, so all four are reported
/// as [`HARNESS_FAULT`]. They are [`FeedCompletenessFixtures::guards`]'.
fn feed_completeness_fixtures() -> Result<FeedCompletenessFixtures, String> {
    let busy = check_meter(FEED_COMPLETENESS, "busy")?;
    let quiet = check_meter(FEED_COMPLETENESS, "quiet")?;
    let quantity = UsageQuantity::parse(FEED_COMPLETENESS_QUANTITY).map_err(|err| {
        format!("the check's own quantity `{FEED_COMPLETENESS_QUANTITY}` does not parse: {err}")
    })?;

    let mut records = Vec::with_capacity(FEED_COMPLETENESS_RECORDS);
    for index in 0..FEED_COMPLETENESS_RECORDS {
        records.push(fixture_record_on(
            busy.clone(),
            CONTRACT_TENANT_ID,
            &feed_completeness_key(&format!("record-{index}"))?,
            quantity,
            CONTRACT_ACCEPTED_AT,
            FEED_COMPLETENESS_WINDOW_FROM,
            FEED_COMPLETENESS_WINDOW_END,
        )?);
    }

    let mut corrections = Vec::new();
    for index in (0..FEED_COMPLETENESS_RECORDS).step_by(2) {
        let target = records
            .get(index)
            .ok_or_else(|| format!("the check built no record at index {index} to withdraw"))?;
        corrections.push((
            index,
            fixture_invalidation_on(
                target,
                FEED_COMPLETENESS_REASON,
                FEED_COMPLETENESS_CORRECTED_AT,
            )?,
        ));
    }

    let fixtures = FeedCompletenessFixtures {
        busy,
        quiet,
        records,
        corrections,
    };
    fixtures.guards()?;
    Ok(fixtures)
}

/// The reason code every correction this check submits states.
///
/// Its own rather than the shared one for no reason of substance — the
/// vocabulary is open and no check reads a reason — but a code naming this
/// check makes a stray entry in another check's report traceable to here.
const FEED_COMPLETENESS_REASON: &str = "feed-completeness-withdrawal";

impl FeedCompletenessFixtures {
    /// The four facts this check's assertions read, established rather than
    /// assumed.
    ///
    /// * **The two meters are two.** A subscription naming one meter twice
    ///   would make the quiet probe a second reading of the busy one, and
    ///   assertion 5 would require the busy meter to deliver nothing.
    /// * **The nine entries derive nine distinct ids.** They do — the
    ///   idempotency key and the entry type are two of the six identity
    ///   inputs — but if two ever collided, one submission would be absorbed
    ///   as a retry of the other and this check would assert that a feed
    ///   delivers an entry that was never separately stored.
    /// * **Every correction names the record it is paired with.**
    ///   `invalidates` is server-assigned, stamped here the way the gateway
    ///   stamps what it resolved, and the correction-order assertion is
    ///   about a correction and *its* target: a pair that named some other
    ///   entry would assert an order between two entries with no relation.
    /// * **Every correction carries an acceptance instant strictly earlier
    ///   than its target's.** This is the skew DESIGN §3.10 names, and
    ///   without it assertion 3 is satisfied by a backend whose feed is
    ///   ordered by the gateway-stamped instant — which is the ordering
    ///   §3.1 denies is claimed and §3.10 forbids outright.
    fn guards(&self) -> Result<(), String> {
        if self.busy == self.quiet {
            return Err(format!(
                "this check's busy and quiet meters are one value ({busy}), so the subscription \
                 it never writes to is the one it wrote nine entries to and the quiet probe \
                 would require those entries not to be delivered",
                busy = self.busy.as_str(),
            ));
        }
        let entries = self.acknowledged();
        for (index, entry) in entries.iter().enumerate() {
            if entries[..index].iter().any(|other| other.id == entry.id) {
                return Err(format!(
                    "two of this check's nine entries derive one id ({id}); the second \
                     submission would be absorbed as a retry of the first, and the walk would be \
                     asserted to deliver an entry that was never separately stored",
                    id = entry.id,
                ));
            }
        }
        for (target, correction) in self.pairs() {
            if correction.invalidation.as_ref().map(|it| it.target) != Some(target.id) {
                return Err(format!(
                    "the correction {correction} is paired with the record {target} and does not \
                     name it in `invalidates`, so the correction-order assertion would be about \
                     two entries with no relation to each other",
                    correction = correction.id,
                    target = target.id,
                ));
            }
            if correction.accepted_at >= target.accepted_at {
                return Err(format!(
                    "the correction {correction} carries the acceptance instant \
                     `{corrected_at}` and the record {target} it withdraws carries \
                     `{accepted_at}`, so the correction is not stamped earlier than its target. \
                     DESIGN section 3.10 names that skew as the reason feed order must not rest \
                     on a gateway-stamped `accepted_at`, and without it a backend ordering its \
                     feed by that field satisfies the correction-order assertion by accident",
                    correction = correction.id,
                    corrected_at = correction.accepted_at,
                    target = target.id,
                    accepted_at = target.accepted_at,
                ));
            }
        }
        Ok(())
    }
}

/// The idempotency key one of this check's records submits under.
///
/// Keyed on the role name for the reason
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture) spells
/// out: in a ledger with no delete path, an edited fixture must take a fresh
/// identity rather than inherit an accepted entry's. It is also what makes a
/// repeated run re-deliver these identities rather than mint more, which is
/// what keeps this check's ledger the same length on every run.
///
/// The corrections take no key of their own: a faithful invalidation repeats
/// its target's idempotency key (DESIGN §3.1, Faithful copy) and is a
/// separate identity by its `entry_type` alone.
fn feed_completeness_key(role: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{FEED_COMPLETENESS}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}
