//! The DESIGN §3.3 `feed-snapshot-and-replay` check.
//!
//! See [`feed_snapshot_and_replay`] for what it asserts. The module holds
//! the eight entries it writes — four before the scan begins and two at each
//! of the two moments a clause of the row needs something to have settled
//! since — and the three grants it reads them under. The walk itself is
//! [`super::super::feed_walk`]'s: it was written here, for this check, and
//! moved out when `feed-completeness` became its second caller.
//!
//! **This is the first check in the suite to read the feed as a paginated
//! walk**, and the first whose fixtures are written *during* a read rather
//! than before it. Both make it the check most exposed to
//! `super::super::run_all` being dispatched twice at one backend, which
//! `the_reference_backend_conforms_to_a_repeated_run` requires. See
//! [`feed_snapshot_and_replay`]'s "Surviving a repeated run" for how every
//! assertion here is written to be true of the first run and of the
//! thousandth.

use toolkit_odata::ast;
use uuid::Uuid;

use crate::contract::feed_walk::{
    FeedWalk, WalkStop, bounded_replay, feed_walk, first_divergence, ids,
};
use crate::contract::fixtures::{
    CONTRACT_ACCEPTED_AT, CONTRACT_TENANT_ID, SCOPE_EXCLUDED_TENANT_ID, SCOPE_UNUSED_TENANT_ID,
    check_meter, check_window_from, contract_tenant, fixture_record_on, tenant_disjunction,
    violation,
};
use crate::contract::{ContractViolation, FEED_SNAPSHOT_AND_REPLAY, HARNESS_FAULT};
use crate::feed::FeedStart;
use crate::models::{IdempotencyKey, MeterTypeId, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;

/// The start of the covered period every entry of this check carries.
///
/// The offset [`check_window_from`] tables for this check, for the reason
/// that accessor gives.
///
/// It buys as little here as it does for
/// [`converged_target_lookup`](super::converged_target_lookup()), and for
/// the same reason: **this check dispatches no range at all.** Every read it
/// makes is a feed page, and a feed page selects by subscription rather than
/// by covered period, so the separation that actually does the work is
/// [`check_meter`]. The offset is kept because the separation runs the
/// *other* way as well: `super::super::run_all` dispatches every check
/// against one backend that never removes an entry, so these eight entries
/// are there for every check that *does* read a range, and a period no other
/// check's range covers is what keeps them out of those reads.
const FEED_SNAPSHOT_WINDOW_FROM: time::OffsetDateTime =
    check_window_from(FEED_SNAPSHOT_AND_REPLAY, "main");

/// The end of that covered period. All eight entries carry it: they are
/// separated by their tenant and their idempotency key, and nothing here
/// asks a covered period to tell them apart.
const FEED_SNAPSHOT_WINDOW_END: time::OffsetDateTime =
    FEED_SNAPSHOT_WINDOW_FROM.saturating_add(time::Duration::hours(1));

/// The quantity every entry of this check carries.
///
/// Its value is asserted nowhere: the assertions compare entries this
/// backend answered against other entries this backend answered, or compare
/// identities. It is exactly representable in a binary float all the same,
/// which keeps `contract_mutants`'s `Defect::QuantityThroughFloat` from
/// changing anything on the way in and so from reaching this check for a
/// rule that belongs to `quantity-round-trip`.
const FEED_SNAPSHOT_QUANTITY: &str = "1.5";

/// How many entries this check seeds before the scan begins.
///
/// Four, alternating the tenant the walking grant admits with the tenant it
/// withholds. Two of each is the smallest ledger in which a withheld entry
/// falls *between* two admitted ones, which is what the alternation is for.
const FEED_SNAPSHOT_SEEDS: usize = 4;

/// The page limit the paginated walk dispatches: **one**.
///
/// `limit` bounds the entries a page *carries*, not the entries it scans, so
/// a limit of two over this check's eight-entry ledger with four admitted
/// would deliver everything the grant admits in a single page and there
/// would be no walk to observe. One is the only limit that guarantees more
/// than one page here whatever the backend.
const FEED_SNAPSHOT_WALK_LIMIT: u64 = 1;

/// The page limit every read that is not the paginated walk dispatches:
/// twice the eight entries this check writes.
///
/// The margin is the assertion rather than an optimisation. These reads are
/// still *followed* — a short page is conforming and this check never
/// assumes otherwise — and the margin is what stops a page from being
/// truncated exactly where a comparison would then report the missing
/// entries as a plugin losing them.
const FEED_SNAPSHOT_PAGE_LIMIT: u64 = 16;

/// The first of the two tenants this check names in a grant and writes
/// nothing under.
///
/// [`contract_tenant`] mints from a block held clear of the suite's four
/// named tenant ids;
/// [`latest_tie_break`](super::latest_tie_break()) already holds indices 0
/// and 1, so this check takes the next two. The premise they carry is only
/// that they own no entry **on this check's meter**, which they cannot,
/// because nothing in the suite writes to a meter [`check_meter`] derived
/// for another check's exclusive use.
const FEED_SNAPSHOT_FIRST_UNWRITTEN_TENANT: u32 = 2;

/// The second of the two, one past the first.
const FEED_SNAPSHOT_SECOND_UNWRITTEN_TENANT: u32 = FEED_SNAPSHOT_FIRST_UNWRITTEN_TENANT + 1;

/// The eight entries this check writes and the meter it reads them over.
struct FeedSnapshotFixtures {
    /// The meter every entry is written to and the only meter any read here
    /// subscribes to.
    meter: MeterTypeId,
    /// The four entries written before the scan begins, in the order they
    /// are submitted, alternating the admitted tenant with the withheld one.
    seeded: Vec<UsageRecord>,
    /// The entry submitted while the scan is paused, under the tenant the
    /// walking grant admits. This is the arrival the row's first clause
    /// excepts: it settles ahead of the cursor and the scan must still
    /// deliver it.
    arrival: UsageRecord,
    /// Submitted with [`Self::arrival`] and under the tenant the walking
    /// grant withholds, so that the alternation holds through the arrivals
    /// as well as through the seeds.
    arrival_withheld: UsageRecord,
    /// The entry submitted between this check's two replays from one cursor,
    /// under the admitted tenant: the whole of what the second replay is
    /// allowed to have been extended by.
    settled_since: UsageRecord,
    /// Submitted with [`Self::settled_since`] and under the withheld tenant,
    /// so the **last** entry on this check's meter is one the walking grant
    /// does not carry. See
    /// [`a_wider_grant_resumes_a_narrower_walk_at_the_head`] for what rests
    /// on that.
    settled_since_withheld: UsageRecord,
}

impl FeedSnapshotFixtures {
    /// The identities the walking grant admits, among everything this check
    /// writes.
    fn admitted_ids(&self) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = self
            .seeded
            .iter()
            .filter(|record| record.tenant_id == CONTRACT_TENANT_ID)
            .map(|record| record.id)
            .collect();
        ids.push(self.arrival.id);
        ids.push(self.settled_since.id);
        ids
    }

    /// Every entry this check writes, in submission order.
    fn all(&self) -> Vec<&UsageRecord> {
        let mut records: Vec<&UsageRecord> = self.seeded.iter().collect();
        records.push(&self.arrival);
        records.push(&self.arrival_withheld);
        records.push(&self.settled_since);
        records.push(&self.settled_since_withheld);
        records
    }
}

/// `feed-snapshot-and-replay` — *"A paginated scan observes no entry
/// appearing, disappearing, or changing, except arrivals ahead of the
/// cursor. Replay from one cursor yields the same entries in the same order,
/// extended only by entries settled since, and replay bounded by `until` is
/// identical — including over a subscription spanning many tenants, some
/// never written."* (DESIGN §3.3, "Plugin contract tests", line 1322.)
///
/// **The row's four clauses each get a probe, and each probe reports on its
/// own** so a plugin author fixing one is not thereby told about the others.
///
/// **The rule the row rests on is §3.1's "Feed order" invariant**, which is
/// where "no entry appearing behind the cursor" is actually stated: *"One
/// deterministic order over a subscription, realised by the plugin through
/// `FeedPosition`. No other ordering is claimed, acceptance-instant order
/// included. **Completeness**: a page carries only settled entries —
/// converged, with nothing more able to become visible before them — so no
/// entry the read's compiled scope admits ever becomes visible behind a
/// returned cursor, whatever the concurrency or commit order. Completeness
/// binds the scope each read ran under; what a changed scope admits or
/// excludes is the §3.3 cursor contract, not an ordering matter. A page
/// reaching the settled head returns its cursor at the head, so a cursor's
/// age reflects its consumer's progress, not the last arrival. **Correction
/// order**: an invalidation follows its target."*
///
/// The SPI restates the head of it for implementors on
/// [`read_feed_page`](UsageCollectorPluginV1::read_feed_page): *"Snapshot-consistent
/// feed page in feed order (§3.1)."*
///
/// # What each probe does
///
/// 1. [`a_scan_observes_no_change_but_arrivals_ahead_of_the_cursor`] walks
///    at `limit = 1`, pauses once something has been delivered, submits two
///    entries, and walks on to the head.
/// 2. [`a_replay_from_one_cursor_repeats_and_only_extends`] replays from one
///    position twice, with two entries settling in between.
/// 3. [`a_bounded_replay_is_identical`] reads the whole span live and then
///    again bounded by the position the live read reached.
/// 4. [`a_grant_that_names_more_tenants_changes_nothing`] reads it once more
///    under a grant naming three further tenants, two of which no entry in
///    this suite is attributed to and one of which owns none here.
///
/// # Positions are compared by size and by behaviour, never by value
///
/// A position's guarantee is **resumability, not identity**. `limit` bounds
/// the entries a page *admits* while the scan is unbounded, so two callers
/// whose grants differ receive the same position only when neither read was
/// limit-bounded; `contract_tests`'
/// `a_bounded_page_limit_hands_two_grants_two_positions_that_each_resume`
/// is the read where they differ, and it differs by design. Nothing here
/// compares two grants' positions for equality. Probe 4 compares
/// [`FeedPosition::len`](crate::feed::FeedPosition::len) — the figure
/// DESIGN's `feed-position-bounded` row is stated over, though that row is
/// stated over the breadth of a **subscription** and this probe varies the
/// **grant**, so the two are neighbours rather than one claim;
/// [`feed_position_bounded`](super::feed_position_bounded()) is the check
/// whose row that is — and then compares what each position *does*, which
/// is the property that holds whatever the limit.
///
/// # The ledger alternates its two tenants
///
/// Eight entries, admitted and withheld in turn. Interleaving is
/// load-bearing rather than decorative, and it is the same argument
/// `contract_tests`' `feed_ledger` makes: a backend that counted admitted
/// entries instead of scanned ones would still walk a ledger whose entries
/// were all admitted, and would still finish one whose withheld entries were
/// all at the end. Alternating them means a page's cursor has to jump past
/// an entry the page did not carry, every page, and it means the **last**
/// entry on this check's meter is one the walking grant withholds — which is
/// what [`a_wider_grant_resumes_a_narrower_walk_at_the_head`] turns on.
///
/// # Surviving a repeated run
///
/// `super::super::run_all` is dispatched against one persistent backend that
/// writes entries and never removes them, and
/// `the_reference_backend_conforms_to_a_repeated_run` requires a second
/// dispatch to be as green as the first. A paginated walk is the most
/// exposed thing in the suite to that, because on the second run every entry
/// this check "submits mid-scan" is already settled before the scan starts.
/// Four disciplines make every assertion below true of both runs:
///
/// * **The identities are fixed.** Every entry is keyed on its role, so a
///   repeated run resubmits the same eight identities and a conforming
///   backend absorbs all eight. The ledger this check reads is the same
///   eight entries on every run.
/// * **No assertion names a ledger length, a page count or a position
///   value.** The walk follows the cursor to a fixpoint and the budget
///   bounds the loop rather than predicting it.
/// * **No assertion names *where* in the delivered order an entry falls.**
///   On the first run an arrival cannot precede the entries already
///   scanned; on a later run it is settled from the start and a backend's
///   own order may put it anywhere. What is asserted is that it is
///   delivered, that nothing is delivered twice, and that a second read of
///   the same span agrees with the first entry for entry.
/// * **"Extended only by entries settled since" is asserted as a subset.**
///   On the first run the extension is exactly the entry that settled; on a
///   later run that entry was already there and the extension is empty.
///   Both satisfy *"only by"*.
///
/// # Which assertions a subject reaches, measured
///
/// Every assertion below was inverted and confirmed to fire against the
/// reference backend, so none of them is dead. What each *catches* was then
/// measured by neutering it and running the discrimination matrix, and the
/// result bounds what this check's matrix rows establish. Three subjects
/// exercise it, all three in `contract_mutants`:
///
/// * **`Defect::FeedCursorCountsAdmittedEntries`** is reported by
///   [`a_wider_grant_resumes_a_narrower_walk_at_the_head`] **alone**. That
///   assertion is therefore individually load-bearing: neutering it leaves
///   that subject reported by nothing.
/// * **`Defect::AFeedPageRedeliversTheEntryAtItsCursor`** is reported by six
///   assertions at once — the three in
///   [`the_scan_delivered_what_it_should_have`],
///   [`a_fresh_read_of_the_span_agrees`],
///   [`the_two_grants_resume_each_other`] and the one above. **None of the
///   six is individually load-bearing**, because the last of them catches
///   that subject too. They are kept because they are six different things
///   to tell a plugin author about one backend — an entry delivered twice,
///   entries never delivered, an arrival withheld, two readings that
///   disagree, and a cursor that stopped short — and a report naming only
///   the last would leave whoever reads it to work out the other five.
/// * **`Defect::ABoundedReplayNeverCloses`** is reported by the closure
///   assertion in [`a_bounded_replay_is_identical`] **alone**, which makes
///   that one individually load-bearing as well.
///
/// # Six assertions no subject reaches, and why each stays
///
/// * **The two in [`the_replay_repeated_itself`] and the one in
///   [`the_replay_extended_by_nothing_older`]** — clause two entire. A
///   subject that reached them would have to answer *two reads of one
///   position differently*, which means a feed order that is not
///   deterministic or a page that carries unsettled entries. Both are
///   `feed-completeness`'s rule rather than this one's: §3.1 states the
///   determinism and the settledness in that invariant, and DESIGN gives
///   that check the concurrency this one deliberately does not drive. A
///   subject built here would be built for the wrong row and would fail two
///   checks. **A recorded gap, deliberately left to the check that owns the
///   rule** — and still open now that check has landed. Neither subject
///   built for it reaches these three, and the matrix is where that was
///   measured: both are deterministic, so two reads of one position still
///   agree, and what each gets wrong is which entries a page carries or the
///   order it carries them in.
/// * **The entry comparison in [`a_bounded_replay_is_identical`]** — every
///   way of getting `until` wrong that was tried reaches the closure
///   assertion beside it as well, because a replay that stops in the wrong
///   place also fails to report that it stopped. Separating the two would
///   need a backend that carries the wrong span and still closes correctly,
///   which is a backend right about its own bound and wrong about the rows
///   inside it. **A recorded gap**: the assertion is what says *identical*
///   rather than *finished*, and the row asks for identical.
/// * **The entry comparison in
///   [`a_grant_that_names_more_tenants_changes_nothing`]** — a subject would
///   have to answer differently for two grants that admit the same rows,
///   which means reading the grant's *shape* rather than evaluating it. No
///   plausible backend does that; an implausible one would be a caricature.
///   **A recorded gap with that reasoning.**
/// * **The size comparison in [`the_two_grants_stand_in_one_place`]** — the
///   owner named here has landed and this gap is still open, which is worth
///   saying plainly because the two are easy to run together.
///   [`FEED_POSITION_BOUNDED`](crate::contract::FEED_POSITION_BOUNDED) is
///   written and dispatched, and one of its two subjects **is** a plugin
///   keying its position per tenant — but it keys on the tenants a
///   subscription's ledger holds rather than on the tenants a grant names,
///   because a position that moved with the grant could not be resumed by a
///   caller whose grant had since widened and would be a subject wrong
///   twice. So it does not reach this comparison. Measured rather than
///   assumed: neutering this comparison leaves every row of both
///   discrimination matrices unchanged. What would reach it is a backend
///   keying its position on the **grant**, which is a different mistake
///   from the one DESIGN names in those words, and nobody has built it.
///   **A recorded gap, with the owner landed and the subject it still wants
///   named.**
///
/// # What this check does not reach
///
/// It writes no invalidation, so §3.1's **correction order** — *"an
/// invalidation follows its target"* — is asserted nowhere here. It drives
/// no concurrency, so completeness *"whatever the concurrency or commit
/// order"* is asserted nowhere either. Both belong to DESIGN's
/// `feed-completeness` row and are asserted by
/// [`feed_completeness`](super::feed_completeness()); a green run **here**
/// still says nothing about either.
///
/// It never compares two grants' position **values**, so a backend that
/// minted a different position per grant over one ledger passes everything
/// here. `contract_tests`'
/// `a_feed_position_denotes_the_same_ledger_prefix_under_every_grant` holds
/// the reference to that and no check holds a plugin to it, because the
/// property a plugin owes is resumability and a value comparison would
/// forbid the limit-bounded case DESIGN permits.
pub async fn feed_snapshot_and_replay(
    plugin: &dyn UsageCollectorPluginV1,
) -> Vec<ContractViolation> {
    let fixtures = match feed_snapshot_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own `{FEED_SNAPSHOT_AND_REPLAY}` \
                     fixtures, so nothing was submitted. This is a fault in the suite, not in \
                     the plugin under test: {detail}"
                ),
            )];
        }
    };

    // A refusal stops the check. Every probe below is a statement about a
    // scan over entries that are there; over an empty subscription they all
    // hold vacuously, and a check that passes because nothing was stored is
    // the one outcome worse than a failing one.
    let mut violations = submit(plugin, &fixtures.seeded, "seed").await;
    if !violations.is_empty() {
        return violations;
    }

    violations.extend(
        a_scan_observes_no_change_but_arrivals_ahead_of_the_cursor(plugin, &fixtures).await,
    );
    violations.extend(a_replay_from_one_cursor_repeats_and_only_extends(plugin, &fixtures).await);
    violations.extend(a_bounded_replay_is_identical(plugin, &fixtures).await);
    violations.extend(a_grant_that_names_more_tenants_changes_nothing(plugin, &fixtures).await);
    violations
}

/// Clause one: *"A paginated scan observes no entry appearing,
/// disappearing, or changing, except arrivals ahead of the cursor."*
///
/// The scan is paginated at [`FEED_SNAPSHOT_WALK_LIMIT`], paused as soon as
/// it has delivered anything, and resumed after two further entries have
/// been submitted. Four assertions, each reported on its own:
///
/// 1. **Nothing appeared and nothing was delivered twice.** Every identity
///    the scan delivered is one this check wrote and the walking grant
///    admits, and none is delivered more than once. A repeat is a page
///    re-scanning what the page before it had already counted.
/// 2. **Nothing disappeared.** Every entry that was already settled and
///    admitted when the scan began is delivered.
/// 3. **The arrival is delivered.** It settled ahead of the cursor, which is
///    the row's one exception, so the scan carries it rather than leaving it
///    for the next consumer.
/// 4. **Nothing changed, and nothing landed behind the cursor.** A fresh
///    read of the whole span, taken after both arrivals settled, delivers
///    the same entries in the same order and field for field. An entry the
///    scan already passed that the fresh read puts *earlier* is an entry
///    that became visible behind a returned cursor, which §3.1's
///    completeness clause forbids outright; an entry whose fields differ
///    between the two reads is the "changing" the row names.
///
/// The fourth is the only assertion in this check that compares whole
/// records. It compares two answers of the **same backend** rather than an
/// answer against a fixture, deliberately: what a backend stamps into a
/// server-assigned field is `server-field-round-trip`'s rule, and comparing
/// against the submitted record here would report that check's subjects
/// under this check's name.
async fn a_scan_observes_no_change_but_arrivals_ahead_of_the_cursor(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FeedSnapshotFixtures,
) -> Vec<ContractViolation> {
    let subscription = [fixtures.meter.clone()];
    let narrow = admitted_grant();
    let prefix = feed_walk(
        plugin,
        &subscription,
        &narrow,
        FeedStart::Oldest,
        FEED_SNAPSHOT_WALK_LIMIT,
        WalkStop::FirstDelivery,
    )
    .await;

    // The arrivals are submitted whatever the first half of the walk did.
    // Three later probes read a ledger these two entries belong to, and a
    // walk that failed here must not quietly shorten theirs as well.
    let mut violations = submit(
        plugin,
        &[fixtures.arrival.clone(), fixtures.arrival_withheld.clone()],
        "mid-scan arrival",
    )
    .await;

    let prefix = match prefix {
        Ok(walk) => walk,
        Err(detail) => {
            violations.push(violation(FEED_SNAPSHOT_AND_REPLAY, detail));
            return violations;
        }
    };
    let rest = match feed_walk(
        plugin,
        &subscription,
        &narrow,
        FeedStart::After(prefix.stopped_at.clone()),
        FEED_SNAPSHOT_WALK_LIMIT,
        WalkStop::AtTheHead,
    )
    .await
    {
        Ok(walk) => walk,
        Err(detail) => {
            violations.push(violation(FEED_SNAPSHOT_AND_REPLAY, detail));
            return violations;
        }
    };

    let mut scanned = prefix.delivered;
    scanned.extend(rest.delivered);
    violations.extend(the_scan_delivered_what_it_should_have(
        &scanned,
        fixtures,
        prefix.pages,
    ));
    violations.extend(a_fresh_read_of_the_span_agrees(plugin, fixtures, &scanned).await);
    violations
}

/// The first three assertions of clause one, over the entries one scan
/// delivered.
fn the_scan_delivered_what_it_should_have(
    scanned: &[UsageRecord],
    fixtures: &FeedSnapshotFixtures,
    paused_after: usize,
) -> Vec<ContractViolation> {
    let mut violations = Vec::new();
    let delivered = ids(scanned);
    let admitted = fixtures.admitted_ids();

    let strays: Vec<Uuid> = delivered
        .iter()
        .filter(|id| !admitted.contains(id))
        .copied()
        .collect();
    let repeats: Vec<Uuid> = admitted
        .iter()
        .filter(|id| delivered.iter().filter(|seen| *seen == *id).count() > 1)
        .copied()
        .collect();
    if !strays.is_empty() || !repeats.is_empty() {
        violations.push(violation(
            FEED_SNAPSHOT_AND_REPLAY,
            format!(
                "a paginated scan over this check's own meter, paused after {paused_after} \
                 page(s) while two further entries were submitted and then resumed from the \
                 position it had reached, delivered {delivered:?}. It should have delivered a \
                 sub-multiset of {admitted:?}, once each. Delivered but written by nothing \
                 here, or attributed to a tenant this grant does not name: {strays:?}. \
                 Delivered more than once: {repeats:?}. A repeat is a page resuming at the \
                 entry its own start position names rather than after it, which is the keyset \
                 off-by-one, or - where a position is a count of entries rather than a name \
                 for one - a cursor that counted only the entries its own page carried."
            ),
        ));
    }

    let seeded_and_admitted: Vec<Uuid> = fixtures
        .seeded
        .iter()
        .filter(|record| record.tenant_id == CONTRACT_TENANT_ID)
        .map(|record| record.id)
        .collect();
    let lost: Vec<Uuid> = seeded_and_admitted
        .iter()
        .filter(|id| !delivered.contains(id))
        .copied()
        .collect();
    if !lost.is_empty() {
        violations.push(violation(
            FEED_SNAPSHOT_AND_REPLAY,
            format!(
                "a paginated scan that ran from the oldest retained position to the head did \
                 not deliver {lost:?}, which were settled and inside its grant before it began. \
                 A scan observes no entry disappearing: an entry settled ahead of the walk is \
                 delivered by it, and a page whose cursor moved past an entry the page never \
                 carried is how a consumer silently loses one. It delivered {delivered:?}."
            ),
        ));
    }

    if !delivered.contains(&fixtures.arrival.id) {
        violations.push(violation(
            FEED_SNAPSHOT_AND_REPLAY,
            format!(
                "entry {arrival} was submitted while a paginated scan was paused, and the scan \
                 did not deliver it. An arrival ahead of the cursor is the one thing the row \
                 excepts from the snapshot: it settles after the scan began, so it is still \
                 ahead of the position the scan reached, and a consumer following that cursor \
                 has to be handed it. A feed that withholds it charges nobody for the usage it \
                 records. Delivered: {delivered:?}.",
                arrival = fixtures.arrival.id,
            ),
        ));
    }

    violations
}

/// The fourth assertion of clause one: a fresh read of the whole span,
/// taken once both arrivals have settled, is the scan entry for entry.
async fn a_fresh_read_of_the_span_agrees(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FeedSnapshotFixtures,
    scanned: &[UsageRecord],
) -> Vec<ContractViolation> {
    let subscription = [fixtures.meter.clone()];
    let fresh = match feed_walk(
        plugin,
        &subscription,
        &admitted_grant(),
        FeedStart::Oldest,
        FEED_SNAPSHOT_PAGE_LIMIT,
        WalkStop::AtTheHead,
    )
    .await
    {
        Ok(walk) => walk,
        Err(detail) => return vec![violation(FEED_SNAPSHOT_AND_REPLAY, detail)],
    };

    let Some(divergence) = first_divergence(scanned, &fresh.delivered) else {
        return Vec::new();
    };
    vec![violation(
        FEED_SNAPSHOT_AND_REPLAY,
        format!(
            "a paginated scan and a fresh read of the same span, taken straight afterwards with \
             nothing submitted in between, disagree: {divergence}. A scan observes no entry \
             appearing, disappearing or changing. An entry the fresh read places ahead of one \
             the scan had already passed became visible behind a returned cursor, which the \
             Feed order invariant forbids outright - a page carries only settled entries, so no \
             entry the read's compiled scope admits ever becomes visible behind a returned \
             cursor, whatever the concurrency or commit order. An entry whose fields differ \
             between the two reads is the `changing` the row names, and one read of it has \
             already been acted on."
        ),
    )]
}

/// Clause two: *"Replay from one cursor yields the same entries in the same
/// order, extended only by entries settled since."*
///
/// One position, two replays from it, and two entries submitted in between —
/// one the grant admits and one it withholds. Two assertions:
///
/// 1. **The second replay begins with the first, entry for entry and in
///    order.** This is the whole of *"the same entries in the same order"*: a
///    replay is what a consumer does after a crash, and a consumer that
///    replays a cursor and is handed a different prefix has no way to tell
///    which of the two it already charged for.
/// 2. **What the second replay adds is only what settled in between.** The
///    admitted entry of the two may appear; nothing else may. An extension
///    holding an entry that settled long before is a cursor that was lagging
///    rather than a feed that grew.
///
/// **The second is asserted as a subset rather than as an equality**, and
/// that is what makes it survive a repeated run: on a later run the entry
/// that "settles in between" is already settled, the first replay carries it
/// already, and the extension is empty. Empty is *"only by entries settled
/// since"* too.
async fn a_replay_from_one_cursor_repeats_and_only_extends(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FeedSnapshotFixtures,
) -> Vec<ContractViolation> {
    let subscription = [fixtures.meter.clone()];
    let narrow = admitted_grant();
    let mut violations = Vec::new();

    let replayed_from = match feed_walk(
        plugin,
        &subscription,
        &narrow,
        FeedStart::Oldest,
        FEED_SNAPSHOT_WALK_LIMIT,
        WalkStop::FirstDelivery,
    )
    .await
    {
        Ok(walk) => Some(walk.stopped_at),
        Err(detail) => {
            violations.push(violation(FEED_SNAPSHOT_AND_REPLAY, detail));
            None
        }
    };

    let first = match &replayed_from {
        Some(position) => Some(
            feed_walk(
                plugin,
                &subscription,
                &narrow,
                FeedStart::After(position.clone()),
                FEED_SNAPSHOT_PAGE_LIMIT,
                WalkStop::AtTheHead,
            )
            .await,
        ),
        None => None,
    };

    // Submitted whether or not the first replay succeeded, for the reason
    // the arrivals are: the probes after this one read a ledger these two
    // entries belong to.
    violations.extend(
        submit(
            plugin,
            &[
                fixtures.settled_since.clone(),
                fixtures.settled_since_withheld.clone(),
            ],
            "entry settled between two replays",
        )
        .await,
    );

    let first = match first {
        Some(Ok(walk)) => Some(walk),
        Some(Err(detail)) => {
            violations.push(violation(FEED_SNAPSHOT_AND_REPLAY, detail));
            None
        }
        None => None,
    };
    let (Some(position), Some(first)) = (replayed_from, first) else {
        return violations;
    };

    let second = match feed_walk(
        plugin,
        &subscription,
        &narrow,
        FeedStart::After(position),
        FEED_SNAPSHOT_PAGE_LIMIT,
        WalkStop::AtTheHead,
    )
    .await
    {
        Ok(walk) => walk,
        Err(detail) => {
            violations.push(violation(FEED_SNAPSHOT_AND_REPLAY, detail));
            return violations;
        }
    };

    violations.extend(the_replay_repeated_itself(&first, &second));
    violations.extend(the_replay_extended_by_nothing_older(
        &first, &second, fixtures,
    ));
    violations
}

/// The first assertion of clause two: the later replay opens with the
/// earlier one, entry for entry.
fn the_replay_repeated_itself(first: &FeedWalk, second: &FeedWalk) -> Vec<ContractViolation> {
    let shared = second.delivered.len().min(first.delivered.len());
    let divergence = if second.delivered.len() < first.delivered.len() {
        Some(format!(
            "the second replay carried {second_len} entries where the first carried \
             {first_len}, so entries the first one delivered are no longer there at all",
            second_len = second.delivered.len(),
            first_len = first.delivered.len(),
        ))
    } else {
        first_divergence(&first.delivered, &second.delivered[..shared])
    };
    let Some(divergence) = divergence else {
        return Vec::new();
    };
    vec![violation(
        FEED_SNAPSHOT_AND_REPLAY,
        format!(
            "two replays from one position disagree on what follows it: {divergence}. A replay \
             from one cursor yields the same entries in the same order, and it is a consumer's \
             recovery path: a charging consumer that crashed after a page replays the cursor it \
             had stored, so a replay that reorders, drops or rewrites what it handed out before \
             leaves that consumer unable to tell which entries it has already acted on."
        ),
    )]
}

/// The second assertion of clause two: the extension holds nothing that was
/// settled before the first replay ran.
fn the_replay_extended_by_nothing_older(
    first: &FeedWalk,
    second: &FeedWalk,
    fixtures: &FeedSnapshotFixtures,
) -> Vec<ContractViolation> {
    if second.delivered.len() <= first.delivered.len() {
        return Vec::new();
    }
    let extension = ids(&second.delivered[first.delivered.len()..]);
    let older: Vec<Uuid> = extension
        .iter()
        .filter(|id| **id != fixtures.settled_since.id)
        .copied()
        .collect();
    if older.is_empty() {
        return Vec::new();
    }
    vec![violation(
        FEED_SNAPSHOT_AND_REPLAY,
        format!(
            "a replay from one position was extended by {older:?}, and the only entry this \
             check let settle between the two replays is {settled}. A replay is extended only \
             by entries settled since: everything else this grant admits was already settled \
             when the first replay ran and should have been carried by it. An entry surfacing \
             late is an entry a consumer had already been told it was past, so a consumer that \
             folds each page as it arrives counts it twice or, having pruned its own state, not \
             at all. The whole extension was {extension:?}.",
            settled = fixtures.settled_since.id,
        ),
    )]
}

/// Clause three: *"replay bounded by `until` is identical."*
///
/// The span is read twice: once live and unbounded, and once bounded by the
/// position the live read reached. Two assertions:
///
/// 1. **The bounded replay carries the same entries in the same order.**
///    Identical means identical: the bound is on the position, not on what
///    the pages in front of it carry.
/// 2. **The bounded replay closes.** Its last page carries no continuation,
///    which is the one thing that tells a caller a bounded replay has
///    reached its `until` ([`crate::feed::FeedPage::next`]: *"`Some` on
///    every page of a live read, short pages included; `None` once a bounded
///    replay has reached its `until`"*). A position here is a page whose
///    continuation the caller keeps following, which is a hang rather than a
///    wrong value.
///
/// Both reads are taken back to back with nothing submitted between them,
/// which is what makes the comparison a statement about the `until`
/// parameter rather than about the ledger.
async fn a_bounded_replay_is_identical(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FeedSnapshotFixtures,
) -> Vec<ContractViolation> {
    let subscription = [fixtures.meter.clone()];
    let narrow = admitted_grant();
    let live = match feed_walk(
        plugin,
        &subscription,
        &narrow,
        FeedStart::Oldest,
        FEED_SNAPSHOT_PAGE_LIMIT,
        WalkStop::AtTheHead,
    )
    .await
    {
        Ok(walk) => walk,
        Err(detail) => return vec![violation(FEED_SNAPSHOT_AND_REPLAY, detail)],
    };
    let bounded = match bounded_replay(
        plugin,
        &subscription,
        &narrow,
        live.stopped_at.clone(),
        FEED_SNAPSHOT_PAGE_LIMIT,
    )
    .await
    {
        Ok(replay) => replay,
        Err(detail) => return vec![violation(FEED_SNAPSHOT_AND_REPLAY, detail)],
    };

    let mut violations = Vec::new();
    if let Some(divergence) = first_divergence(&live.delivered, &bounded.delivered) {
        violations.push(violation(
            FEED_SNAPSHOT_AND_REPLAY,
            format!(
                "a replay bounded by the position a live read of the same span had just \
                 reached is not that read: {divergence}. Replay bounded by `until` is \
                 identical. The bound names where the replay stops, not what the pages in \
                 front of it carry, so a consumer reconciling a closed range against the live \
                 feed it followed compares two readings of one ledger - and a bound that \
                 changes the reading makes that reconciliation report a discrepancy the ledger \
                 does not hold."
            ),
        ));
    }
    if !bounded.closed {
        violations.push(violation(
            FEED_SNAPSHOT_AND_REPLAY,
            "a replay bounded by the position a live read had just reached never closed: it \
             went on carrying a continuation until this check stopped following it. An absent \
             `next` is the one thing that says a bounded replay has reached its `until`, so a \
             replay that keeps minting one is a caller following a cursor forever over a range \
             it has already read whole."
                .to_owned(),
        ));
    }
    violations
}

/// Clause four: *"including over a subscription spanning many tenants, some
/// never written."*
///
/// **Three grants are in play in this check, and the two widest meet here.**
/// [`admitted_grant`] pins one tenant and is what every clause above walks
/// under; the two below name two and five. "Narrower" and "wider" in this
/// probe and its helpers always mean the two-tenant and five-tenant grants,
/// never the one-tenant one — [`a_wider_grant_resumes_a_narrower_walk_at_the_head`]
/// is the only place the one-tenant grant is compared against another.
///
/// The narrower of the two names the two tenants this check actually writes
/// under; the wider names those two and three more —
/// [`SCOPE_UNUSED_TENANT_ID`], which owns no entry anywhere in this suite by
/// design, and two ids [`contract_tenant`] mints that own none on this
/// check's meter. The two grants therefore admit exactly the same entries,
/// and the clause is that the wider one changes nothing.
///
/// Three assertions:
///
/// 1. **The entries are the same, in the same order and field for field.**
/// 2. **The position encodes to the same size.** Not to the same *value*: a
///    position's guarantee is resumability rather than identity, and this
///    check does not compare two grants' positions for equality anywhere.
///    Size is the figure DESIGN bounds — §3.1 has a position's *"encoded
///    size"* not grow with a subscription's breadth — and a grant that grew
///    its position per tenant named is exactly what that bound forbids.
/// 3. **Each grant resumes from the other's position with nothing left.**
///    This is the behavioural half, and it is what "the cursor is unchanged"
///    means for a type whose values may not be compared across grants.
///
/// A fourth assertion, in
/// [`a_wider_grant_resumes_a_narrower_walk_at_the_head`], reads the same
/// property in the direction where the two grants do *not* admit the same
/// entries.
async fn a_grant_that_names_more_tenants_changes_nothing(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FeedSnapshotFixtures,
) -> Vec<ContractViolation> {
    let subscription = [fixtures.meter.clone()];
    let written = both_written_tenants_grant();
    let spanning = many_tenants_grant();

    let (narrow, wide) = match (
        feed_walk(
            plugin,
            &subscription,
            &written,
            FeedStart::Oldest,
            FEED_SNAPSHOT_PAGE_LIMIT,
            WalkStop::AtTheHead,
        )
        .await,
        feed_walk(
            plugin,
            &subscription,
            &spanning,
            FeedStart::Oldest,
            FEED_SNAPSHOT_PAGE_LIMIT,
            WalkStop::AtTheHead,
        )
        .await,
    ) {
        (Ok(narrow), Ok(wide)) => (narrow, wide),
        (Err(detail), _) | (_, Err(detail)) => {
            return vec![violation(FEED_SNAPSHOT_AND_REPLAY, detail)];
        }
    };

    let mut violations = Vec::new();
    if let Some(divergence) = first_divergence(&narrow.delivered, &wide.delivered) {
        violations.push(violation(
            FEED_SNAPSHOT_AND_REPLAY,
            format!(
                "a grant naming three further tenants - one that owns no entry anywhere in this \
                 suite and two that own none on this check's meter - changed what the feed \
                 carried: {divergence}. A subscription spanning many tenants, some never \
                 written, reads exactly as one spanning only the tenants that were: a tenant \
                 with nothing under it contributes nothing, and a feed whose answer moves when \
                 one is named is answering the grant's shape rather than the ledger."
            ),
        ));
    }
    violations.extend(the_two_grants_stand_in_one_place(&narrow, &wide));
    violations.extend(
        the_two_grants_resume_each_other(plugin, fixtures, (&written, &narrow), (&spanning, &wide))
            .await,
    );
    violations.extend(a_wider_grant_resumes_a_narrower_walk_at_the_head(plugin, fixtures).await);
    violations
}

/// The size half of clause four: naming three more tenants does not grow the
/// position.
fn the_two_grants_stand_in_one_place(narrow: &FeedWalk, wide: &FeedWalk) -> Vec<ContractViolation> {
    let (narrow, wide) = (&narrow.stopped_at, &wide.stopped_at);
    if narrow.len() == wide.len() {
        return Vec::new();
    }
    vec![violation(
        FEED_SNAPSHOT_AND_REPLAY,
        format!(
            "one grant naming two tenants was handed a {narrow_len}-byte position and a grant \
             naming those two and three more a {wide_len}-byte one, over the same subscription \
             and the same entries. A position's encoded size may not grow with a subscription's \
             breadth: the gateway carries it inside a length-bounded wire cursor, so a position \
             keyed per tenant named runs a consumer out of cursor as its grant widens. The two \
             values are deliberately not compared - a position's guarantee is resumability, not \
             identity - but their sizes are.",
            narrow_len = narrow.len(),
            wide_len = wide.len(),
        ),
    )]
}

/// The behavioural half of clause four: either grant resumes from the
/// other's position and finds nothing left.
async fn the_two_grants_resume_each_other(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FeedSnapshotFixtures,
    narrow: (&ast::Expr, &FeedWalk),
    wide: (&ast::Expr, &FeedWalk),
) -> Vec<ContractViolation> {
    let subscription = [fixtures.meter.clone()];
    let mut violations = Vec::new();
    for (resuming, minted_by, scope, position) in [
        (
            "the narrower grant",
            "the wider one",
            narrow.0,
            &wide.1.stopped_at,
        ),
        (
            "the wider grant",
            "the narrower one",
            wide.0,
            &narrow.1.stopped_at,
        ),
    ] {
        match feed_walk(
            plugin,
            &subscription,
            scope,
            FeedStart::After(position.clone()),
            FEED_SNAPSHOT_PAGE_LIMIT,
            WalkStop::AtTheHead,
        )
        .await
        {
            Ok(walk) if walk.delivered.is_empty() => {}
            Ok(walk) => violations.push(violation(
                FEED_SNAPSHOT_AND_REPLAY,
                format!(
                    "{resuming} resumed from the position {minted_by} reached at the head and \
                     was handed {left:?}. Both grants admit the same entries here - the wider \
                     one names three tenants that own none - and both had just read the \
                     subscription to its head, so nothing is left behind either position. An \
                     entry surfacing here is a cursor that stopped short of the head under one \
                     grant, which is a consumer told it was up to date while entries sat behind \
                     its stored position.",
                    left = ids(&walk.delivered),
                ),
            )),
            Err(detail) => violations.push(violation(FEED_SNAPSHOT_AND_REPLAY, detail)),
        }
    }
    violations
}

/// The direction of clause four in which the two grants do **not** admit the
/// same entries: a grant that admits more, resuming from the position a
/// narrower walk reached at the head, is still at the head.
///
/// **This is the assertion that catches a cursor counting the entries a page
/// admitted rather than the entries it scanned**, and it catches it from a
/// different direction than `contract_tests`' four reference tests do — none
/// of those resumes a wider grant from a narrower walk's *fixpoint*.
///
/// DESIGN §3.1's Feed order invariant is what it rests on: *"A page reaching
/// the settled head returns its cursor at the head, so a cursor's age
/// reflects its consumer's progress, not the last arrival."* The head is the
/// subscription's, not the grant's — §3.1 fixes a position's age by the
/// oldest subsequent entry of a subscribed type *"whether or not the
/// reader's authorization scope admits that entry"* — so a walk that has
/// reached it has left nothing behind its cursor, including the entries its
/// own grant withheld.
///
/// The ledger is built for it. The **last** entry on this check's meter is
/// attributed to the tenant the narrow grant withholds, so a cursor that
/// advanced only past admitted entries stops one short of the head and this
/// read hands the wider grant that entry back. A cursor that advanced past
/// every entry it scanned stops at the head and this read hands back
/// nothing.
///
/// What it costs a consumer is the reason DESIGN separates the two: a
/// position that means *"the last entry this grant admitted"* cannot be
/// resumed by a caller whose grant has since widened without silently
/// skipping everything the narrower grant withheld ahead of it.
async fn a_wider_grant_resumes_a_narrower_walk_at_the_head(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FeedSnapshotFixtures,
) -> Vec<ContractViolation> {
    let subscription = [fixtures.meter.clone()];
    let narrow = match feed_walk(
        plugin,
        &subscription,
        &admitted_grant(),
        FeedStart::Oldest,
        FEED_SNAPSHOT_PAGE_LIMIT,
        WalkStop::AtTheHead,
    )
    .await
    {
        Ok(walk) => walk,
        Err(detail) => return vec![violation(FEED_SNAPSHOT_AND_REPLAY, detail)],
    };

    match feed_walk(
        plugin,
        &subscription,
        &both_written_tenants_grant(),
        FeedStart::After(narrow.stopped_at),
        FEED_SNAPSHOT_PAGE_LIMIT,
        WalkStop::AtTheHead,
    )
    .await
    {
        Ok(walk) if walk.delivered.is_empty() => Vec::new(),
        Ok(walk) => vec![violation(
            FEED_SNAPSHOT_AND_REPLAY,
            format!(
                "a grant pinning one tenant read this subscription to its head, and a grant \
                 naming that tenant and one more resumed from the position it stopped at and \
                 was handed {left:?}. A page reaching the settled head returns its cursor at \
                 the head, and the head is the subscription's rather than the grant's: a \
                 position's age is fixed by the oldest subsequent entry of a subscribed type \
                 whether or not the reader's scope admits that entry. So a cursor advances past \
                 every entry the read scanned, not past every entry it carried, and a walk that \
                 has reached the head has left nothing behind it. Entries surfacing here are \
                 ones the narrower grant withheld and its cursor never passed - which means a \
                 consumer whose grant later widens resumes its stored cursor and silently skips \
                 every entry the narrower grant withheld ahead of it.",
                left = ids(&walk.delivered),
            ),
        )],
        Err(detail) => vec![violation(FEED_SNAPSHOT_AND_REPLAY, detail)],
    }
}

/// Submits entries that must all be accepted, reporting the first refusal.
///
/// Every probe here is a statement about a scan over entries that are there.
/// A refusal is reported against the check rather than as a harness fault:
/// the submission is well formed, and a backend that will not accept it has
/// broken something this check cannot then go on to observe.
async fn submit(
    plugin: &dyn UsageCollectorPluginV1,
    records: &[UsageRecord],
    role: &str,
) -> Vec<ContractViolation> {
    for record in records {
        if let Err(err) = plugin.create_usage_record(record.clone()).await {
            return vec![violation(
                FEED_SNAPSHOT_AND_REPLAY,
                format!(
                    "`create_usage_record` refused a {role} entry (record {id}, tenant \
                     {tenant}), so the paginated scan this check makes had nothing to observe \
                     at that point in the ledger: {err}",
                    id = record.id,
                    tenant = record.tenant_id,
                ),
            )];
        }
    }
    Vec::new()
}

/// The grant the paginated walk dispatches: `tenant_id eq <suite tenant>`.
///
/// It admits half this check's ledger and withholds the other half, which is
/// what makes every page of the walk jump its cursor past an entry the page
/// did not carry.
fn admitted_grant() -> ast::Expr {
    tenant_disjunction(CONTRACT_TENANT_ID, &[])
}

/// A grant naming both tenants this check writes under, and so admitting
/// everything on its meter.
fn both_written_tenants_grant() -> ast::Expr {
    tenant_disjunction(CONTRACT_TENANT_ID, &[SCOPE_EXCLUDED_TENANT_ID])
}

/// The same grant with three further tenants named, none of which owns an
/// entry on this check's meter.
///
/// [`SCOPE_UNUSED_TENANT_ID`] owns no entry anywhere in this suite — that is
/// its whole reason for existing, and
/// `scope-is-a-filter-on-every-read-path` rests on it — so naming it here
/// adds a disjunct and nothing else. The two [`contract_tenant`] ids come
/// from a block held clear of the four named tenant ids, and own nothing on
/// a meter derived for this check's exclusive use.
fn many_tenants_grant() -> ast::Expr {
    tenant_disjunction(
        CONTRACT_TENANT_ID,
        &[
            SCOPE_EXCLUDED_TENANT_ID,
            SCOPE_UNUSED_TENANT_ID,
            contract_tenant(FEED_SNAPSHOT_FIRST_UNWRITTEN_TENANT),
            contract_tenant(FEED_SNAPSHOT_SECOND_UNWRITTEN_TENANT),
        ],
    )
}

/// Builds the four seeded entries, the two that arrive mid-scan and the two
/// that settle between the replays.
///
/// The meter is this check's own, which is the separation that matters here:
/// a feed read selects by subscription, so an entry of this check's on the
/// suite's shared meter would be an entry some *other* check's page has to
/// account for, and an entry of some other check's on this meter would be
/// one this check's walk delivers and cannot name.
///
/// The tenants alternate through all eight, so that a withheld entry falls
/// between every pair of admitted ones and the last entry on the meter is a
/// withheld one.
///
/// Three guards keep the check from passing by construction, and all three
/// are the suite's own facts rather than the plugin's, so all three are
/// reported as [`HARNESS_FAULT`]:
///
/// * The admitted and withheld tenants differ. If they did not, the walking
///   grant would admit every entry, no page's cursor would ever jump past
///   one it did not carry, and the assertion in
///   [`a_wider_grant_resumes_a_narrower_walk_at_the_head`] would have
///   nothing to be about.
/// * None of the three tenants named only in the wide grant is one of those
///   two. If one were, the wide grant would not be the narrow grant plus
///   tenants that own nothing, and clause four would be comparing two grants
///   that admit different things.
/// * The eight entries derive eight distinct ids. They do — the tenant and
///   the idempotency key are two of the six identity inputs — but if two
///   ever collided, one submission would be absorbed as a retry of the
///   other and this check would assert that a scan delivers an entry that
///   was never separately stored.
fn feed_snapshot_fixtures() -> Result<FeedSnapshotFixtures, String> {
    if CONTRACT_TENANT_ID == SCOPE_EXCLUDED_TENANT_ID {
        return Err(format!(
            "the tenant the walking grant admits and the tenant it withholds are one value \
             ({CONTRACT_TENANT_ID}), so the grant admits this check's whole ledger and no page \
             of the walk has to advance its cursor past an entry it did not carry"
        ));
    }
    let unwritten = [
        SCOPE_UNUSED_TENANT_ID,
        contract_tenant(FEED_SNAPSHOT_FIRST_UNWRITTEN_TENANT),
        contract_tenant(FEED_SNAPSHOT_SECOND_UNWRITTEN_TENANT),
    ];
    for tenant in unwritten {
        if tenant == CONTRACT_TENANT_ID || tenant == SCOPE_EXCLUDED_TENANT_ID {
            return Err(format!(
                "tenant {tenant} is named by the wider grant as one this check writes nothing \
                 under, and it is one of the two this check writes under. The wider grant would \
                 then admit a different set of entries from the narrower one, and the clause \
                 that compares them asserts that naming a tenant with nothing under it changes \
                 nothing"
            ));
        }
    }

    let meter = check_meter(FEED_SNAPSHOT_AND_REPLAY, "main")?;
    let quantity = UsageQuantity::parse(FEED_SNAPSHOT_QUANTITY).map_err(|err| {
        format!("the check's own quantity `{FEED_SNAPSHOT_QUANTITY}` does not parse: {err}")
    })?;
    let entry = |role: &str, tenant_id: Uuid| -> Result<UsageRecord, String> {
        fixture_record_on(
            meter.clone(),
            tenant_id,
            &feed_snapshot_key(role)?,
            quantity,
            CONTRACT_ACCEPTED_AT,
            FEED_SNAPSHOT_WINDOW_FROM,
            FEED_SNAPSHOT_WINDOW_END,
        )
    };

    let mut seeded = Vec::with_capacity(FEED_SNAPSHOT_SEEDS);
    for index in 0..FEED_SNAPSHOT_SEEDS {
        let admitted = index % 2 == 0;
        let tenant_id = if admitted {
            CONTRACT_TENANT_ID
        } else {
            SCOPE_EXCLUDED_TENANT_ID
        };
        seeded.push(entry(&format!("seed-{index}"), tenant_id)?);
    }

    let arrival = entry("arrival", CONTRACT_TENANT_ID)?;
    let arrival_withheld = entry("arrival-withheld", SCOPE_EXCLUDED_TENANT_ID)?;
    let settled_since = entry("settled-since", CONTRACT_TENANT_ID)?;
    let settled_since_withheld = entry("settled-since-withheld", SCOPE_EXCLUDED_TENANT_ID)?;
    // Bound out of the way of the move below: the closure borrows `meter`,
    // and the struct this builds owns it.
    let _ = entry;
    let fixtures = FeedSnapshotFixtures {
        meter,
        seeded,
        arrival,
        arrival_withheld,
        settled_since,
        settled_since_withheld,
    };

    let written = fixtures.all();
    for (index, record) in written.iter().enumerate() {
        if written[..index].iter().any(|other| other.id == record.id) {
            return Err(format!(
                "two of this check's eight entries derive one id ({id}); the second submission \
                 would be absorbed as a retry of the first, and the scan would be asserted to \
                 deliver an entry that was never separately stored",
                id = record.id,
            ));
        }
    }
    Ok(fixtures)
}

/// The idempotency key one of this check's entries submits under.
///
/// Keyed on the role name for the reason
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture) spells
/// out: in a ledger with no delete path, an edited fixture must take a fresh
/// identity rather than inherit an accepted entry's. It is also what makes a
/// repeated run resubmit these eight identities rather than mint eight more,
/// which is what keeps this check's ledger the same length on every run.
fn feed_snapshot_key(role: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{FEED_SNAPSHOT_AND_REPLAY}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}
