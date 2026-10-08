//! The DESIGN §3.3 `feed-bootstrap-position` check.
//!
//! See [`feed_bootstrap_position`] for what it asserts. The module holds the
//! entries it writes, the meters it names, and the two drops it drives — the
//! purge its assertions are about, and the one that puts back what the purge
//! took. Its reads are [`super::super::feed_walk`]'s, shared with
//! [`feed_snapshot_and_replay`](super::feed_snapshot_and_replay()) and
//! [`feed_completeness`](super::feed_completeness()).
//!
//! **It is the only check that removes anything**, and its coverage under
//! [`run_all`](super::super::run_all) is partial: the retention assertion needs
//! a backend's retention to have actually swept, which no method on
//! [`UsageCollectorPluginV1`] performs. The drive is [`ContractRetention`],
//! beside the SPI, arriving through
//! [`run_all_with_retention`](super::super::run_all_with_retention); handed
//! `None` the check skips that assertion.
//! [`RETENTION_DRIVEN_CHECKS`](super::super::RETENTION_DRIVEN_CHECKS) is what a
//! caller reporting coverage reads.

use crate::contract::feed_walk::{WalkStop, feed_walk, ids};
use crate::contract::fixtures::{
    CONTRACT_ACCEPTED_AT, CONTRACT_TENANT_ID, check_meter, check_window_from, contract_scope,
    fixture_record_on, seed_usage_record, violation,
};
use crate::contract::retention::ContractRetention;
use crate::contract::{ContractViolation, FEED_BOOTSTRAP_POSITION, HARNESS_FAULT};
use crate::feed::FeedStart;
use crate::models::IdempotencyKey;
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::stored::{MeterRef, StoredUsageRecord};

/// The start of the covered period this check's entries are offset from:
/// the offset [`check_window_from`] tables for this check, for the reason
/// that accessor gives.
///
/// **Where the other checks sit is not enumerated here**, and this constant is
/// where the reason is recorded: per-module lists of everybody else's offsets
/// all went stale, and a list that is wrong reads as a guarantee of separation
/// nobody is keeping. The offsets are one table with a compile-time guard over
/// it, so the separation is established rather than asserted.
///
/// The separation buys little for the reads — **this check dispatches no range
/// at all**, every read being a feed page, which selects by subscription, so
/// [`check_meter`] is what does the work. The offset is kept because the
/// separation runs the other way too, and **this is the one place that matters
/// most**: the purge is keyed on a covered-period floor, and a floor reaching
/// into another check's period would take that check's fixtures with it. That
/// the drive is also keyed on a GTS type is the belt to this brace — see
/// [`ContractRetention::drop_before`].
const FEED_BOOTSTRAP_WINDOW_FROM: time::OffsetDateTime =
    check_window_from(FEED_BOOTSTRAP_POSITION, "main");

/// The end of the **first** entry's covered period, an hour past
/// [`FEED_BOOTSTRAP_WINDOW_FROM`].
///
/// It is the entry the purge takes: a floor at
/// [`FEED_BOOTSTRAP_RETAINED_WINDOW_END`] is strictly above this bound, and
/// [`ContractRetention::drop_before`] removes every entry of the named type
/// whose covered period ends *before* the floor.
const FEED_BOOTSTRAP_PURGED_WINDOW_END: time::OffsetDateTime =
    FEED_BOOTSTRAP_WINDOW_FROM.saturating_add(time::Duration::hours(1));

/// The end of the **second** entry's covered period, and the floor the purge
/// is driven to.
///
/// The bound `drop_before` applies is exclusive, which is what lets one
/// value be both: an entry whose period ends exactly at the floor is inside
/// the retention that floor expresses and stays. So a drop to this instant
/// removes the first entry and leaves the second, which is the two-entry
/// ledger the fourth assertion needs — an oldest that is gone and an oldest
/// that took its place.
const FEED_BOOTSTRAP_RETAINED_WINDOW_END: time::OffsetDateTime =
    FEED_BOOTSTRAP_WINDOW_FROM.saturating_add(time::Duration::hours(2));

/// The floor the **restoring** drop is driven to, an hour above
/// [`FEED_BOOTSTRAP_RETAINED_WINDOW_END`], so that it takes the second entry
/// as well.
///
/// See [`restore_this_checks_own_meter`] for why a check that purges has to
/// finish by emptying its own meter rather than by leaving one entry on it.
const FEED_BOOTSTRAP_RESTORING_FLOOR: time::OffsetDateTime =
    FEED_BOOTSTRAP_WINDOW_FROM.saturating_add(time::Duration::hours(3));

/// The quantity both entries carry.
///
/// Its value is asserted nowhere: every assertion here compares identities or
/// reads a page's shape. It is exactly representable in a binary float all the
/// same, so `contract_mutants`'s `Defect::QuantityThroughFloat` cannot reach
/// this check for a rule that belongs to `quantity-round-trip`.
const FEED_BOOTSTRAP_QUANTITY: &str = "1.5";

/// The page limit every read in this check dispatches: **one**.
///
/// The row is about where a read *begins*, and a page wide enough to carry the
/// whole subscription answers that only incidentally. At a limit of one, the
/// first entry a read delivers is the first it could have delivered.
///
/// This is not an assumption that one entry arrives on the first page: a short
/// page is conforming, which is why the reads below are **followed** (see
/// [`a_bootstrap_read_begins_at_the_oldest_entry`]).
const FEED_BOOTSTRAP_PAGE_LIMIT: u64 = 1;

/// The two entries this check writes and the two meters it reads over.
struct FeedBootstrapFixtures {
    /// The meter both entries are written to, and the meter the purge is
    /// driven over.
    main: MeterRef,
    /// The meter this check subscribes to and never writes to.
    ///
    /// DESIGN's row asks what a *"subscription retaining no entries"* answers,
    /// and a meter derived for this check's exclusive use is the only way to
    /// have one: `run_all` dispatches every check against one backend, so any
    /// other meter is empty only by luck of dispatch order.
    empty: MeterRef,
    /// The older of the two, submitted first, and the one the purge removes.
    purged: StoredUsageRecord,
    /// The newer of the two, submitted second. It survives the purge and is
    /// then the oldest entry the subscription retains.
    retained: StoredUsageRecord,
}

/// `feed-bootstrap-position` — *"`FeedStart::Oldest` begins at the oldest
/// entry the subscription retains, never at the head, and is never refused
/// on the retention floor. A subscription retaining no entries returns an
/// empty page carrying a head cursor."* (DESIGN §3.3, "Plugin contract
/// tests", line 1324.)
///
/// **The whole row is about one argument**, which is why it is one check:
/// `FeedStart` is named rather than inferred, and `Oldest` is the name for the
/// one place a consumer with no position of its own can start. DESIGN §3.1's
/// `FeedStart` row fixes what the name means — *"`Oldest` is the oldest entry
/// the subscription retains, `After(position)` continues from a position the
/// feed issued. … v1 admits these two, and neither begins at the head"* — and
/// the SPI restates it on
/// [`read_feed_page`](UsageCollectorPluginV1::read_feed_page).
///
/// # What is asserted
///
/// 1. **`Oldest` begins at the oldest.** The first entry a read from
///    `FeedStart::Oldest` delivers is this check's **first** entry, not its
///    second.
/// 2. **`Oldest` is not the head.** That read delivers something at all.
/// 3. **A subscription retaining no entries is served, carries nothing, and
///    carries a head cursor.** Three reports over one page read on a meter
///    nothing has ever been written to.
/// 4. **After a purge, `Oldest` is still not refused and begins at the oldest
///    entry that is left.** Retention is driven to a floor that takes the first
///    entry and leaves the second; the read that follows must answer, and must
///    answer with the second.
///
/// One and two are two reports rather than one because they are two things to
/// tell a plugin author: *"it returned the wrong entry"* is a backend whose feed
/// order or start position is wrong, *"it returned nothing"* is one that treated
/// an absent cursor as "start from now" and will hand a bootstrapping consumer
/// an empty stream forever.
///
/// # The reads are followed, and why at a limit of one
///
/// `limit` bounds what a page *carries* and promises nothing about everything
/// settled fitting on one, so a short page is conforming: a backend answering an
/// empty first page and delivering on the second is paging in small steps, not
/// starting at the head. The reads that ask *where a bootstrap begins* —
/// assertions 1, 2 and 4 — are therefore [`super::super::feed_walk`]'s
/// [`WalkStop::FirstDelivery`] walks, following the cursor until something
/// arrives or it stands still; *"nothing ever arrived"* is what says a read
/// began at the head.
///
/// Assertion 3 reads **one page** directly instead. Its clause is about a page's
/// own shape — *"returns an empty page carrying a head cursor"* — and a walk
/// would fold the continuation into its own exit condition and report an absent
/// one as a failed walk, leaving nothing to say about which obligation broke.
///
/// # The purge, and what it costs a run that cannot drive one
///
/// Assertion 4 needs retention to have actually removed something, and no SPI
/// method removes anything — see [`retention`](super::super::retention) for why
/// the drive sits beside the SPI. [`run_all`](super::super::run_all) therefore
/// skips it and [`run_all_with_retention`](super::super::run_all_with_retention)
/// runs it, so a green `run_all` says nothing about assertion 4 and
/// [`RETENTION_DRIVEN_CHECKS`](super::super::RETENTION_DRIVEN_CHECKS) is what a
/// caller reporting coverage reads. That claim is held mechanically:
/// `contract_tests`' `RETENTION_DRIVEN_MATRIX` carries rows whose only defect is
/// one assertion 4 reaches, and
/// `each_driven_check_fails_against_its_own_defect_and_no_other` requires them
/// to pass under `run_all` and fail under `run_all_with_retention`.
///
/// # Surviving a repeated run
///
/// Both entry points are dispatched against one persistent backend, and
/// `the_reference_backend_conforms_to_a_repeated_run` /
/// `..._under_a_retention_drive` require a second dispatch to be as green as the
/// first. **A purging check cannot survive a repeat by keying its identities
/// alone.** Every other check re-delivers its entries and a conforming backend
/// absorbs them, so the ledger it reads is the ledger it read before. A purged
/// entry is not absorbed: re-delivering it is an **insert**, which lands at the
/// head of the feed's order rather than back where it was, and *"the oldest
/// entry the subscription retains"* would then name the other one.
///
/// **So the check restores its own meter**: after assertion 4 it drives a second
/// drop above both entries and re-delivers both in order, so both are fresh
/// inserts in this check's own order. See [`restore_this_checks_own_meter`].
///
/// That restoration is what buys a discipline this check cannot keep.
/// [`feed_snapshot_and_replay`](super::feed_snapshot_and_replay()) asks that no
/// assertion name where in the delivered order an entry falls, and **this
/// check's whole subject is exactly that**. It cannot avoid the assertion, so it
/// keeps the order stable instead — a stronger obligation, and the reason the
/// restoration is not optional. A run handed no driver neither purges nor
/// restores, and has nothing to restore.
///
/// # Which assertions a subject reaches, measured
///
/// Every assertion below was inverted and confirmed to fire against the
/// reference backend, so none is dead. What each *catches* was then measured by
/// neutering it and re-running the discrimination matrix and the driven subject
/// tests. `contract_mutants`' `AFeedBootstrapReadStartsAtTheHead` is reported by
/// assertions 1 and 2 and by **neither alone** (it delivers nothing, so the page
/// carries no entry *and* the entry it carries is not the oldest), so the pair is
/// load-bearing; it also reaches every other check that bootstraps a feed read.
/// `RefusesTheOldestStartAfterASweep` is reported by assertion 4's refusal report
/// alone and `SkipsTheOldestEntryASweepLeft` by its position report alone, each
/// individually load-bearing: a read that is refused never answers, and a read
/// that answers can still answer from the wrong place. Neither driven subject is
/// in the discrimination matrix — `run_all` drives nothing, so under that
/// dispatch both are behaviourally the reference backend — and their columns are
/// in `RETENTION_DRIVEN_MATRIX`.
///
/// # The assertions no subject reaches, and why each stays
///
/// Neutering any of these changed no row of the matrix and neither driven
/// subject's result — the measurement rather than an expectation.
///
/// * **The walk's own failure report**, in
///   [`a_bootstrap_read_begins_at_the_oldest_entry`]: a subject producing either
///   of its causes would produce it for every check that walks. **A recorded gap
///   shared with every caller of [`super::super::feed_walk`]**.
/// * **Assertion 3's three parts** — refused, invented, closed. A subject would
///   have to answer differently for a subscription with nothing under it, which
///   is a backend reading its subscription's shape rather than evaluating it.
///   The plausible shape-reading defect needs a second meter with entries in it,
///   and this check's second meter is empty by definition. One candidate exists
///   for the *refusal* part and is named rather than built: a backend resolving a
///   subscription through a per-type feed-order row and answering `Internal`
///   when a type has none yet — which would reach `feed-completeness`'s quiet
///   probe too, so it is a two-check row rather than an isolating one.
/// * **The two drive-failure reports**, in
///   [`a_bootstrap_read_after_a_purge_begins_at_what_is_left`] and
///   [`restore_this_checks_own_meter`]. **No subject can reach these and it is
///   not for want of one**: they fire when [`ContractRetention::drop_before`]
///   answers `Err`, the suite failing to set a scenario up rather than a backend
///   answering an SPI call, which is why both are [`HARNESS_FAULT`].
///   **Unreachable by construction, recorded rather than removed**, because a
///   drive that started failing silently would leave assertion 4 passing over a
///   ledger nothing had been removed from.
/// * **The submission report** in [`submit`]. No subject refuses a well-formed
///   record carrying a fresh idempotency key. **A recorded gap shared with every
///   other check's submission guard**, whose value is that a check which lost its
///   fixtures says so instead of asserting over an empty ledger.
/// * **The fixture guards** in [`FeedBootstrapFixtures::guards`] — the suite's
///   own facts rather than the plugin's, so no backend outcome can make one
///   fire. They fire for an edit to this module, which is what `HARNESS_FAULT`
///   is for.
///
/// # What this check does not reach
///
/// It writes under one tenant and reads under a grant that admits it, so it
/// asserts nothing about a feed page narrowing on its scope. It writes no
/// invalidation and drives no concurrency, so neither §3.1 clause
/// `feed-completeness` owns is touched here. It dispatches no bounded replay,
/// and it compares no two positions — DESIGN's `feed-position-bounded` row is
/// the one about a position's size.
///
/// It also asserts nothing about the *refusal* side of the retention contract.
/// That a cursor whose continuation a sweep truncated is refused is DESIGN's
/// `feed-retention-refusal` row, and this check reaches only the exemption from
/// it.
pub async fn feed_bootstrap_position(
    plugin: &dyn UsageCollectorPluginV1,
    retention: Option<&dyn ContractRetention>,
) -> Vec<ContractViolation> {
    let fixtures = match feed_bootstrap_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own `{FEED_BOOTSTRAP_POSITION}` \
                     fixtures, so nothing was submitted. This is a fault in the suite, not in \
                     the plugin under test: {detail}"
                ),
            )];
        }
    };

    // A refusal stops the check. Three of the four assertions below are
    // statements about where a read over a stocked subscription begins, and
    // over an empty one they hold vacuously; a check that passes because
    // nothing was stored is the one outcome worse than a failing one.
    let mut violations = submit(
        plugin,
        &fixtures.main,
        &[fixtures.purged.clone(), fixtures.retained.clone()],
        "the pair this check bootstraps over",
    )
    .await;
    if !violations.is_empty() {
        return violations;
    }

    violations.extend(a_bootstrap_read_begins_at_the_oldest_entry(plugin, &fixtures).await);
    violations.extend(a_subscription_retaining_nothing_is_served(plugin, &fixtures).await);

    if let Some(retention) = retention {
        violations.extend(
            a_bootstrap_read_after_a_purge_begins_at_what_is_left(plugin, retention, &fixtures)
                .await,
        );
        violations.extend(restore_this_checks_own_meter(plugin, retention, &fixtures).await);
    }
    violations
}

/// Assertions one and two: a read from `FeedStart::Oldest` delivers
/// something, and what it delivers first is this check's first entry.
///
/// The walk stops at the first delivery, which makes the assertion about the
/// starting point rather than about how a backend divides a span into pages:
/// only a backend that never delivers at all began at the head.
async fn a_bootstrap_read_begins_at_the_oldest_entry(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FeedBootstrapFixtures,
) -> Vec<ContractViolation> {
    let delivered = match bootstrap(plugin, &fixtures.main).await {
        Ok(delivered) => delivered,
        Err(detail) => return vec![violation(FEED_BOOTSTRAP_POSITION, detail)],
    };

    let mut violations = Vec::new();
    if delivered.is_empty() {
        violations.push(violation(
            FEED_BOOTSTRAP_POSITION,
            format!(
                "a feed read from `FeedStart::Oldest` over a subscription naming this check's \
                 own meter, which holds two settled entries this read's scope admits, followed \
                 its cursor to the head and delivered nothing. `FeedStart::Oldest` is the \
                 oldest entry the subscription retains, never the head. This is the shape a \
                 backend takes when it reads an absent cursor as `start from now`: every \
                 consumer that has never held a position is handed an empty stream and a \
                 cursor at the head, and the entries already on the ledger are never rated by \
                 anybody. The two entries it did not deliver are {expected:?}.",
                expected = vec![fixtures.purged.id, fixtures.retained.id],
            ),
        ));
    }

    if delivered.first().map(|entry| entry.id) != Some(fixtures.purged.id) {
        violations.push(violation(
            FEED_BOOTSTRAP_POSITION,
            format!(
                "a feed read from `FeedStart::Oldest` over a subscription naming this check's \
                 own meter was followed to its first delivery and delivered {delivered:?}; the \
                 oldest entry that subscription retains is {oldest}. `FeedStart::Oldest` \
                 begins at the oldest \
                 entry the subscription retains: a read that begins anywhere else skips the \
                 entries in front of it, and a consumer bootstrapping from it never rates \
                 them. The other entry on this meter is {other}, which this check submitted \
                 second and over a later covered period.",
                delivered = ids(&delivered),
                oldest = fixtures.purged.id,
                other = fixtures.retained.id,
            ),
        ));
    }
    violations
}

/// Assertion three: a subscription naming a meter nothing has ever been
/// written to answers, carries nothing, and carries a cursor.
///
/// *"A subscription retaining no entries returns an empty page carrying a head
/// cursor."* Reported separately for each thing the clause forbids — a refusal,
/// an invented entry, and a page that tells the consumer it has finished — so a
/// plugin author fixing one is not told about the others in the same sentence.
/// The last is easiest to pass over: [`FeedPage::next`](crate::feed::FeedPage::next)
/// is *"`Some` on every page of a live read, short pages included; `None` once a
/// bounded replay has reached its `until`"*, so a live read answering `None`
/// tells a consumer that a stream it has not started following has already
/// ended.
///
/// **This clause is read off `FeedStart::Oldest` rather than off a resumed
/// read**, which makes it this check's rather than `feed-completeness`'s: that
/// check's row is about *"a subscription that stays quiet while its consumer
/// keeps reading"*, a consumer that already holds a position.
async fn a_subscription_retaining_nothing_is_served(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FeedBootstrapFixtures,
) -> Vec<ContractViolation> {
    let subscription = [fixtures.empty.clone()];
    let page = match plugin
        .read_feed_page(
            &subscription,
            &contract_scope(),
            FeedStart::Oldest,
            None,
            FEED_BOOTSTRAP_PAGE_LIMIT,
        )
        .await
    {
        Ok(page) => page,
        Err(err) => {
            return vec![violation(
                FEED_BOOTSTRAP_POSITION,
                format!(
                    "a feed read from `FeedStart::Oldest` over a subscription naming only this \
                     check's empty meter, which nothing has ever been written to, was refused: \
                     {err}. A subscription retaining no entries returns an empty page carrying \
                     a head cursor. A consumer bootstrapping a subscription cannot know in \
                     advance whether anything has been metered under it yet, and one whose \
                     first read fails has no way to tell an empty subscription from a broken \
                     feed."
                ),
            )];
        }
    };

    let mut violations = Vec::new();
    if !page.entries.is_empty() {
        violations.push(violation(
            FEED_BOOTSTRAP_POSITION,
            format!(
                "a feed read from `FeedStart::Oldest` over a subscription naming only this \
                 check's empty meter delivered {delivered:?}. Nothing has ever been written to \
                 that meter, and a feed page carries the entries of the subscription it was \
                 asked for: an entry here is one this consumer is not subscribed to and would \
                 rate anyway.",
                delivered = ids(&page.entries),
            ),
        ));
    }
    if page.next.is_none() {
        violations.push(violation(
            FEED_BOOTSTRAP_POSITION,
            "a feed read from `FeedStart::Oldest` over a subscription naming only this check's \
             empty meter carried no continuation. A subscription retaining no entries returns \
             an empty page carrying a head cursor: a live read carries one on every page, \
             short and empty pages included, and an absent one is reserved for a bounded \
             replay reaching its `until`, and this read sent no `until` at all. What the \
             consumer loses is its whole place in the stream. It has no position to resume \
             from, so when the first entry is metered under this subscription it either \
             bootstraps again and re-rates whatever has settled in between, or gives up."
                .to_owned(),
        ));
    }
    violations
}

/// Assertion four: retention is driven over this check's own meter, and the
/// bootstrap read that follows is served and begins at what is left.
///
/// **Only this assertion needs the drive**, and it is why
/// [`FEED_BOOTSTRAP_POSITION`] is in
/// [`RETENTION_DRIVEN_CHECKS`](super::super::RETENTION_DRIVEN_CHECKS).
///
/// The floor is [`FEED_BOOTSTRAP_RETAINED_WINDOW_END`], the second entry's own
/// covered-period end: `drop_before`'s bound is exclusive, so the drop takes the
/// first entry and leaves the second, and *"the oldest entry the subscription
/// retains"* moves from one to the other. A read that still answers the first is
/// a backend whose `Oldest` is a stored constant rather than a lookup.
///
/// A failure to drive is a [`HARNESS_FAULT`] rather than a plugin violation: a
/// backend that could not be driven has not answered an SPI call wrongly.
async fn a_bootstrap_read_after_a_purge_begins_at_what_is_left(
    plugin: &dyn UsageCollectorPluginV1,
    retention: &dyn ContractRetention,
    fixtures: &FeedBootstrapFixtures,
) -> Vec<ContractViolation> {
    if let Err(detail) = retention
        .drop_before(&fixtures.main, FEED_BOOTSTRAP_RETAINED_WINDOW_END)
        .await
    {
        return vec![violation(
            HARNESS_FAULT,
            format!(
                "the contract suite could not drive this backend's retention over its own \
                 `{FEED_BOOTSTRAP_POSITION}` meter, so the entry the next read is asserted not \
                 to begin at was never removed and the assertion would have held for the wrong \
                 reason. This is a fault in the drive, not an answer the plugin gave to an SPI \
                 call: {detail}"
            ),
        )];
    }

    let delivered = match bootstrap(plugin, &fixtures.main).await {
        Ok(delivered) => delivered,
        Err(detail) => {
            return vec![violation(
                FEED_BOOTSTRAP_POSITION,
                format!(
                    "{detail} The read was taken from `FeedStart::Oldest` immediately after \
                     this check drove retention over its own meter, and `FeedStart::Oldest` is \
                     never refused on the retention floor: it asks for no particular \
                     continuation, only for whatever the subscription still retains, so a \
                     sweep cannot have truncated anything it asked for. A backend that refuses \
                     here has applied the cursor refusal to the one start mode DESIGN exempts \
                     from it, and a consumer bootstrapping after a sweep can then never start \
                     at all."
                ),
            )];
        }
    };

    if delivered.first().map(|entry| entry.id) == Some(fixtures.retained.id) {
        return Vec::new();
    }
    vec![violation(
        FEED_BOOTSTRAP_POSITION,
        format!(
            "retention was driven over this check's own meter to a floor above the covered \
             period of the entry {purged} and at the covered period of the entry {retained}, \
             which removes the first and keeps the second; a feed read from `FeedStart::Oldest` \
             was then followed to its first delivery and delivered {delivered:?}. \
             `FeedStart::Oldest` begins at the oldest entry the subscription retains, and after \
             a sweep that is a different entry from the one it was before - here it is \
             {retained}. A read that delivers some other entry has resolved its start from a \
             position it recorded before the sweep rather than from what it still holds. A read \
             that delivers nothing has resolved it past the oldest entry left, which is what \
             taking the retention floor inclusively costs: the floor names what was removed, \
             and the first entry above it is the one a consumer is owed. Either way the oldest \
             thing this backend still holds is handed to nobody.",
            purged = fixtures.purged.id,
            retained = fixtures.retained.id,
            delivered = ids(&delivered),
        ),
    )]
}

/// Puts this check's own meter back the way a later run needs to find it:
/// empty of both entries, so that re-delivering them inserts them afresh and
/// in order.
///
/// **This is the price of being the check that purges**, paid here rather than
/// left to the next run to discover. The suite's premise is that a repeated run
/// re-delivers identical entries and a conforming backend absorbs them; a purged
/// entry is no longer there to be absorbed, so re-delivering it is an insert,
/// and an insert joins the feed at the head rather than back where it was. The
/// second run would find this check's *first* entry last in its meter's order.
///
/// So the meter is emptied and rewritten: the drop is driven to
/// [`FEED_BOOTSTRAP_RESTORING_FLOOR`], above both covered periods, and both
/// entries are re-delivered in submission order.
///
/// **A restoration, not a rollback.** The positions the backend issued before
/// are not reissued and nothing asks them to be. What is restored is the only
/// thing any assertion here reads: the order of this check's two entries
/// relative to each other on its own meter.
///
/// A refusal is reported against the check rather than as a harness fault: the
/// submission is identical to the one the backend accepted moments earlier, so a
/// backend that will not take it back has broken something — and that would
/// otherwise show up as a *later* run failing an assertion about an order this
/// run was supposed to leave behind.
async fn restore_this_checks_own_meter(
    plugin: &dyn UsageCollectorPluginV1,
    retention: &dyn ContractRetention,
    fixtures: &FeedBootstrapFixtures,
) -> Vec<ContractViolation> {
    if let Err(detail) = retention
        .drop_before(&fixtures.main, FEED_BOOTSTRAP_RESTORING_FLOOR)
        .await
    {
        return vec![violation(
            HARNESS_FAULT,
            format!(
                "the contract suite could not drive this backend's retention over its own \
                 `{FEED_BOOTSTRAP_POSITION}` meter a second time, so that meter is left \
                 holding one of this check's two entries and the other would be re-delivered \
                 into a later run as an insert behind it. This is a fault in the drive, not an \
                 answer the plugin gave to an SPI call: {detail}"
            ),
        )];
    }
    submit(
        plugin,
        &fixtures.main,
        &[fixtures.purged.clone(), fixtures.retained.clone()],
        "the pair this check restores its own meter with",
    )
    .await
}

/// Follows a feed read from `FeedStart::Oldest` over one meter until it
/// delivers something or reaches the head.
///
/// The shared walk rather than a single page read, because a short page is
/// conforming: see [`FEED_BOOTSTRAP_PAGE_LIMIT`] and
/// [`super::super::feed_walk`]. `Err` carries a ready-to-report detail.
async fn bootstrap(
    plugin: &dyn UsageCollectorPluginV1,
    meter: &MeterRef,
) -> Result<Vec<StoredUsageRecord>, String> {
    let subscription = [meter.clone()];
    feed_walk(
        plugin,
        &subscription,
        &contract_scope(),
        FeedStart::Oldest,
        FEED_BOOTSTRAP_PAGE_LIMIT,
        WalkStop::FirstDelivery,
    )
    .await
    .map(|walk| walk.delivered)
}

/// Submits `entries` in order, reporting any refusal.
///
/// Every entry is submitted whatever the ones before it did: a submission that
/// stopped at its first refusal would leave a shorter ledger than the report
/// describes. `role` names what the submission was for, a refused first delivery
/// and a refused restoration being different failures.
async fn submit(
    plugin: &dyn UsageCollectorPluginV1,
    meter: &MeterRef,
    entries: &[StoredUsageRecord],
    role: &str,
) -> Vec<ContractViolation> {
    let mut violations = Vec::new();
    for entry in entries {
        if let Err(err) = seed_usage_record(plugin, meter, entry.clone()).await {
            violations.push(violation(
                FEED_BOOTSTRAP_POSITION,
                format!(
                    "`create_usage_records` refused {role} (entry {id}, covered period ending \
                     {window_end}), so this check's own meter does not hold the ledger its \
                     assertions are about: {err}",
                    id = entry.id,
                    window_end = entry.window_end,
                ),
            ));
        }
    }
    violations
}

/// Builds the two entries and the two meters.
///
/// The entries differ in their covered period and in nothing else that matters:
/// one acceptance instant, one tenant, one quantity, one resource. The covered
/// period is what the purge is keyed on, and holding everything else fixed keeps
/// the purge the only thing that separates them.
///
/// [`FeedBootstrapFixtures::guards`] keeps the check from passing by
/// construction; being the suite's own facts rather than the plugin's, each
/// guard reports a [`HARNESS_FAULT`].
fn feed_bootstrap_fixtures() -> Result<FeedBootstrapFixtures, String> {
    let main = check_meter(FEED_BOOTSTRAP_POSITION, "main")?;
    let empty = check_meter(FEED_BOOTSTRAP_POSITION, "empty")?;
    let quantity = UsageQuantity::parse(FEED_BOOTSTRAP_QUANTITY).map_err(|err| {
        format!("the check's own quantity `{FEED_BOOTSTRAP_QUANTITY}` does not parse: {err}")
    })?;

    let purged = fixture_record_on(
        &main,
        CONTRACT_TENANT_ID,
        &feed_bootstrap_key("purged")?,
        quantity,
        CONTRACT_ACCEPTED_AT,
        FEED_BOOTSTRAP_WINDOW_FROM,
        FEED_BOOTSTRAP_PURGED_WINDOW_END,
    )?;
    let retained = fixture_record_on(
        &main,
        CONTRACT_TENANT_ID,
        &feed_bootstrap_key("retained")?,
        quantity,
        CONTRACT_ACCEPTED_AT,
        FEED_BOOTSTRAP_PURGED_WINDOW_END,
        FEED_BOOTSTRAP_RETAINED_WINDOW_END,
    )?;

    let fixtures = FeedBootstrapFixtures {
        main,
        empty,
        purged,
        retained,
    };
    fixtures.guards()?;
    Ok(fixtures)
}

impl FeedBootstrapFixtures {
    /// The facts this check's assertions read, established rather than
    /// assumed.
    ///
    /// * **The two meters are two.** A subscription naming one meter twice
    ///   would make assertion 3 a second reading of the stocked meter, and
    ///   it would then require the two entries this check wrote not to be
    ///   delivered.
    /// * **The two entries derive two ids.** They do — the idempotency key and
    ///   both covered-period bounds are identity inputs — but if they collided,
    ///   the second submission would be absorbed as a retry of the first and a
    ///   meter this check believes holds two entries would hold one. Assertions
    ///   1 and 4 would then both name the one entry there is and both pass
    ///   whatever the backend did with `FeedStart::Oldest`.
    /// * **The purge takes exactly one of the two.** The first entry's
    ///   covered period ends strictly before the floor the purge is driven
    ///   to and the second's ends exactly at it, which is what
    ///   `drop_before`'s exclusive bound turns into "one goes, one stays".
    ///   A floor at or below the first would remove nothing and assertion 4
    ///   would assert the same thing as assertion 1; a floor above the
    ///   second would empty the meter and assertion 4 would have no entry to
    ///   name.
    /// * **The two entries carry one acceptance instant.** Nothing here wants a
    ///   feed ordered by the gateway-stamped instant to be caught — DESIGN §3.10
    ///   forbids that ordering and `feed-completeness` owns the assertion that
    ///   catches it. Agreeing on `accepted_at` gives a backend ordering by it
    ///   nothing to get wrong, keeping that subject under the one check whose
    ///   row is about it.
    fn guards(&self) -> Result<(), String> {
        if self.main == self.empty {
            return Err(format!(
                "this check's stocked and empty meters are one value ({main}), so the \
                 subscription it never writes to is the one it wrote two entries to and the \
                 empty-subscription probe would require those entries not to be delivered",
                main = self.main.id.as_str(),
            ));
        }
        if self.purged.id == self.retained.id {
            return Err(format!(
                "this check's two entries derive one id ({id}); the second submission would be \
                 absorbed as a retry of the first, and a meter the assertions treat as holding \
                 two entries would hold one",
                id = self.purged.id,
            ));
        }
        if !(self.purged.window_end < FEED_BOOTSTRAP_RETAINED_WINDOW_END
            && self.retained.window_end == FEED_BOOTSTRAP_RETAINED_WINDOW_END
            && self.retained.window_end < FEED_BOOTSTRAP_RESTORING_FLOOR)
        {
            return Err(format!(
                "this check's purge floor does not separate its two entries. The floor is \
                 `{floor}`, the entry it must remove covers a period ending `{purged}` and the \
                 entry it must keep covers one ending `{retained}`; the drive removes an entry \
                 whose period ends strictly before the floor, so the first has to end below it \
                 and the second exactly at it. The restoring floor `{restoring}` must then be \
                 above both",
                floor = FEED_BOOTSTRAP_RETAINED_WINDOW_END,
                purged = self.purged.window_end,
                retained = self.retained.window_end,
                restoring = FEED_BOOTSTRAP_RESTORING_FLOOR,
            ));
        }
        if self.purged.accepted_at != self.retained.accepted_at {
            return Err(format!(
                "this check's two entries carry the acceptance instants `{purged}` and \
                 `{retained}`, which differ. They are held equal so that a backend ordering its \
                 feed by the gateway-stamped instant, which DESIGN section 3.10 forbids, has \
                 nothing here to get wrong: that ordering is `feed-completeness`'s to catch, \
                 and a second check catching it would report one mistake under two names",
                purged = self.purged.accepted_at,
                retained = self.retained.accepted_at,
            ));
        }
        Ok(())
    }
}

/// The idempotency key one of this check's entries submits under.
///
/// Keyed on the role name for the reason
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture) gives: in
/// a ledger with no delete path, an edited fixture must take a fresh identity
/// rather than inherit an accepted entry's. **This check does have a delete
/// path**, and the reasoning survives it: the delete is driven over a
/// covered-period floor rather than over an identity.
fn feed_bootstrap_key(role: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{FEED_BOOTSTRAP_POSITION}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}
