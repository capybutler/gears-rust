//! The DESIGN §3.3 `feed-bootstrap-position` check.
//!
//! See [`feed_bootstrap_position`] for what it asserts. The module holds the
//! two entries it writes, the two meters it names, and the one purge it
//! drives. The reads it makes are [`super::super::feed_walk`]'s, shared with
//! [`feed_snapshot_and_replay`](super::feed_snapshot_and_replay()) and
//! [`feed_completeness`](super::feed_completeness()).
//!
//! **It is the first check in the suite that removes anything**, and the
//! first whose coverage under [`run_all`](super::super::run_all) is
//! partial. One of its four assertions needs a backend's retention to have
//! actually swept, which no method on [`UsageCollectorPluginV1`] performs;
//! the drive is [`ContractRetention`], beside the SPI rather than on it,
//! and it arrives through
//! [`run_all_with_retention`](super::super::run_all_with_retention).
//! Handed `None` the check runs its other three and skips that one.
//! [`RETENTION_DRIVEN_CHECKS`](super::super::RETENTION_DRIVEN_CHECKS) is
//! the list of the checks in that position, and it rather than this
//! sentence is what a caller reporting coverage reads.

use crate::contract::feed_walk::{WalkStop, feed_walk, ids};
use crate::contract::fixtures::{
    CONTRACT_ACCEPTED_AT, CONTRACT_TENANT_ID, FIXTURE_EPOCH, check_meter, contract_scope,
    fixture_record_on, violation,
};
use crate::contract::retention::ContractRetention;
use crate::contract::{ContractViolation, FEED_BOOTSTRAP_POSITION, HARNESS_FAULT};
use crate::feed::FeedStart;
use crate::models::{IdempotencyKey, MeterTypeId, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;

/// The start of the covered period this check's entries are offset from:
/// **four hundred and twenty days** past [`FIXTURE_EPOCH`], for the reason
/// [`WINDOW_SELECTION_FROM`](super::window_end_selection::WINDOW_SELECTION_FROM)
/// gives.
///
/// **Which offsets the other checks hold is deliberately not enumerated
/// here.** Seven modules in `super` carry such a list, each written as the
/// offsets taken at the moment that check landed, and every one of the
/// seven has to be edited by every check that lands afterwards. **None of
/// them has been**: all seven stop at day 360, and day 390 has been taken
/// since `feed-completeness` landed. A list that is wrong is worse than no
/// list, because it reads as a guarantee of separation that nobody is
/// keeping. What this constant states instead is its own value and why the
/// separation exists at all, which is a fact about this module and stays
/// true however many checks land beside it — the shape
/// [`feed_completeness`](super::feed_completeness()) already took, in
/// fewer words. A structural replacement, the offsets derived rather than
/// written down, is proposed for this slice's closeout; until then the
/// seven are left as they are rather than corrected in passing, because
/// correcting them is the work the replacement exists to stop.
///
/// The separation buys as little here as it does for
/// [`feed_completeness`](super::feed_completeness()), and for the same
/// reason: **this check dispatches no range at all.** Every read it makes is
/// a feed page, and a feed page selects by subscription rather than by
/// covered period, so the separation that does the work is [`check_meter`].
/// The offset is kept because the separation runs the other way as well, and
/// **this check is the one place that matters most**: it is the only check
/// that *purges*, its purge is keyed on a covered-period floor, and a floor
/// that reached into another check's period would take that check's fixtures
/// with it. That the drive is also keyed on a GTS type is the belt to this
/// brace — [`ContractRetention::drop_before`] says why it has to be.
const FEED_BOOTSTRAP_WINDOW_FROM: time::OffsetDateTime =
    FIXTURE_EPOCH.saturating_add(time::Duration::days(420));

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
/// Its value is asserted nowhere: every assertion here compares identities
/// or reads a page's shape. It is exactly representable in a binary float
/// all the same, which keeps `contract_mutants`'s `Defect::QuantityThroughFloat`
/// from changing anything on the way in and so from reaching this check for
/// a rule that belongs to `quantity-round-trip`.
const FEED_BOOTSTRAP_QUANTITY: &str = "1.5";

/// The page limit every read in this check dispatches: **one**.
///
/// The row is about where a read *begins*, and a page wide enough to carry
/// the whole subscription answers that only incidentally — it would carry
/// the oldest entry whether the read began there or two entries earlier. At
/// a limit of one the first entry a read delivers is the first entry it
/// could have delivered, so nothing but the starting point decides it.
///
/// It is not an assumption that one entry arrives on the first page. A short
/// page is conforming, and the reads below are **followed** for exactly that
/// reason; see [`a_bootstrap_read_begins_at_the_oldest_entry`].
const FEED_BOOTSTRAP_PAGE_LIMIT: u64 = 1;

/// The two entries this check writes and the two meters it reads over.
struct FeedBootstrapFixtures {
    /// The meter both entries are written to, and the meter the purge is
    /// driven over.
    main: MeterTypeId,
    /// The meter this check subscribes to and never writes to.
    ///
    /// DESIGN's row asks what a *"subscription retaining no entries"*
    /// answers, and a meter derived for this check's exclusive use is the
    /// only way to have one: `run_all` dispatches every check against one
    /// backend, so any meter another check writes to is empty only by luck
    /// of dispatch order.
    empty: MeterTypeId,
    /// The older of the two, submitted first, and the one the purge removes.
    purged: UsageRecord,
    /// The newer of the two, submitted second. It survives the purge and is
    /// then the oldest entry the subscription retains.
    retained: UsageRecord,
}

/// `feed-bootstrap-position` — *"`FeedStart::Oldest` begins at the oldest
/// entry the subscription retains, never at the head, and is never refused
/// on the retention floor. A subscription retaining no entries returns an
/// empty page carrying a head cursor."* (DESIGN §3.3, "Plugin contract
/// tests", line 1324.)
///
/// **The whole row is about one argument**, which is why it is one check:
/// `FeedStart` is named rather than inferred, and `Oldest` is the name for
/// the one place a consumer with no position of its own can start. DESIGN
/// §3.1's `FeedStart` row fixes what the name means — *"`Oldest` is the
/// oldest entry the subscription retains, `After(position)` continues from a
/// position the feed issued. … v1 admits these two, and neither begins at
/// the head"* — and the SPI restates it for implementors on
/// [`read_feed_page`](UsageCollectorPluginV1::read_feed_page):
/// *"`FeedStart::Oldest` means the oldest position this plugin still serves
/// for `subscription` under `scope` — never the head."*
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
/// 4. **After a purge, `Oldest` is still not refused and begins at the
///    oldest entry that is left.** Retention is driven to a floor that takes
///    the first entry and leaves the second; the read that follows must
///    answer, and must answer with the second.
///
/// **One and two are two reports rather than one** because they are two
/// things to tell a plugin author. *"It returned the wrong entry"* is a
/// backend whose feed order or whose start position is wrong; *"it returned
/// nothing"* is a backend that treated an absent cursor as "start from now"
/// and will hand a bootstrapping consumer an empty stream forever. A reader
/// should not have to tell those apart from one message, and a backend that
/// makes the second mistake makes the first as a consequence, so both fire.
///
/// **Four is the only one that needs a drive**, and the one that makes this
/// check's coverage under [`run_all`](super::super::run_all) partial. See
/// "The purge" below.
///
/// # The reads are followed, and why at a limit of one
///
/// `limit` bounds what a page *carries* and promises nothing about
/// everything settled fitting on one, so a short page is conforming: a
/// backend that answered an empty first page and delivered on the second
/// would be paging in small steps, not starting at the head. The two reads
/// that ask *where a bootstrap begins* — assertions 1 and 2, and assertion
/// 4 after the purge — are therefore [`super::super::feed_walk`]'s
/// [`WalkStop::FirstDelivery`] walks rather than single page reads: the
/// cursor is followed until something arrives or until it stands still, and
/// *"nothing ever arrived"* is what says a read began at the head.
///
/// The third read is assertion 3's, and it reads **one page** directly. Its
/// clause is about a page's own shape — *"returns an empty page carrying a
/// head cursor"* — and a walk would answer a different question: it would
/// fold the continuation into its own exit condition and report an absent
/// one as a failed walk, leaving nothing for this check to say about which
/// of the three obligations a backend broke.
///
/// # The purge, and what it costs a run that cannot drive one
///
/// Assertion 4 needs retention to have actually removed something, and no
/// method on [`UsageCollectorPluginV1`] removes anything — DESIGN declares
/// none, and [`retention`](super::super::retention)'s module docs say why
/// the drive sits beside the SPI instead. [`run_all`](super::super::run_all)
/// therefore runs three quarters of this check and
/// [`run_all_with_retention`](super::super::run_all_with_retention) runs all
/// of it. A green `run_all` says nothing about assertion 4, and
/// [`RETENTION_DRIVEN_CHECKS`](super::super::RETENTION_DRIVEN_CHECKS) is the
/// constant a caller reporting coverage reads to find that out.
///
/// That claim is held mechanically rather than by this paragraph:
/// `contract_tests`' `RETENTION_DRIVEN_MATRIX` carries two rows whose only
/// defect is one assertion 4 reaches, and
/// `each_driven_check_fails_against_its_own_defect_and_no_other` requires
/// both to pass under `run_all` and to fail under `run_all_with_retention`.
///
/// # Surviving a repeated run
///
/// Both entry points are dispatched against one persistent backend, and two
/// tests require a second dispatch to be as green as the first:
/// `the_reference_backend_conforms_to_a_repeated_run` over
/// `super::super::run_all`, and
/// `the_reference_backend_conforms_to_a_repeated_run_under_a_retention_drive`
/// over `super::super::run_all_with_retention`. The second landed with this
/// check and is the one that bites here. **This is the first check that
/// cannot survive a repeat by keying its identities alone**, and the reason
/// is worth stating exactly, because it is the one thing a purging check
/// has to get right.
///
/// Every other check re-delivers its entries on a later run and a conforming
/// backend absorbs them, so the ledger it reads is the ledger it read before.
/// A purged entry is not absorbed: it is gone, so re-delivering it is an
/// **insert**, and an insert lands at the head of the feed's order rather
/// than back where it was. A check that purged its oldest entry and then
/// re-delivered it would, on its second run, find that entry at the *end* of
/// its meter's order — and *"the oldest entry the subscription retains"*
/// would name the other one. No assertion naming a fixture survives that.
///
/// **So the check restores its own meter.** After assertion 4 it drives a
/// second drop, above both entries this time, and re-delivers both in order.
/// Both are inserts, both are fresh, and their order is this check's own
/// again. [`restore_this_checks_own_meter`] is where that happens and says
/// what it is and is not.
///
/// **That restoration is what buys a discipline this check cannot keep.**
/// [`feed_snapshot_and_replay`](super::feed_snapshot_and_replay()) states
/// four, and this check keeps two of them outright — every identity is
/// keyed on its role, and no assertion names a ledger length, a page count
/// or a position value. The third is *"no assertion names where in the
/// delivered order an entry falls"*, and **this check is the one whose
/// whole subject is exactly that**: DESIGN's row is about which entry a
/// read begins at. It cannot avoid the assertion, so it keeps the order
/// stable instead — which is a stronger obligation than the discipline it
/// replaces, and the reason the restoration is not optional. The fourth is
/// about a bounded replay, and this check dispatches none.
///
/// The restoration is driven through the same capability as the purge, so a
/// run handed no driver neither purges nor restores — and has nothing to
/// restore, because without a drive nothing is ever removed and the ordinary
/// absorb keeps the ledger where it was.
///
/// # Which assertions a subject reaches, measured
///
/// Every assertion below was inverted and confirmed to fire against the
/// reference backend, so none of the fifteen is dead. What each *catches*
/// was then measured by neutering it and re-running the discrimination
/// matrix and the two driven subject tests, and the result bounds what this
/// check's rows establish. Three subjects reach it, all three in
/// `contract_mutants`:
///
/// * **`Defect::AFeedBootstrapReadStartsAtTheHead`** — the subject built for
///   this check's undriven half — is reported by assertions 1 and 2, and by
///   **neither of them alone**. It delivers nothing, so the page carries no
///   entry *and* the entry it carries is not the oldest; neutering either
///   leaves the other reporting and the row unchanged, and neutering both
///   takes this check out of that subject's row altogether. They stay as two
///   because they are two things to tell a plugin author, and the pair is
///   what is load-bearing.
///
///   That subject reaches three other checks as well — every check in the
///   suite that bootstraps a feed read, which is every check that reads one.
///   `contract_tests`' `DISCRIMINATION_MATRIX` carries that argument.
/// * **`Defect::RefusesTheOldestStartAfterASweep`** is reported by assertion
///   4's refusal report **alone**, which makes it individually
///   load-bearing: neutering it leaves that subject reported by nothing at
///   all, under either entry point. It is also the subject the structural
///   guard on
///   [`RETENTION_DRIVEN_CHECKS`](super::super::RETENTION_DRIVEN_CHECKS)
///   turns on.
/// * **`Defect::SkipsTheOldestEntryASweepLeft`** is reported by assertion
///   4's position report **alone**, and is individually load-bearing the
///   same way. It exists because the two halves of the row's retention
///   clause are two claims: a read that is refused never answers, and a read
///   that answers can still answer from the wrong place. Neither subject
///   reaches the other's assertion, which is the measurement rather than the
///   design intent.
///
/// **Neither driven subject is in the discrimination matrix**, and neither
/// could be: `run_all` drives nothing, so under the matrix's dispatch both
/// are behaviourally the reference backend. Their columns are the first two
/// rows of `contract_tests`' `RETENTION_DRIVEN_MATRIX`.
///
/// # Eleven assertions no subject reaches, and why each stays
///
/// Neutering any of these changed no row of the matrix and neither driven
/// subject's result, which is the measurement rather than an expectation.
///
/// * **The walk's own failure report**, in
///   [`a_bootstrap_read_begins_at_the_oldest_entry`]. It fires when a live
///   feed read is refused outright or when its cursor never stands still,
///   and a subject that produced either would produce it for every check
///   that walks — the report comes from [`super::super::feed_walk`], which
///   is a reader rather than a check and says so. **A recorded gap shared
///   with every caller of that walk**, and not this check's to close alone.
/// * **Assertion 3's three parts** — refused, invented, closed.
///   `feed-completeness` recorded these as gaps with this check named as
///   their owner; the owner has landed, it has taken the `FeedStart::Oldest`
///   reading of them, and **the gap has moved here rather than closed**. A
///   subject would have to answer differently for a subscription with
///   nothing under it, which is a backend reading its subscription's shape
///   rather than evaluating it. The shape-reading defect that is plausible —
///   a page limit applied once per subscribed meter and the results merged —
///   needs a second meter with entries in it, and this check's second meter
///   is empty by definition of the clause it exists for. One candidate does
///   exist for the *refusal* part, and is named rather than built: a backend
///   that resolves a subscription through a per-type feed-order row and
///   answers `Internal` when a type has none yet. It would reach
///   `feed-completeness`'s quiet probe at the same time, so it is a
///   two-check row rather than an isolating one, and it is recorded here for
///   whoever decides that trade.
/// * **The two drive-failure reports**, in
///   [`a_bootstrap_read_after_a_purge_begins_at_what_is_left`] and
///   [`restore_this_checks_own_meter`]. **No subject can reach these and it
///   is not for want of one.** They fire when
///   [`ContractRetention::drop_before`] answers `Err`, which is the suite
///   failing to set a scenario up rather than a backend answering an SPI
///   call — which is exactly why both are reported as
///   [`HARNESS_FAULT`] and why that error type
///   is a `String` rather than a plugin error. A subject built to fail a
///   drive would be a broken harness wearing a backend's name. And the
///   opposite subject, one whose drive quietly removes nothing, is not
///   non-conforming at all: a backend that retains more than it was asked to
///   is a backend that retains, and every assertion here is about what it
///   *retains*. **Unreachable by construction, recorded rather than
///   removed**, because a drive that starts failing silently would leave
///   assertion 4 passing over a ledger nothing had been removed from.
/// * **The submission report** in [`submit`]. No subject in the suite
///   refuses a well-formed record carrying a fresh idempotency key: the two
///   that refuse anything refuse a *collision*, and this check submits none.
///   **A recorded gap shared with every other check's submission guard** —
///   each has the same one, each for the same reason, and the value of all
///   of them is that a check which lost its fixtures says so instead of
///   asserting over an empty ledger.
/// * **The four fixture guards** in [`FeedBootstrapFixtures::guards`]. They
///   are the suite's own facts rather than the plugin's — two meters, two
///   ids, a floor that separates them, one acceptance instant — so no
///   backend outcome can make one fire and no subject could be built that
///   did. **Unreachable by any plugin, deliberately**, which is what
///   `HARNESS_FAULT` is for. They fire for an edit to this module, which is
///   the only thing that can break them and the reason they are here.
///
/// # What this check does not reach
///
/// It writes under one tenant and reads under a grant that admits it, so it
/// asserts nothing about a feed page narrowing on its scope. It writes no
/// invalidation and drives no concurrency, so neither §3.1 clause
/// `feed-completeness` owns is touched here. It dispatches no bounded
/// replay, and it compares no two positions — DESIGN's `feed-position-bounded`
/// row is the one about a position's size.
///
/// It also asserts nothing about the *refusal* side of the retention
/// contract. That a cursor whose continuation a sweep truncated is refused
/// is DESIGN's `feed-retention-refusal` row, and this check reaches only the
/// exemption from it.
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
/// The walk stops at the first delivery, which is what makes the assertion
/// about the starting point rather than about how a backend divides a span
/// into pages: a backend that answers an empty first page and delivers on
/// the second began at the oldest entry and paged short, and only a backend
/// that never delivers at all began at the head.
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
/// *"A subscription retaining no entries returns an empty page carrying a
/// head cursor."* Three reports, because the clause forbids three different
/// things and a plugin author fixing one should not be told about the other
/// two in the same sentence: a refusal, an invented entry, and a page that
/// tells the consumer it has finished. The third is the one a reader is most
/// likely to pass over. [`FeedPage::next`](crate::feed::FeedPage::next)
/// enumerates two dispositions — *"`Some` on every page of a live read,
/// short pages included; `None` once a bounded replay has reached its
/// `until`"* — so a live read that answered `None` would be telling a
/// consumer that a stream it has not started following has already ended,
/// and leaving it no position to come back to.
///
/// **This clause is read off `FeedStart::Oldest` rather than off a resumed
/// read**, which is what makes it this check's rather than
/// `feed-completeness`'s. That check asserts the same three obligations over
/// a quiet meter and says so: its own row is about *"a subscription that
/// stays quiet while its consumer keeps reading"*, which is a consumer that
/// already holds a position, and the bootstrap read it takes first is there
/// only to get one.
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
/// The floor is [`FEED_BOOTSTRAP_RETAINED_WINDOW_END`], which is the second
/// entry's own covered-period end: `drop_before`'s bound is exclusive, so
/// the drop takes the first entry and leaves the second, and *"the oldest
/// entry the subscription retains"* moves from one to the other. A read that
/// still answers the first is a backend whose `Oldest` is a stored constant
/// rather than a lookup.
///
/// A failure to drive is reported as a
/// [`HARNESS_FAULT`] rather than against the
/// plugin, for the reason [`ContractRetention::drop_before`]'s error type
/// gives: a backend that could not be driven has not answered an SPI call
/// wrongly, and putting its name on this would be blaming it for a scenario
/// the suite failed to set up.
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
/// **This is the price of being the check that purges**, and it is paid here
/// rather than left to the next run to discover. The suite's premise is that
/// a repeated run re-delivers identical entries and a conforming backend
/// absorbs them, so the ledger a check reads is the ledger it read before.
/// A purged entry breaks that premise and nothing else in the suite does: it
/// is no longer there to be absorbed, so re-delivering it is an insert, and
/// an insert joins the feed at the head rather than back where it was. The
/// second run of this check would then find its *first* entry last in its
/// meter's own order, and *"the oldest entry the subscription retains"*
/// would name the other one.
///
/// So the meter is emptied and rewritten. The drop is driven to
/// [`FEED_BOOTSTRAP_RESTORING_FLOOR`], above both covered periods, and both
/// entries are then re-delivered in the order the check submits them in. Two
/// inserts, in this check's own order, on a meter nothing else writes to.
///
/// **It is a restoration, not a rollback.** The positions the backend issued
/// before are not reissued and nothing here asks them to be — a position is
/// this plugin's own and a sequence is never reused. What is restored is the
/// only thing any assertion in this module reads: the order of this check's
/// two entries relative to each other on this check's own meter.
///
/// A refusal is reported against the check rather than as a harness fault.
/// The submission is well formed and identical to the one the backend
/// accepted moments earlier, so a backend that will not take it back has
/// broken something — and what it has broken shows up as a *later* run
/// failing an assertion about an order this run was supposed to leave
/// behind, which is the hardest kind of failure to read. Saying it here is
/// what keeps that from happening.
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
    meter: &MeterTypeId,
) -> Result<Vec<UsageRecord>, String> {
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
/// Every entry is submitted whatever the ones before it did: the assertions
/// read a meter both entries belong to, and a submission that stopped at its
/// first refusal would leave a shorter ledger than the report describes.
/// `role` names what the submission was for, because a refused first
/// delivery and a refused restoration are different failures.
async fn submit(
    plugin: &dyn UsageCollectorPluginV1,
    entries: &[UsageRecord],
    role: &str,
) -> Vec<ContractViolation> {
    let mut violations = Vec::new();
    for entry in entries {
        if let Err(err) = plugin.create_usage_record(entry.clone()).await {
            violations.push(violation(
                FEED_BOOTSTRAP_POSITION,
                format!(
                    "`create_usage_record` refused {role} (entry {id}, covered period ending \
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
/// The entries differ in their covered period and in nothing else that
/// matters: one acceptance instant, one tenant, one quantity, one resource.
/// The covered period is what the purge is keyed on, and holding everything
/// else fixed is what keeps the purge the only thing that separates them.
///
/// Four guards keep the check from passing by construction, and all four are
/// the suite's own facts rather than the plugin's, so all four are reported
/// as [`HARNESS_FAULT`]. They are
/// [`FeedBootstrapFixtures::guards`]'.
fn feed_bootstrap_fixtures() -> Result<FeedBootstrapFixtures, String> {
    let main = check_meter(FEED_BOOTSTRAP_POSITION, "main")?;
    let empty = check_meter(FEED_BOOTSTRAP_POSITION, "empty")?;
    let quantity = UsageQuantity::parse(FEED_BOOTSTRAP_QUANTITY).map_err(|err| {
        format!("the check's own quantity `{FEED_BOOTSTRAP_QUANTITY}` does not parse: {err}")
    })?;

    let purged = fixture_record_on(
        main.clone(),
        CONTRACT_TENANT_ID,
        &feed_bootstrap_key("purged")?,
        quantity,
        CONTRACT_ACCEPTED_AT,
        FEED_BOOTSTRAP_WINDOW_FROM,
        FEED_BOOTSTRAP_PURGED_WINDOW_END,
    )?;
    let retained = fixture_record_on(
        main.clone(),
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
    /// The four facts this check's assertions read, established rather than
    /// assumed.
    ///
    /// * **The two meters are two.** A subscription naming one meter twice
    ///   would make assertion 3 a second reading of the stocked meter, and
    ///   it would then require the two entries this check wrote not to be
    ///   delivered.
    /// * **The two entries derive two ids.** They do — the idempotency key
    ///   and the covered period are three of the six identity inputs — but
    ///   if they ever collided, the second submission would be absorbed as a
    ///   retry of the first and a meter this check believes holds two
    ///   entries would hold one. Assertions 1 and 4 would then both name the
    ///   one entry there is, and both would pass whatever the backend did
    ///   with `FeedStart::Oldest`.
    /// * **The purge takes exactly one of the two.** The first entry's
    ///   covered period ends strictly before the floor the purge is driven
    ///   to and the second's ends exactly at it, which is what
    ///   `drop_before`'s exclusive bound turns into "one goes, one stays".
    ///   A floor at or below the first would remove nothing and assertion 4
    ///   would assert the same thing as assertion 1; a floor above the
    ///   second would empty the meter and assertion 4 would have no entry to
    ///   name.
    /// * **The two entries carry one acceptance instant.** Nothing here
    ///   wants a feed ordered by the gateway-stamped instant to be caught —
    ///   DESIGN §3.10 forbids that ordering and `feed-completeness` owns the
    ///   assertion that catches it. Two entries that agree on `accepted_at`
    ///   give a backend ordering by it nothing to get wrong, which keeps
    ///   that subject reported under the one check whose row is about it.
    fn guards(&self) -> Result<(), String> {
        if self.main == self.empty {
            return Err(format!(
                "this check's stocked and empty meters are one value ({main}), so the \
                 subscription it never writes to is the one it wrote two entries to and the \
                 empty-subscription probe would require those entries not to be delivered",
                main = self.main.as_str(),
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
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture) spells
/// out: in a ledger with no delete path, an edited fixture must take a fresh
/// identity rather than inherit an accepted entry's. **This check does have
/// a delete path** — it is the only one that does — and the reasoning
/// survives it unchanged, because the delete is driven over a covered-period
/// floor rather than over an identity and an edited fixture is not the thing
/// it removes.
fn feed_bootstrap_key(role: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{FEED_BOOTSTRAP_POSITION}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}
