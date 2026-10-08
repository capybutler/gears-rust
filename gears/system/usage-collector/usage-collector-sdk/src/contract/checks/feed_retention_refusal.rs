//! The DESIGN §3.3 `feed-retention-refusal` check.
//!
//! See [`feed_retention_refusal`] for what it asserts. The module holds the six
//! entries it writes, the two meters it names, and the two drops it drives. The
//! reads it makes are [`super::super::feed_walk`]'s.
//!
//! **It is the check [`ContractRetention`] was built for.** Every other clause
//! in DESIGN's feed rows can be read off a ledger nothing has been removed from;
//! this one cannot, because [`UsageCollectorPluginError::CursorBeyondRetention`]
//! is unreachable until a sweep has actually removed an entry and no method on
//! [`UsageCollectorPluginV1`] removes one. The drive arrives through
//! [`run_all_with_retention`](super::super::run_all_with_retention); handed
//! `None` the check runs the one assertion that needs no sweep and skips the
//! three that do, as
//! [`RETENTION_DRIVEN_CHECKS`](super::super::RETENTION_DRIVEN_CHECKS) records.

use crate::contract::feed_walk::{WalkStop, feed_walk, ids};
use crate::contract::fixtures::{
    CONTRACT_ACCEPTED_AT, CONTRACT_TENANT_ID, SCOPE_EXCLUDED_TENANT_ID, check_meter,
    check_window_from, contract_scope, fixture_record_on, seed_usage_record, violation,
};
use crate::contract::retention::ContractRetention;
use crate::contract::{ContractViolation, FEED_RETENTION_REFUSAL, HARNESS_FAULT};
use crate::error::UsageCollectorPluginError;
use crate::feed::{FeedPosition, FeedStart};
use crate::models::IdempotencyKey;
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::stored::{MeterRef, StoredUsageRecord};
use uuid::Uuid;

/// The start of the covered period this check's entries are offset from: the
/// offset [`check_window_from`] tables for this check.
///
/// The separation buys little on the read side — **this check dispatches no
/// range at all**, every read is a feed page, and a feed page selects by
/// subscription, so the separation that does the work there is [`check_meter`].
/// The offset is kept because **this check drives two drops**, and a drop keyed
/// on a covered-period floor reaching into another check's period would take
/// that check's fixtures with it. That the drive is also keyed on a GTS type is
/// the belt to this brace — see [`ContractRetention::drop_before`].
const FEED_RETENTION_WINDOW_FROM: time::OffsetDateTime =
    check_window_from(FEED_RETENTION_REFUSAL, "main");

/// The floor the sweep this check's assertions are about is driven to: three
/// hours past [`FEED_RETENTION_WINDOW_FROM`].
///
/// It separates the two entries the sweep removes from the two it leaves.
/// [`ContractRetention::drop_before`]'s bound is exclusive, so an entry whose
/// covered period ends exactly at the floor stays; [`FeedRetentionFixtures::guards`]
/// holds the four entries to their sides of it.
const FEED_RETENTION_SWEPT_FLOOR: time::OffsetDateTime =
    FEED_RETENTION_WINDOW_FROM.saturating_add(time::Duration::hours(3));

/// The floor a **declared retention** would permit this backend to sweep to:
/// five hours past [`FEED_RETENTION_WINDOW_FROM`], two hours above the floor
/// anything is actually driven to.
///
/// **Nothing drives it, and that is the point.** DESIGN §3.1's "Plugin-owned
/// lifecycle" row makes the horizon a lower bound — *"A purge later than the
/// horizon is permitted, which is why the horizon is a floor rather than an
/// exact boundary"* — and §3.3's row then requires a cursor over those entries
/// to be served: *"A cursor whose continuation is intact is served, including
/// one older than the floor where the plugin retains longer than it."*
///
/// [`FEED_RETENTION_BELOW_THE_FLOOR_WINDOW_END`] is the entry between the two
/// floors, and the third assertion reads it: a backend deciding the refusal from
/// a declared floor refuses that cursor; one deciding it from the mark serves it.
const FEED_RETENTION_DECLARED_FLOOR: time::OffsetDateTime =
    FEED_RETENTION_WINDOW_FROM.saturating_add(time::Duration::hours(5));

/// The floor the **emptying** drop is driven to: nine hours past
/// [`FEED_RETENTION_WINDOW_FROM`], above every covered period this check writes
/// to its swept meter. See [`empty_the_swept_meter`] for why a check that purges
/// has to finish by emptying the meter it purged.
const FEED_RETENTION_RESTORING_FLOOR: time::OffsetDateTime =
    FEED_RETENTION_WINDOW_FROM.saturating_add(time::Duration::hours(9));

/// The end of the covered period of the entry the sweep removes **and this
/// read's scope admits**: one hour past [`FEED_RETENTION_WINDOW_FROM`], below
/// [`FEED_RETENTION_SWEPT_FLOOR`].
const FEED_RETENTION_ADMITTED_LOSS_WINDOW_END: time::OffsetDateTime =
    FEED_RETENTION_WINDOW_FROM.saturating_add(time::Duration::hours(1));

/// The end of the covered period of the entry the sweep removes and **this
/// read's scope never admitted**: two hours past
/// [`FEED_RETENTION_WINDOW_FROM`], also below [`FEED_RETENTION_SWEPT_FLOOR`].
const FEED_RETENTION_EXCLUDED_LOSS_WINDOW_END: time::OffsetDateTime =
    FEED_RETENTION_WINDOW_FROM.saturating_add(time::Duration::hours(2));

/// The end of the covered period of the entry the sweep **leaves behind
/// although a declared floor would permit removing it**: four hours past
/// [`FEED_RETENTION_WINDOW_FROM`], above [`FEED_RETENTION_SWEPT_FLOOR`] and
/// below [`FEED_RETENTION_DECLARED_FLOOR`].
const FEED_RETENTION_BELOW_THE_FLOOR_WINDOW_END: time::OffsetDateTime =
    FEED_RETENTION_WINDOW_FROM.saturating_add(time::Duration::hours(4));

/// The end of the covered period of the entry every cursor on the swept meter
/// is issued after: seven hours past [`FEED_RETENTION_WINDOW_FROM`], above
/// [`FEED_RETENTION_DECLARED_FLOOR`].
///
/// Above the declared floor as well as the swept one, so the swept meter holds
/// one retained entry on each side of that floor — which keeps the third
/// assertion about the entry *below* it rather than about retention in general.
const FEED_RETENTION_LANDMARK_WINDOW_END: time::OffsetDateTime =
    FEED_RETENTION_WINDOW_FROM.saturating_add(time::Duration::hours(7));

/// The end of the covered period of the first of the two entries on the meter
/// nothing is ever removed from: six hours past [`FEED_RETENTION_WINDOW_FROM`].
///
/// The two are on a meter of their own, so nothing about their covered periods
/// has to be separated from the swept meter's. They differ from each other
/// because a covered period is one of the six identity inputs and two entries
/// this check treats as two must derive two ids.
const FEED_RETENTION_INTACT_FIRST_WINDOW_END: time::OffsetDateTime =
    FEED_RETENTION_WINDOW_FROM.saturating_add(time::Duration::hours(6));

/// The end of the covered period of the second entry on that meter: seven
/// hours past [`FEED_RETENTION_WINDOW_FROM`].
const FEED_RETENTION_INTACT_SECOND_WINDOW_END: time::OffsetDateTime =
    FEED_RETENTION_WINDOW_FROM.saturating_add(time::Duration::hours(7));

/// The acceptance instant the two entries the sweep removes carry: an hour
/// **before** [`CONTRACT_ACCEPTED_AT`], which every entry the sweep leaves
/// behind carries.
///
/// **This is the one fixture choice in the module built for a defect, and the
/// defect is the one DESIGN names twice.** §3.1's `FeedPosition` row rules a
/// position's age out as an input — *"That age is a progress measure, not the
/// retention refusal's input (§3.2)"* — and §3.2 says the same from the
/// gateway's side, the plugin deciding the refusal *"from what it still holds
/// rather than from the cursor's age, which a sweep clamps to the retention
/// boundary"*.
///
/// The skew is what makes *"whatever that cursor's own age"* say something here.
/// Both refused cursors are issued immediately before an entry the sweep
/// removes, so the sweep **raises** both cursors' ages by an hour — exactly that
/// clamping — and a backend testing a cursor's age against its retention
/// boundary finds two young cursors and serves both a silently truncated range.
/// Without the skew the ages would not move and the clause would be satisfied by
/// accident. [`FeedRetentionFixtures::guards`] holds the skew.
const FEED_RETENTION_LOST_ACCEPTED_AT: time::OffsetDateTime =
    CONTRACT_ACCEPTED_AT.saturating_sub(time::Duration::hours(1));

/// The quantity every entry this check writes carries.
///
/// Its value is asserted nowhere. It is exactly representable in a binary float
/// all the same, which keeps `contract_mutants`'s `Defect::QuantityThroughFloat`
/// from reaching this check for a rule that belongs to `quantity-round-trip`.
const FEED_RETENTION_QUANTITY: &str = "1.5";

/// The page limit every read in this check dispatches: **one**.
///
/// The row is about whether a cursor is *refused*, never about how a page is
/// filled. One is chosen rather than tolerated: the walks that issue this
/// check's cursors have to reach the head over a ledger that withholds one entry
/// from the grant they run under, and a limit of one is the setting at which
/// every such walk really pages instead of answering in one read.
const FEED_RETENTION_PAGE_LIMIT: u64 = 1;

/// The six entries this check writes and the two meters it reads them over.
struct FeedRetentionFixtures {
    /// The meter the sweep is driven over, and the meter the three driven
    /// assertions read.
    swept: MeterRef,
    /// The meter **no drop of this check's is ever driven over**, and the meter
    /// the undriven assertion reads.
    ///
    /// A meter derived for this check's exclusive use is the only way to have
    /// one: every check runs against one backend, and a retention mark only ever
    /// rises, so a meter swept on an earlier run carries that mark forward. The
    /// undriven assertion is about a cursor over a ledger nothing has been
    /// removed from.
    intact: MeterRef,
    /// The oldest entry on the swept meter in the feed's order, and the entry
    /// the first cursor is issued after. Its covered period is above both
    /// floors, so no drop but the emptying one removes it.
    landmark: StoredUsageRecord,
    /// The entry the sweep removes that this read's scope **admits**. Second
    /// in the feed's order.
    admitted_loss: StoredUsageRecord,
    /// The entry the sweep removes that this read's scope **never admitted**.
    /// Third in the feed's order, and the entry the second assertion is about.
    excluded_loss: StoredUsageRecord,
    /// The entry the sweep leaves behind although
    /// [`FEED_RETENTION_DECLARED_FLOOR`] would permit removing it. Last in the
    /// feed's order, and the entry the third assertion requires a served
    /// continuation to carry.
    below_the_floor: StoredUsageRecord,
    /// The two entries on the meter nothing removes from, in the order they
    /// are submitted.
    intact_ledger: Vec<StoredUsageRecord>,
}

/// The three cursors the driven half issues, each named for the entry the swept
/// ledger ended at when it was taken.
///
/// Every one is the head of the swept meter at that moment, so what lies after
/// it is exactly what the staging submits next.
struct SweptCursors {
    /// Issued once the landmark was the whole ledger, so **both** entries the
    /// sweep removes are after it — and one of them is under the tenant this
    /// read's scope admits.
    landmark: FeedPosition,
    /// Issued once the loss this read's scope admits had been submitted, so
    /// the **only** entry the sweep removes after it is one that scope has
    /// never admitted.
    admitted_loss: FeedPosition,
    /// Issued once both losses had been submitted, so **nothing** the sweep
    /// removes is after it and its continuation is intact.
    excluded_loss: FeedPosition,
}

/// `feed-retention-refusal` — *"A cursor after which retention has removed an
/// entry of a subscribed GTS type is refused rather than served as a short page,
/// whatever that cursor's own age and whether or not the caller's scope admitted
/// that entry. A cursor whose continuation is intact is served, including one
/// older than the floor where the plugin retains longer than it."* (DESIGN §3.3,
/// "Plugin contract tests".)
///
/// **The row is one rule stated from both sides**: the refusal reads what a
/// sweep *removed*, and nothing else. Three things are named as not being inputs
/// to it — the cursor's age, the caller's scope, and (in the second sentence)
/// the floor itself — and each is a backend somebody would write.
///
/// # What is asserted
///
/// 1. **A cursor whose continuation a sweep truncated is refused**, with
///    [`UsageCollectorPluginError::CursorBeyondRetention`] and not with a short
///    page. Two entries are submitted after this cursor was issued and both are
///    then swept away; one is under the tenant this read's scope admits.
/// 2. **It is refused whether or not the caller's scope admitted the entry that
///    was removed.** The second cursor is issued between the two losses, so the
///    only entry the sweep removes after it belongs to a tenant the grant never
///    admitted. It must still be refused.
/// 3. **A cursor whose continuation is intact is served**, and the continuation
///    is delivered. The third cursor is issued after both losses and the entry
///    after it is one [`FEED_RETENTION_DECLARED_FLOOR`] would permit removing and
///    the sweep did not.
/// 4. **A cursor over a ledger nothing has been removed from is served.** The
///    meter this one is issued over is the one no drop of this check's is driven
///    against.
///
/// **Assertion 4 is the only one that needs no drive**, and it is what this check
/// contributes to a [`run_all`](super::super::run_all) run.
///
/// *"Whatever that cursor's own age"* is not an assertion of its own and could
/// not be — age is not an output of any SPI call. It is a property of the
/// fixtures instead; [`FEED_RETENTION_LOST_ACCEPTED_AT`] is where it lives.
///
/// # Why the driven half builds its ledger in front of its cursors
///
/// A cursor cannot be asked for; it is whatever a read hands back, and the only
/// way to know what lies after one is to have written nothing there yet. So the
/// driven half **submits, reads to the head, submits, reads to the head**: the
/// walk's exit condition — the cursor standing still — fixes each position as one
/// the whole ledger so far is behind (DESIGN §3.1's Feed order invariant).
/// Naming the position a page at a chosen limit stopped at would instead read the
/// backend's page arithmetic, and **a short page is conforming**. The staging
/// therefore needs an empty meter to build into, which is what
/// [`empty_the_swept_meter`] leaves behind.
///
/// # Which assertions a subject reaches, measured
///
/// Every assertion below was inverted and confirmed to fire against the
/// reference backend, so none is dead: assertions 1, 2 and 3 fire only under a
/// drive, and assertion 4 under both entry points. The subjects that reach this
/// check are all in `contract_mutants` and all built on `MutantLedger` — the
/// refusal is a branch of `read_feed_page` itself, and a wrapper delegating to a
/// conforming backend cannot make that backend fail to refuse:
///
/// * **`Defect::ServesAShortPageWhereRetentionTruncatedACursor`** — the subject
///   the row names outright — is reported by assertions 1 and 2, and by
///   **neither alone**. What *is* individually load-bearing is the branch inside
///   the shared probe that reports a **served** page.
/// * **`Defect::TheRetentionRefusalReadsTheCallersGrant`** is reported by
///   assertion 2 **alone**, which is what makes *"whether or not the caller's
///   scope admitted that entry"* a clause this check establishes rather than
///   restates. The subject refuses assertion 1's cursor correctly.
/// * **`Defect::RefusesEveryCursorOnceASweepHasRun`** is reported by assertion 3
///   **alone**, and by its **refusal** branch alone. It is the subject for the
///   row's second sentence.
/// * **`Defect::TheRetentionRefusalIgnoresTheSubscription`** is reported by
///   assertion 4 **alone**, and by its refusal branch alone. It is only reachable
///   because assertion 4 is taken **last**: under a drive it then runs while this
///   check's other meter carries a mark, and a backend reading its marks across
///   every type at once refuses a cursor over a ledger nothing was removed from.
///
/// **Assertion 1 is therefore not individually load-bearing, and it stays**:
/// everything it catches, assertion 2 catches too, but assertion 2's report names
/// an entry the caller's grant never admitted, and a backend with no marks table
/// at all should be told about the ordinary half first.
///
/// **None of the four subjects is in the discrimination matrix**, and none could
/// be: `run_all` drives nothing, so under that dispatch all four are
/// behaviourally the reference backend. They are rows of `contract_tests`'
/// `RETENTION_DRIVEN_MATRIX`.
///
/// # What this check pins that no undriven check can
///
/// `contract_mutants`' `MutantLedger` mirrors the reference backend by hand. Its
/// retention sweep and cursor refusal exist only because this check needed them,
/// and **its feed seek** is kept honest only here: over a dense, append-ordered
/// ledger `sequence > from` and a skip of that many rows name the same entry, so
/// the undriven matrix stays green under either — measured. Only a gap separates
/// them, a retention drop makes one, and assertion 3's refusal branch reports the
/// skip. `contract_tests`'
/// `a_subject_carrying_its_own_ledger_is_still_itself_under_a_drive` asserts it.
///
/// # Assertions no subject reaches, and why each stays
///
/// Neutering any of these changed no row of either matrix.
///
/// * **The refusal probe's second branch**, which fires when a read is refused
///   with some error other than
///   [`UsageCollectorPluginError::CursorBeyondRetention`]. A subject answering the
///   wrong variant would be modelling a mis-typed error rather than a mis-decided
///   refusal. **A recorded gap**, kept because a consumer that bootstraps again
///   and one that retries the same cursor forever are different failures.
/// * **Assertion 3's delivery branch**, which fires when an intact continuation
///   is answered without the entry in front of the cursor. The skip above was the
///   nearest thing to a subject and it reached the refusal branch instead. **A
///   recorded gap**: a probe that stopped at the answer would report a lost
///   continuation as a served one.
/// * **Assertion 4's walk-failure branch**, and **the walk-failure report in
///   [`stage`]**. The report comes from [`super::super::feed_walk`], so **a
///   recorded gap shared with every caller of that walk.**
/// * **The two drive-failure reports**, in [`empty_the_swept_meter`] and
///   [`sweep_and_read_each_cursor`]. They fire when
///   [`ContractRetention::drop_before`] answers `Err`, which is the suite failing
///   to set a scenario up rather than a backend answering wrongly — hence
///   [`HARNESS_FAULT`]. **Unreachable by construction, recorded rather than
///   removed**, because a drive failing silently would leave assertions 1 and 2
///   asserting a refusal over an intact ledger.
/// * **The submission reports** in [`submit`]. No subject refuses a well-formed
///   record carrying a fresh idempotency key. **A recorded gap shared with every
///   other check's submission guard.**
/// * **The seven fixture guards** in [`FeedRetentionFixtures::guards`]. Each was
///   inverted and each fires; neutering all seven together changes no row. They
///   are the suite's own facts, so no backend outcome can make one fire.
///   **Unreachable by any plugin, deliberately.**
///
/// # What this check does not reach
///
/// It asserts nothing about **which** entries a served page carries beyond the
/// one entry assertion 3 names, and nothing about their order — those are
/// `feed-snapshot-and-replay`'s and `feed-completeness`'s, and a second check
/// asserting them would report one mistake under two names. It dispatches no
/// bounded replay and compares no two positions (`feed-position-bounded`'s row),
/// writes no invalidation and drives no concurrency.
///
/// It also asserts nothing about `FeedStart::Oldest`: that start mode is exempt
/// from the refusal, and the exemption is
/// [`feed_bootstrap_position`](super::feed_bootstrap_position())'s row.
pub async fn feed_retention_refusal(
    plugin: &dyn UsageCollectorPluginV1,
    retention: Option<&dyn ContractRetention>,
) -> Vec<ContractViolation> {
    let fixtures = match feed_retention_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own `{FEED_RETENTION_REFUSAL}` \
                     fixtures, so nothing was submitted. This is a fault in the suite, not in \
                     the plugin under test: {detail}"
                ),
            )];
        }
    };

    // A refusal stops the check: every assertion below is a statement about a
    // cursor over a ledger that holds entries, and over an empty one they hold
    // vacuously.
    let mut violations = submit(
        plugin,
        &fixtures.intact,
        &fixtures.intact_ledger,
        "the pair its unswept cursor is issued over",
    )
    .await;
    if !violations.is_empty() {
        return violations;
    }

    if let Some(retention) = retention {
        violations.extend(the_refusal_reads_what_was_removed(plugin, retention, &fixtures).await);
    }
    violations.extend(a_cursor_over_an_unswept_ledger_is_served(plugin, &fixtures).await);
    violations
}

/// Assertions one, two and three, and the emptying that always follows them.
///
/// Three steps: build the ledger in front of three cursors
/// ([`stage_the_swept_ledger`]), sweep it and read each cursor back
/// ([`sweep_and_read_each_cursor`]), and leave the meter empty
/// ([`empty_the_swept_meter`]). The third runs whatever the first two did, which
/// is why the `match` below is not written as an early return.
async fn the_refusal_reads_what_was_removed(
    plugin: &dyn UsageCollectorPluginV1,
    retention: &dyn ContractRetention,
    fixtures: &FeedRetentionFixtures,
) -> Vec<ContractViolation> {
    let mut violations = match stage_the_swept_ledger(plugin, fixtures).await {
        Ok(cursors) => sweep_and_read_each_cursor(plugin, retention, fixtures, &cursors).await,
        Err(violations) => violations,
    };
    // Unconditional: the emptying is the only thing that puts this meter back,
    // so a run that gave up on its own assertions must not leave the next run a
    // ledger its cursors would be issued behind rather than in front of.
    violations.extend(empty_the_swept_meter(retention, fixtures).await);
    violations
}

/// Drives the sweep the driven assertions are about and reads each cursor back.
///
/// The drop is the only thing that happens between the staging and the reads, so
/// a difference in how the cursors are answered can be nothing but the sweep.
///
/// A failure to drive is reported as a [`HARNESS_FAULT`] rather than against the
/// plugin: a backend that could not be driven has not answered an SPI call
/// wrongly. See [`ContractRetention::drop_before`]'s error type.
async fn sweep_and_read_each_cursor(
    plugin: &dyn UsageCollectorPluginV1,
    retention: &dyn ContractRetention,
    fixtures: &FeedRetentionFixtures,
    cursors: &SweptCursors,
) -> Vec<ContractViolation> {
    if let Err(detail) = retention
        .drop_before(&fixtures.swept, FEED_RETENTION_SWEPT_FLOOR)
        .await
    {
        return vec![violation(
            HARNESS_FAULT,
            format!(
                "the contract suite could not drive this backend's retention over its own \
                 `{FEED_RETENTION_REFUSAL}` meter, so the entries the next two reads are \
                 asserted to be refused over were never removed and both assertions would have \
                 failed for the wrong reason. This is a fault in the drive, not an answer the \
                 plugin gave to an SPI call: {detail}"
            ),
        )];
    }

    let mut violations = a_truncated_cursor_is_refused(
        plugin,
        fixtures,
        &cursors.landmark,
        "two entries this check submitted after it, one of them under the tenant this read's \
         scope admits",
    )
    .await;
    violations.extend(
        a_truncated_cursor_is_refused(
            plugin,
            fixtures,
            &cursors.admitted_loss,
            "one entry this check submitted after it, under a tenant this read's scope has \
             never admitted",
        )
        .await,
    );
    violations
        .extend(an_intact_continuation_is_served(plugin, fixtures, &cursors.excluded_loss).await);
    violations
}

/// Builds the swept ledger one step at a time, taking a cursor between the
/// steps.
///
/// Four entries and three cursors, in this order: the landmark, a cursor, the
/// loss the scope admits, a cursor, the loss the scope withholds, a cursor, and
/// the entry the sweep leaves behind. Each cursor is the head of the ledger when
/// it is taken, so exactly the entries submitted after it are in front of it.
///
/// `Err` carries the violations that stopped the staging — a refused submission
/// or a walk that could not reach the head — which leave the ledger in a shape
/// the driven assertions are not about.
async fn stage_the_swept_ledger(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FeedRetentionFixtures,
) -> Result<SweptCursors, Vec<ContractViolation>> {
    let landmark = stage(
        plugin,
        fixtures,
        &fixtures.landmark,
        "the entry every cursor on this meter is issued after",
    )
    .await?;
    let admitted_loss = stage(
        plugin,
        fixtures,
        &fixtures.admitted_loss,
        "the entry the sweep removes that this read's scope admits",
    )
    .await?;
    let excluded_loss = stage(
        plugin,
        fixtures,
        &fixtures.excluded_loss,
        "the entry the sweep removes that this read's scope withholds",
    )
    .await?;

    let refused = submit(
        plugin,
        &fixtures.swept,
        std::slice::from_ref(&fixtures.below_the_floor),
        "the entry the sweep leaves behind below the declared floor",
    )
    .await;
    if !refused.is_empty() {
        return Err(refused);
    }

    Ok(SweptCursors {
        landmark,
        admitted_loss,
        excluded_loss,
    })
}

/// Submits one entry and answers with the position the feed has reached over the
/// swept meter.
///
/// The walk's exit is the cursor standing still, which is what makes the position
/// the head rather than wherever a page happened to stop (DESIGN §3.1's Feed
/// order invariant: *"A page reaching the settled head returns its cursor at the
/// head"*). Everything submitted after this call is therefore in front of it.
///
/// The walk runs under [`contract_scope`], the same grant the assertions read
/// under, over a ledger one of whose entries that grant withholds. That is
/// deliberate: a position denotes a prefix of the ledger rather than of what a
/// grant admits, and a walk that could not get past an entry it may not carry is
/// `feed-snapshot-and-replay`'s to report.
async fn stage(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FeedRetentionFixtures,
    entry: &StoredUsageRecord,
    role: &str,
) -> Result<FeedPosition, Vec<ContractViolation>> {
    let refused = submit(plugin, &fixtures.swept, std::slice::from_ref(entry), role).await;
    if !refused.is_empty() {
        return Err(refused);
    }

    let subscription = [fixtures.swept.clone()];
    feed_walk(
        plugin,
        &subscription,
        &contract_scope(),
        FeedStart::Oldest,
        FEED_RETENTION_PAGE_LIMIT,
        WalkStop::AtTheHead,
    )
    .await
    .map(|walk| walk.stopped_at)
    .map_err(|detail| {
        vec![violation(
            FEED_RETENTION_REFUSAL,
            format!(
                "{detail} The walk was taken over this check's own swept meter to obtain the \
                 position it issues its cursors from, immediately after submitting {role}, and \
                 nothing had yet been removed from that meter on this run. A cursor this check \
                 cannot obtain is a cursor it cannot then have refused or served, so none of \
                 the three assertions about one ran."
            ),
        )]
    })
}

/// Assertions one and two: a cursor whose continuation a sweep truncated is
/// refused.
///
/// One probe over two cursors rather than two written out, because the
/// obligation is the same one both times. `what_was_removed` is what tells them
/// apart in a report, and it is the whole of the difference the row draws: the
/// first cursor has an entry the grant admits removed after it, the second has
/// only an entry the grant never admitted.
///
/// **A short page is the failure the row names**, and it is the dangerous one: a
/// refusal is an error a consumer can act on, a short page is a gap it cannot
/// detect. A refusal carrying some other variant is reported separately, because
/// a mis-typed error and a mis-decided refusal are different work.
///
/// The read is a single page rather than a walk, which would fold the refusal
/// into its own exit condition and leave nothing to say about which of the two
/// obligations a backend broke.
async fn a_truncated_cursor_is_refused(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FeedRetentionFixtures,
    cursor: &FeedPosition,
    what_was_removed: &str,
) -> Vec<ContractViolation> {
    let subscription = [fixtures.swept.clone()];
    match plugin
        .read_feed_page(
            &subscription,
            &contract_scope(),
            FeedStart::After(cursor.clone()),
            None,
            FEED_RETENTION_PAGE_LIMIT,
        )
        .await
    {
        Err(UsageCollectorPluginError::CursorBeyondRetention) => Vec::new(),
        Err(err) => vec![violation(
            FEED_RETENTION_REFUSAL,
            format!(
                "a feed read resumed from a cursor this backend issued over this check's own \
                 meter was refused with `{err}`. Retention had removed {what_was_removed}, so \
                 the refusal is right and the variant is not: a cursor after which retention \
                 has removed an entry of a subscribed GTS type is refused with \
                 `CursorBeyondRetention`, which DESIGN section 3.3 lifts to an \
                 `InvalidArgument` naming the `cursor` field. A consumer told its cursor is \
                 beyond retention bootstraps again and re-rates what it has to; one told the \
                 backend broke retries the same cursor forever."
            ),
        )],
        Ok(page) => vec![violation(
            FEED_RETENTION_REFUSAL,
            format!(
                "a feed read resumed from a cursor this backend issued over this check's own \
                 meter was served, carrying {delivered:?}. Retention had removed \
                 {what_was_removed}: a cursor after which retention has removed an entry of a \
                 subscribed GTS type is refused rather than served as a short page. This is the \
                 failure the row exists to forbid and the worst one a feed has, because it is \
                 the only one a consumer cannot see. The page that arrives is well formed, its \
                 cursor advances, and the entries that were swept away between this consumer's \
                 position and the ones it is handed are simply never delivered - so the usage \
                 they record is charged to nobody and nothing says so. The refusal is decided \
                 from what this backend still holds, not from how old the cursor is and not \
                 from what this caller's scope would have admitted.",
                delivered = ids(&page.entries),
            ),
        )],
    }
}

/// Assertion three: a cursor whose continuation is intact is served, and the
/// continuation arrives.
///
/// The cursor is issued after both entries the sweep removes, so nothing it
/// asked for is missing; the entry in front of it is
/// [`FeedRetentionFixtures::below_the_floor`], whose covered period
/// [`FEED_RETENTION_DECLARED_FLOOR`] would permit removing and the sweep did not.
/// That is DESIGN's *"including one older than the floor where the plugin retains
/// longer than it"*, and it follows from the mark recording what was **actually
/// removed**.
///
/// **The delivery is asserted as well as the answer**, and the two are one
/// obligation: a page that answers `Ok` and hands back a cursor at the head
/// without the entry in front of it has lost that continuation rather than served
/// it. The read is a walk because a short page is conforming, so a single page
/// proves nothing about what a continuation carries.
async fn an_intact_continuation_is_served(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FeedRetentionFixtures,
    cursor: &FeedPosition,
) -> Vec<ContractViolation> {
    let subscription = [fixtures.swept.clone()];
    let walk = match feed_walk(
        plugin,
        &subscription,
        &contract_scope(),
        FeedStart::After(cursor.clone()),
        FEED_RETENTION_PAGE_LIMIT,
        WalkStop::AtTheHead,
    )
    .await
    {
        Ok(walk) => walk,
        Err(detail) => {
            return vec![violation(
                FEED_RETENTION_REFUSAL,
                format!(
                    "{detail} The cursor was issued over this check's own meter after every \
                     entry retention then removed, so nothing it asked for is missing and its \
                     continuation is intact. A cursor whose continuation is intact is served, \
                     including one older than the floor where the plugin retains longer than \
                     it. A backend that refuses here has decided the refusal from a floor \
                     rather than from what a sweep removed, and once it has swept anything it \
                     refuses every consumer that was following it - which no consumer can \
                     retry its way out of, because a retention mark only ever rises."
                ),
            )];
        }
    };

    if ids(&walk.delivered).contains(&fixtures.below_the_floor.id) {
        return Vec::new();
    }
    vec![violation(
        FEED_RETENTION_REFUSAL,
        format!(
            "a feed read resumed from a cursor this backend issued over this check's own meter \
             was followed to the head and delivered {delivered:?}; the entry {expected} was \
             submitted after that cursor was issued, was not removed by the sweep this check \
             drove, and is admitted by the scope this read ran under. A cursor whose \
             continuation is intact is served, and a continuation that is answered without the \
             entries in it has not been served but lost. The covered period of that entry ends \
             below the floor a declared retention would permit this backend to sweep to and \
             above the floor the sweep was actually driven to, which is the case DESIGN names: \
             a plugin may retain longer than its own floor, and what decides the refusal is \
             what a sweep removed rather than what a floor would permit removing.",
            delivered = ids(&walk.delivered),
            expected = fixtures.below_the_floor.id,
        ),
    )]
}

/// Assertion four: a cursor over a ledger nothing has been removed from is
/// served.
///
/// **The only assertion here that needs no drive**, and the whole of what this
/// check contributes to a [`run_all`](super::super::run_all) run. It is the floor
/// under the other three: a backend that refused a resumed read whatever its
/// retention had done would satisfy assertions 1 and 2 and break every consumer
/// there is.
///
/// The meter is the one no drop of this check's is driven over, because a
/// retention mark only ever rises. **It is taken last**, after the driven half:
/// this check's other meter then carries a mark, so a backend whose mark is keyed
/// on nothing but the sweep is refused here. `usage-collector-v1.yaml` states the
/// obligation that would break: removal *"is read per subscribed GTS type, so a
/// cursor can be refused for an entry the caller's own scope excluded"*.
///
/// Nothing is asserted about what the page carries — that is
/// `feed-snapshot-and-replay`'s and `feed-completeness`'s. One consequence is
/// worth naming: a backend whose bootstrap read begins at the head hands this
/// probe a cursor at the head, which is then served and reported by nothing here.
/// That defect is `feed-bootstrap-position`'s.
async fn a_cursor_over_an_unswept_ledger_is_served(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FeedRetentionFixtures,
) -> Vec<ContractViolation> {
    let subscription = [fixtures.intact.clone()];
    let walk = match feed_walk(
        plugin,
        &subscription,
        &contract_scope(),
        FeedStart::Oldest,
        FEED_RETENTION_PAGE_LIMIT,
        WalkStop::FirstDelivery,
    )
    .await
    {
        Ok(walk) => walk,
        Err(detail) => {
            return vec![violation(
                FEED_RETENTION_REFUSAL,
                format!(
                    "{detail} The walk was taken over the one meter this check never drives \
                     retention against, to obtain a cursor whose continuation nothing has \
                     removed anything from. A cursor this check cannot obtain is a cursor it \
                     cannot then have served."
                ),
            )];
        }
    };

    match plugin
        .read_feed_page(
            &subscription,
            &contract_scope(),
            FeedStart::After(walk.stopped_at),
            None,
            FEED_RETENTION_PAGE_LIMIT,
        )
        .await
    {
        Ok(_) => Vec::new(),
        Err(err) => vec![violation(
            FEED_RETENTION_REFUSAL,
            format!(
                "a feed read resumed from a cursor this backend had just issued over a meter \
                 nothing has ever been removed from was refused: {err}. A cursor whose \
                 continuation is intact is served. Retention removes what it removes per \
                 subscribed GTS type, so a sweep over some other meter is not a reason to \
                 refuse a consumer of this one, and a backend that refuses here refuses every \
                 consumer that has a position at all - which is every consumer past its first \
                 page."
            ),
        )],
    }
}

/// Puts this check's swept meter back the way a later run needs to find it:
/// **empty**, so that the ledger a later run builds is built in front of the
/// cursors it issues.
///
/// **This is the price of being a check that purges.** A purged entry is not
/// absorbed on re-delivery: it is gone, so re-delivering it is an insert, and an
/// insert joins the feed at the head rather than back where it was.
/// [`feed_bootstrap_position`](super::feed_bootstrap_position()) answers by
/// re-delivering its two entries in its own order, because what it asserts is
/// which of them a read begins at. This one asserts what lies *after* cursors it
/// issues, and there is only one ledger from which that is the same on every run:
/// the one its own staging builds, from nothing. So the drop is driven to
/// [`FEED_RETENTION_RESTORING_FLOOR`], above every covered period this check
/// writes to this meter, and nothing is re-delivered.
///
/// **It is the only drop driven here that is not the assertions' own, and it is
/// made after them rather than also before them.** A second, defensive drop at
/// the top would look free and is not: every cursor this check issues comes from
/// a `FeedStart::Oldest` walk, that start mode is the one DESIGN §3.3 exempts
/// from the retention refusal, and a walk over a meter this check had *already*
/// swept would exercise the exemption `feed-bootstrap-position` owns — so one
/// mistake would be reported under two names.
async fn empty_the_swept_meter(
    retention: &dyn ContractRetention,
    fixtures: &FeedRetentionFixtures,
) -> Vec<ContractViolation> {
    let Err(detail) = retention
        .drop_before(&fixtures.swept, FEED_RETENTION_RESTORING_FLOOR)
        .await
    else {
        return Vec::new();
    };
    vec![violation(
        HARNESS_FAULT,
        format!(
            "the contract suite could not drive this backend's retention over its own \
             `{FEED_RETENTION_REFUSAL}` meter a second time, so that meter is left holding \
             entries a later run would find ahead of the cursors it issues rather than behind \
             them, and every assertion that run makes about what a sweep removed after a cursor \
             would be about a sweep that removed something before it. This is a fault in the \
             drive, not an answer the plugin gave to an SPI call: {detail}"
        ),
    )]
}

/// Submits `entries` in order, reporting any refusal.
///
/// Every entry is submitted whatever the ones before it did: the assertions read
/// ledgers every entry belongs to, and stopping at the first refusal would leave
/// a shorter ledger than the report describes. `role` names what the submission
/// was for, because a refusal at each of this check's five submission points
/// leaves a different ledger.
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
                FEED_RETENTION_REFUSAL,
                format!(
                    "`create_usage_records` refused {role} (entry {id}, tenant {tenant}, covered \
                     period ending {window_end}), so this check's meters do not hold the \
                     ledgers its assertions are about: {err}",
                    id = entry.id,
                    tenant = entry.tenant_id,
                    window_end = entry.window_end,
                ),
            ));
        }
    }
    violations
}

/// Builds the six entries and the two meters.
///
/// **The covered periods are deliberately not in admission order**, and that is
/// the one thing about these fixtures worth reading twice. The feed's order is
/// the order entries were admitted in; a retention floor is a bound on the
/// covered period. The two are independent — DESIGN says so by having backfill
/// exist at all — and this check needs an entry **after** a cursor in the feed's
/// order and **below** a floor in covered period. So the two entries the sweep
/// removes are submitted third and fifth while carrying the two earliest covered
/// periods of the six.
///
/// Everything else is held fixed: one quantity, one resource, one meter per role.
/// The covered period decides what the sweep removes, and the tenant decides what
/// the grant admits. Seven guards, all the suite's own facts rather than the
/// plugin's and so all [`HARNESS_FAULT`], keep the check from passing by
/// construction: see [`FeedRetentionFixtures::guards`].
fn feed_retention_fixtures() -> Result<FeedRetentionFixtures, String> {
    let swept = check_meter(FEED_RETENTION_REFUSAL, "swept")?;
    let intact = check_meter(FEED_RETENTION_REFUSAL, "intact")?;
    let quantity = UsageQuantity::parse(FEED_RETENTION_QUANTITY).map_err(|err| {
        format!("the check's own quantity `{FEED_RETENTION_QUANTITY}` does not parse: {err}")
    })?;

    let on_the_swept_meter = |role: &str,
                              tenant: Uuid,
                              accepted_at: time::OffsetDateTime,
                              window_end: time::OffsetDateTime|
     -> Result<StoredUsageRecord, String> {
        fixture_record_on(
            &swept,
            tenant,
            &feed_retention_key(role)?,
            quantity,
            accepted_at,
            FEED_RETENTION_WINDOW_FROM,
            window_end,
        )
    };

    let landmark = on_the_swept_meter(
        "landmark",
        CONTRACT_TENANT_ID,
        CONTRACT_ACCEPTED_AT,
        FEED_RETENTION_LANDMARK_WINDOW_END,
    )?;
    let admitted_loss = on_the_swept_meter(
        "admitted-loss",
        CONTRACT_TENANT_ID,
        FEED_RETENTION_LOST_ACCEPTED_AT,
        FEED_RETENTION_ADMITTED_LOSS_WINDOW_END,
    )?;
    let excluded_loss = on_the_swept_meter(
        "excluded-loss",
        SCOPE_EXCLUDED_TENANT_ID,
        FEED_RETENTION_LOST_ACCEPTED_AT,
        FEED_RETENTION_EXCLUDED_LOSS_WINDOW_END,
    )?;
    let below_the_floor = on_the_swept_meter(
        "below-the-floor",
        CONTRACT_TENANT_ID,
        CONTRACT_ACCEPTED_AT,
        FEED_RETENTION_BELOW_THE_FLOOR_WINDOW_END,
    )?;

    let mut intact_ledger = Vec::with_capacity(2);
    for (role, window_end) in [
        ("intact-first", FEED_RETENTION_INTACT_FIRST_WINDOW_END),
        ("intact-second", FEED_RETENTION_INTACT_SECOND_WINDOW_END),
    ] {
        intact_ledger.push(fixture_record_on(
            &intact,
            CONTRACT_TENANT_ID,
            &feed_retention_key(role)?,
            quantity,
            CONTRACT_ACCEPTED_AT,
            FEED_RETENTION_WINDOW_FROM,
            window_end,
        )?);
    }

    let fixtures = FeedRetentionFixtures {
        swept,
        intact,
        landmark,
        admitted_loss,
        excluded_loss,
        below_the_floor,
        intact_ledger,
    };
    fixtures.guards()?;
    Ok(fixtures)
}

impl FeedRetentionFixtures {
    /// Every entry this check writes to its swept meter, in the order it
    /// submits them.
    fn swept_ledger(&self) -> [&StoredUsageRecord; 4] {
        [
            &self.landmark,
            &self.admitted_loss,
            &self.excluded_loss,
            &self.below_the_floor,
        ]
    }

    /// The seven facts this check's assertions read, established rather than
    /// assumed.
    ///
    /// * **The two meters are two.** A single meter would put the undriven
    ///   assertion's cursor over a ledger this check sweeps, and that
    ///   assertion is about one nothing removes from.
    /// * **The six entries derive six ids.** They do — the idempotency key is
    ///   one of the six identity inputs and every role here names its own —
    ///   but if two ever collided, the second submission would be absorbed as
    ///   a retry of the first and a ledger this check believes holds four
    ///   entries would hold three.
    /// * **The sweep floor separates the two losses from the two entries that
    ///   stay.** Both losses' covered periods end strictly below
    ///   [`FEED_RETENTION_SWEPT_FLOOR`] and both survivors' end at or above
    ///   it, which is what `drop_before`'s exclusive bound turns into "two go,
    ///   two stay". A floor that took nothing would leave assertions 1 and 2
    ///   asserting a refusal over an intact continuation; one that took
    ///   everything would leave assertion 3 with no entry to name.
    /// * **The entry the sweep leaves behind really is below a floor that
    ///   would permit removing it**, and the landmark really is above that
    ///   floor. Without the first, assertion 3 asserts nothing about *"a
    ///   cursor older than the floor"*; without the second, the swept meter
    ///   holds nothing above the declared floor and the contrast the assertion
    ///   draws is between an entry and an empty set.
    /// * **The emptying floor is above every covered period on the swept
    ///   meter.** A drop that left one entry behind would leave it ahead of
    ///   the next run's first cursor.
    /// * **The two losses fall on opposite sides of the grant.** One is under
    ///   the tenant [`contract_scope`] pins and the other is not, which is the
    ///   whole of the difference between assertions 1 and 2. Two losses under
    ///   one tenant would make the second assertion a second reading of the
    ///   first.
    /// * **Every entry the sweep removes carries an acceptance instant
    ///   strictly earlier than every entry it leaves.** This is the age clamp
    ///   DESIGN §3.2 names, and without it the sweep leaves both refused
    ///   cursors' ages where they were — so a backend deciding the refusal
    ///   from a cursor's age would be satisfying assertions 1 and 2 by
    ///   accident rather than being caught by them.
    ///   [`FEED_RETENTION_LOST_ACCEPTED_AT`] says the rest.
    fn guards(&self) -> Result<(), String> {
        if self.swept == self.intact {
            return Err(format!(
                "this check's swept and intact meters are one value ({swept}), so the meter its \
                 undriven assertion reads an unswept ledger over is the meter it drives three \
                 drops against",
                swept = self.swept.id.as_str(),
            ));
        }

        let entries: Vec<&StoredUsageRecord> = self
            .swept_ledger()
            .into_iter()
            .chain(self.intact_ledger.iter())
            .collect();
        for (index, entry) in entries.iter().enumerate() {
            if entries[..index].iter().any(|other| other.id == entry.id) {
                return Err(format!(
                    "two of this check's six entries derive one id ({id}); the second \
                     submission would be absorbed as a retry of the first, and a ledger the \
                     assertions treat as holding four entries would hold three",
                    id = entry.id,
                ));
            }
        }

        if !(self.admitted_loss.window_end < FEED_RETENTION_SWEPT_FLOOR
            && self.excluded_loss.window_end < FEED_RETENTION_SWEPT_FLOOR
            && self.below_the_floor.window_end >= FEED_RETENTION_SWEPT_FLOOR
            && self.landmark.window_end >= FEED_RETENTION_SWEPT_FLOOR)
        {
            return Err(format!(
                "this check's sweep floor does not separate the two entries it must remove from \
                 the two it must leave. The floor is `{floor}`; the entries it must remove \
                 cover periods ending `{admitted}` and `{excluded}`, and the entries it must \
                 leave cover periods ending `{below}` and `{landmark}`. The drive removes an \
                 entry whose period ends strictly before the floor",
                floor = FEED_RETENTION_SWEPT_FLOOR,
                admitted = self.admitted_loss.window_end,
                excluded = self.excluded_loss.window_end,
                below = self.below_the_floor.window_end,
                landmark = self.landmark.window_end,
            ));
        }

        if !(self.below_the_floor.window_end < FEED_RETENTION_DECLARED_FLOOR
            && self.landmark.window_end >= FEED_RETENTION_DECLARED_FLOOR)
        {
            return Err(format!(
                "this check's declared floor `{floor}` does not fall between the covered period \
                 of the entry the sweep leaves behind, ending `{below}`, and that of the \
                 landmark, ending `{landmark}`. The third assertion is about a cursor over an \
                 entry a declared retention would permit removing and a sweep did not, so that \
                 entry has to end below the declared floor while some other retained entry ends \
                 above it",
                floor = FEED_RETENTION_DECLARED_FLOOR,
                below = self.below_the_floor.window_end,
                landmark = self.landmark.window_end,
            ));
        }

        for entry in self.swept_ledger() {
            if entry.window_end >= FEED_RETENTION_RESTORING_FLOOR {
                return Err(format!(
                    "this check's emptying floor `{floor}` does not reach the entry {id}, whose \
                     covered period ends `{window_end}`, so a drop driven to it would leave \
                     that entry on the swept meter and a later run would find it ahead of the \
                     first cursor it issues rather than behind it",
                    floor = FEED_RETENTION_RESTORING_FLOOR,
                    id = entry.id,
                    window_end = entry.window_end,
                ));
            }
        }

        if self.admitted_loss.tenant_id != CONTRACT_TENANT_ID
            || self.excluded_loss.tenant_id == CONTRACT_TENANT_ID
        {
            return Err(format!(
                "this check's two losses do not fall on opposite sides of the grant its reads \
                 run under. The grant admits `{admitted_tenant}`; the loss it must admit is \
                 attributed to `{admitted}` and the loss it must withhold to `{excluded}`. \
                 Without the split the second assertion is a second reading of the first, and \
                 the clause about a scope that never admitted the removed entry is asserted \
                 nowhere",
                admitted_tenant = CONTRACT_TENANT_ID,
                admitted = self.admitted_loss.tenant_id,
                excluded = self.excluded_loss.tenant_id,
            ));
        }

        for loss in [&self.admitted_loss, &self.excluded_loss] {
            for survivor in [&self.landmark, &self.below_the_floor] {
                if loss.accepted_at >= survivor.accepted_at {
                    return Err(format!(
                        "the entry {loss} that this check's sweep removes carries the \
                         acceptance instant `{lost_at}` and the entry {survivor} it leaves \
                         carries `{kept_at}`, so the sweep does not raise the age of the \
                         cursors issued in front of the losses. DESIGN section 3.2 has a sweep \
                         clamp a position's age to the retention boundary, and that clamp is \
                         what makes the clause about a cursor's own age say something here: \
                         without it a backend deciding the refusal from an age satisfies both \
                         refusal assertions by accident",
                        loss = loss.id,
                        lost_at = loss.accepted_at,
                        survivor = survivor.id,
                        kept_at = survivor.accepted_at,
                    ));
                }
            }
        }

        Ok(())
    }
}

/// The idempotency key one of this check's entries submits under.
///
/// Keyed on the role name for the reason
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture) spells out:
/// an edited fixture must take a fresh identity rather than inherit an accepted
/// entry's. **This check does have a delete path**, and the reasoning survives it
/// unchanged: the delete is driven over a covered-period floor rather than over
/// an identity, so an edited fixture is not the thing it removes.
fn feed_retention_key(role: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{FEED_RETENTION_REFUSAL}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}
