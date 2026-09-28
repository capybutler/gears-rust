//! The DESIGN §3.3 `feed-retention-refusal` check.
//!
//! See [`feed_retention_refusal`] for what it asserts. The module holds the
//! six entries it writes, the two meters it names, and the two drops it
//! drives. The reads it makes are [`super::super::feed_walk`]'s, shared with
//! [`feed_snapshot_and_replay`](super::feed_snapshot_and_replay()),
//! [`feed_completeness`](super::feed_completeness()) and
//! [`feed_bootstrap_position`](super::feed_bootstrap_position()).
//!
//! **It is the check [`ContractRetention`] was built for.** Every other
//! clause in DESIGN's feed rows can be read off a ledger nothing has been
//! removed from; this one cannot be read off anything else, because
//! [`UsageCollectorPluginError::CursorBeyondRetention`] is unreachable until
//! a sweep has actually removed an entry and no method on
//! [`UsageCollectorPluginV1`] removes one. The drive is [`ContractRetention`],
//! beside the SPI rather than on it, and it arrives through
//! [`run_all_with_retention`](super::super::run_all_with_retention). Handed
//! `None` the check runs the one assertion that needs no sweep and skips the
//! three that do.
//! [`RETENTION_DRIVEN_CHECKS`](super::super::RETENTION_DRIVEN_CHECKS) is the
//! list of the checks in that position, and it rather than this sentence is
//! what a caller reporting coverage reads.

use crate::contract::feed_walk::{WalkStop, feed_walk, ids};
use crate::contract::fixtures::{
    CONTRACT_ACCEPTED_AT, CONTRACT_TENANT_ID, FIXTURE_EPOCH, SCOPE_EXCLUDED_TENANT_ID, check_meter,
    contract_scope, fixture_record_on, violation,
};
use crate::contract::retention::ContractRetention;
use crate::contract::{ContractViolation, FEED_RETENTION_REFUSAL, HARNESS_FAULT};
use crate::error::UsageCollectorPluginError;
use crate::feed::{FeedPosition, FeedStart};
use crate::models::{IdempotencyKey, MeterTypeId, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use uuid::Uuid;

/// The start of the covered period this check's entries are offset from:
/// **four hundred and fifty days** past [`FIXTURE_EPOCH`], for the reason
/// [`WINDOW_SELECTION_FROM`](super::window_end_selection::WINDOW_SELECTION_FROM)
/// gives.
///
/// **Which offsets the other checks hold is deliberately not enumerated
/// here**, for the reason
/// [`feed_bootstrap_position`](super::feed_bootstrap_position())'s own window
/// constant states at length: seven modules carry such a list, every one of
/// them is stale, and a structural replacement is proposed for this slice's
/// closeout. A list that is wrong reads as a guarantee of separation nobody is
/// keeping.
///
/// The separation buys little on the read side: **this check dispatches no
/// range at all**, every read it makes is a feed page, and a feed page selects
/// by subscription rather than by covered period, so the separation that does
/// the work there is [`check_meter`]. The offset is kept because the
/// separation runs the other way as well, and **this check drives two
/// drops**. A drop is keyed on a covered-period floor and a floor reaching
/// into another check's period would take that check's fixtures with it. That
/// the drive is also keyed on a GTS type is the belt to this brace —
/// [`ContractRetention::drop_before`] says why it has to be, and it is also
/// what lets this check and
/// [`feed_bootstrap_position`](super::feed_bootstrap_position()) purge over
/// one shared backend without either disturbing the other.
const FEED_RETENTION_WINDOW_FROM: time::OffsetDateTime =
    FIXTURE_EPOCH.saturating_add(time::Duration::days(450));

/// The floor the sweep this check's assertions are about is driven to: three
/// hours past [`FEED_RETENTION_WINDOW_FROM`].
///
/// It separates the two entries the sweep removes from the two it leaves.
/// [`ContractRetention::drop_before`]'s bound is exclusive, so an entry whose
/// covered period ends exactly at the floor is inside the retention that floor
/// expresses and stays; [`FeedRetentionFixtures::guards`] is what holds the
/// four entries to their sides of it.
const FEED_RETENTION_SWEPT_FLOOR: time::OffsetDateTime =
    FEED_RETENTION_WINDOW_FROM.saturating_add(time::Duration::hours(3));

/// The floor a **declared retention** would permit this backend to sweep to:
/// five hours past [`FEED_RETENTION_WINDOW_FROM`], two hours above the floor
/// anything is actually driven to.
///
/// **Nothing drives it, and that is the point.** DESIGN §3.1's "Plugin-owned
/// lifecycle" row makes the horizon a lower bound rather than a boundary —
/// *"A purge later than the horizon is permitted, which is why the horizon is
/// a floor rather than an exact boundary"* — so a conforming deployment holds
/// entries its own floor would already let it drop. DESIGN §3.3's row then
/// requires a cursor over those entries to be served: *"A cursor whose
/// continuation is intact is served, including one older than the floor where
/// the plugin retains longer than it."*
///
/// [`FEED_RETENTION_BELOW_THE_FLOOR_WINDOW_END`] is the entry that sits
/// between the two floors, and the third assertion is the one that reads it. A
/// backend deciding the refusal from a floor it declares rather than from what
/// a sweep removed refuses that cursor; a backend deciding it from the mark
/// serves it.
const FEED_RETENTION_DECLARED_FLOOR: time::OffsetDateTime =
    FEED_RETENTION_WINDOW_FROM.saturating_add(time::Duration::hours(5));

/// The floor the **emptying** drop is driven to: nine hours past
/// [`FEED_RETENTION_WINDOW_FROM`], above every covered period this check
/// writes to its swept meter.
///
/// See [`empty_the_swept_meter`] for why a check that purges has to finish by
/// emptying the meter it purged, and why *empty* is what this one leaves
/// behind where
/// [`feed_bootstrap_position`](super::feed_bootstrap_position()) leaves a
/// ledger.
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
/// It is above the declared floor as well as above the swept one so that the
/// swept meter holds one retained entry on each side of that floor, which is
/// what keeps the third assertion about the entry *below* it rather than about
/// retention in general.
const FEED_RETENTION_LANDMARK_WINDOW_END: time::OffsetDateTime =
    FEED_RETENTION_WINDOW_FROM.saturating_add(time::Duration::hours(7));

/// The end of the covered period of the first of the two entries on the meter
/// nothing is ever removed from: six hours past
/// [`FEED_RETENTION_WINDOW_FROM`].
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
/// defect is the one DESIGN names twice.** §3.1's `FeedPosition` row fixes a
/// position's age as *"the acceptance instant of the oldest entry of a
/// subscribed GTS type after it — whether or not the reader's authorization
/// scope admits that entry — and a position with no such entry after it is
/// current"*, and then rules that age out as an input: *"That age is a
/// progress measure, not the retention refusal's input (§3.2)."* §3.2 says the
/// same thing from the gateway's side, the plugin deciding the refusal *"from
/// what it still holds rather than from the cursor's age, which a sweep clamps
/// to the retention boundary"*.
///
/// The skew is what makes *"whatever that cursor's own age"* say something
/// here. Both refused cursors are issued immediately before an entry the sweep
/// removes, so before the sweep each carries the older instant this constant
/// names; after the sweep the oldest entry left after either of them is the
/// one below the declared floor, which carries [`CONTRACT_ACCEPTED_AT`]. The
/// sweep therefore **raises** both cursors' ages by an hour, exactly the
/// clamping §3.2 describes, and a backend testing a cursor's age against its
/// retention boundary finds two young cursors and serves both a silently
/// truncated range. Without the skew the ages would not move and the clause
/// would be satisfied by accident.
///
/// That it really is a skew rather than a coincidence is
/// [`FeedRetentionFixtures::guards`]' business rather than this comment's.
const FEED_RETENTION_LOST_ACCEPTED_AT: time::OffsetDateTime =
    CONTRACT_ACCEPTED_AT.saturating_sub(time::Duration::hours(1));

/// The quantity every entry this check writes carries.
///
/// Its value is asserted nowhere: every assertion here reads whether a read
/// was refused, and one of them reads an identity. It is exactly representable
/// in a binary float all the same, which keeps `contract_mutants`'s
/// `Defect::QuantityThroughFloat` from changing anything on the way in and so
/// from reaching this check for a rule that belongs to `quantity-round-trip`.
const FEED_RETENTION_QUANTITY: &str = "1.5";

/// The page limit every read in this check dispatches: **one**.
///
/// The row is about whether a cursor is *refused*, never about how a page is
/// filled, so the narrowest page the SPI admits is enough for every read here.
/// One is chosen rather than tolerated: the walks that issue this check's
/// cursors have to reach the head over a ledger that withholds one of its
/// entries from the grant they run under, and a limit of one is the setting at
/// which every such walk really pages instead of answering in one read.
const FEED_RETENTION_PAGE_LIMIT: u64 = 1;

/// The six entries this check writes and the two meters it reads them over.
struct FeedRetentionFixtures {
    /// The meter the sweep is driven over, and the meter the three driven
    /// assertions read.
    swept: MeterTypeId,
    /// The meter **no drop of this check's is ever driven over**, and the
    /// meter the undriven assertion reads.
    ///
    /// A meter derived for this check's exclusive use is the only way to have
    /// one: `super::super::run_all` dispatches every check against one
    /// backend, and a retention mark only ever rises, so a meter this check
    /// swept on an earlier run carries that mark into every run after it. The
    /// undriven assertion is about a cursor over a ledger nothing has been
    /// removed from, and only a meter nothing removes from is one.
    intact: MeterTypeId,
    /// The oldest entry on the swept meter in the feed's order, and the entry
    /// the first cursor is issued after. Its covered period is above both
    /// floors, so no drop but the emptying one removes it.
    landmark: UsageRecord,
    /// The entry the sweep removes that this read's scope **admits**. Second
    /// in the feed's order.
    admitted_loss: UsageRecord,
    /// The entry the sweep removes that this read's scope **never admitted**.
    /// Third in the feed's order, and the entry the second assertion is about.
    excluded_loss: UsageRecord,
    /// The entry the sweep leaves behind although
    /// [`FEED_RETENTION_DECLARED_FLOOR`] would permit removing it. Last in the
    /// feed's order, and the entry the third assertion requires a served
    /// continuation to carry.
    below_the_floor: UsageRecord,
    /// The two entries on the meter nothing removes from, in the order they
    /// are submitted.
    intact_ledger: Vec<UsageRecord>,
}

/// The three cursors the driven half issues, each named for the entry the
/// swept ledger ended at when it was taken.
///
/// Every one of them is the head of the swept meter at that moment, so what
/// lies after it is exactly what the staging submits next. What each assertion
/// reads is therefore what the sweep removes from *that* remainder, which is
/// what the doc on each field states.
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
/// entry of a subscribed GTS type is refused rather than served as a short
/// page, whatever that cursor's own age and whether or not the caller's scope
/// admitted that entry. A cursor whose continuation is intact is served,
/// including one older than the floor where the plugin retains longer than
/// it."* (DESIGN §3.3, "Plugin contract tests", line 1325.)
///
/// **The row is one rule stated from both sides**: the refusal reads what a
/// sweep *removed*, and nothing else. Three things are named as not being
/// inputs to it — the cursor's age, the caller's scope, and (in the second
/// sentence) the floor itself — and each of the three is a backend somebody
/// would write.
///
/// # What is asserted
///
/// 1. **A cursor whose continuation a sweep truncated is refused**, with
///    [`UsageCollectorPluginError::CursorBeyondRetention`] and not with a
///    short page. Two entries are submitted after this cursor was issued and
///    both are then swept away; one of them is under the tenant this read's
///    scope admits.
/// 2. **It is refused whether or not the caller's scope admitted the entry
///    that was removed.** The second cursor is issued between the two losses,
///    so the only entry the sweep removes after it belongs to a tenant the
///    grant this read runs under never admitted. It must still be refused.
/// 3. **A cursor whose continuation is intact is served**, and the
///    continuation is delivered. The third cursor is issued after both losses
///    and the entry after it is one [`FEED_RETENTION_DECLARED_FLOOR`] would
///    permit removing and the sweep did not.
/// 4. **A cursor over a ledger nothing has been removed from is served.** The
///    meter this one is issued over is the one no drop of this check's is
///    driven against.
///
/// **Assertion 4 is the only one that needs no drive**, and it is what this
/// check contributes to a [`run_all`](super::super::run_all) run.
/// [`RETENTION_DRIVEN_CHECKS`](super::super::RETENTION_DRIVEN_CHECKS) is the
/// constant a caller reporting coverage reads to find that out.
///
/// # Where *"whatever that cursor's own age"* is asserted
///
/// Not in an assertion of its own, and it could not be: age is not an output
/// of any SPI call, so nothing here can read a cursor's age back and compare
/// it. It is a property of the **fixtures** instead, and
/// [`FEED_RETENTION_LOST_ACCEPTED_AT`] is where it lives: the two entries the
/// sweep removes carry an acceptance instant an hour earlier than every entry
/// it leaves, so the sweep *raises* the age of both refused cursors — the
/// clamping DESIGN §3.2 names — and assertions 1 and 2 are then assertions
/// about cursors a backend testing an age would find young enough to serve.
/// [`FeedRetentionFixtures::guards`] holds the skew.
///
/// # Why the driven half builds its ledger in front of its cursors
///
/// A cursor cannot be asked for; it is whatever a read hands back. This check
/// needs three of them at three points in one ledger, and the only way to know
/// what lies after a cursor is to have written nothing there yet. So the
/// driven half **submits, reads to the head, submits, reads to the head**: at
/// each step the walk's own exit condition — the cursor standing still —
/// fixes the position as one the whole ledger so far is behind, and everything
/// submitted afterwards is in front of it. DESIGN §3.1's Feed order invariant
/// is what makes that exact rather than incidental: *"A page reaching the
/// settled head returns its cursor at the head, so a cursor's age reflects
/// its consumer's progress, not the last arrival."*
///
/// The alternative would be to read a page at a chosen limit and name the
/// position it stopped at, which reads a backend's page arithmetic rather than
/// its retention: **a short page is conforming**, so a backend that answered a
/// shorter page than the limit allows would hand back a position earlier than
/// this check expected and fail an assertion about something else entirely.
///
/// **The staging therefore needs an empty meter to build into**, and what
/// guarantees one is the restoration at the other end: this check leaves its
/// swept meter empty rather than restocked, and [`empty_the_swept_meter`] says
/// why that is the restoration rather than a shortcut past one. Nothing
/// submits to that meter outside the driven half, so an undriven run leaves it
/// alone and a driven one hands the next its own empty ledger.
///
/// # Surviving a repeated run
///
/// Both entry points are dispatched against one persistent backend, and two
/// tests require a second dispatch to be as green as the first:
/// `the_reference_backend_conforms_to_a_repeated_run` over
/// `super::super::run_all`, and
/// `the_reference_backend_conforms_to_a_repeated_run_under_a_retention_drive`
/// over `super::super::run_all_with_retention`.
///
/// **This is the second check that purges and the discipline is
/// [`feed_bootstrap_position`](super::feed_bootstrap_position())'s**, taken
/// one step further. A purged entry is not absorbed on re-delivery: it is
/// gone, so re-delivering it is an insert, and an insert joins the feed at the
/// head rather than back where it was. That check answers by putting its two
/// entries back in its own order. This one answers by **leaving its swept
/// meter empty**, because an order is not what it needs: what it needs is that
/// the four entries of its swept ledger arrive *after* the cursors it issues,
/// and the one state from which that is true on every run is an empty meter.
/// The restoration is therefore a removal, and it is driven **whatever the
/// driven half did** — a run that gave up on its own assertions still empties
/// the meter, because a run that left entries behind would decide the next
/// one. [`ContractRetention`]'s module docs are where the obligation to
/// restore is stated, and [`empty_the_swept_meter`] says why this check's
/// restoration is not also made before the staging rather than only after it.
///
/// **The two driven checks do not disturb each other**, and the reason is
/// structural rather than a matter of ordering:
/// [`ContractRetention::drop_before`] is keyed on a GTS type, and each of them
/// drops on a meter [`check_meter`] derived for its own exclusive use. Neither
/// check's floors are comparable with the other's and neither needs them to
/// be.
///
/// The meter assertion 4 reads is never swept at all, which is a discipline of
/// its own: a retention mark only ever rises, so a check that swept a meter
/// once could never again read an unswept ledger over it.
///
/// # Which assertions a subject reaches, measured
///
/// Every assertion below was inverted and confirmed to fire against the
/// reference backend, so none is dead: assertions 1, 2 and 3 fire only under
/// a drive, and assertion 4 fires under both entry points, which is the
/// mechanical form of the split this check's coverage claim rests on. What
/// each *catches* was then measured by neutering it and re-running both
/// discrimination matrices. Four subjects reach this check, all four in
/// `contract_mutants` and all four built on `MutantLedger` — the refusal is a
/// branch of `read_feed_page` itself, and a wrapper delegating to a conforming
/// backend cannot make that backend fail to refuse:
///
/// * **`Defect::ServesAShortPageWhereRetentionTruncatedACursor`** — the
///   subject the row names outright — is reported by assertions 1 and 2, and
///   by **neither of them alone**. It never refuses, so both cursors come back
///   served; neutering either leaves the other reporting, and neutering both
///   takes this check out of that subject's row. What *is* individually
///   load-bearing is the branch inside the shared probe that reports a
///   **served** page: neutering that one branch empties this subject's row and
///   the next one's together.
/// * **`Defect::TheRetentionRefusalReadsTheCallersGrant`** is reported by
///   assertion 2 **alone**, measured by neutering each call site in turn. That
///   is what makes assertion 2 individually load-bearing and what makes
///   *"whether or not the caller's scope admitted that entry"* a clause this
///   check establishes rather than restates. The subject refuses assertion 1's
///   cursor correctly, which is the whole difference between the two.
/// * **`Defect::RefusesEveryCursorOnceASweepHasRun`** is reported by assertion
///   3 **alone**, and by its **refusal** branch alone: neutering that branch
///   empties the subject's row while neutering the delivery branch beside it
///   changes nothing. It is the subject for the row's second sentence — a
///   backend deciding the refusal from a floor rather than from what was
///   removed passes both refusal assertions and fails the one that requires an
///   intact continuation to be served.
/// * **`Defect::TheRetentionRefusalIgnoresTheSubscription`** is reported by
///   assertion 4 **alone**, and by its refusal branch alone. It is what makes
///   this check's one undriven assertion load-bearing rather than a floor
///   nothing tests, and it is only reachable because assertion 4 is taken
///   **last**: under a drive it then runs while this check's other meter
///   carries a mark, and a backend reading its marks across every type at once
///   refuses a cursor over a ledger nothing has been removed from.
///
/// **Assertion 1 is therefore not individually load-bearing, and it stays.**
/// Everything it catches, assertion 2 catches too. What it adds is the report
/// a plugin author reads: assertion 2's names an entry the caller's grant
/// never admitted, which is the exotic half of the row, and a backend with no
/// marks table at all should be told about the ordinary half first. The pair
/// is what the row states and the pair is what fires.
///
/// **None of the four is in the discrimination matrix**, and none could be:
/// `run_all` drives nothing, so under the matrix's dispatch all four are
/// behaviourally the reference backend. They are rows of `contract_tests`'
/// `RETENTION_DRIVEN_MATRIX`, which asserts an empty column against
/// `run_all` and the named one against `run_all_with_retention`.
///
/// # What this check pins that no undriven check can
///
/// `contract_mutants`' `MutantLedger` is a hand-written mirror of the
/// reference backend, and the undriven matrix keeps it honest by running every
/// check against subjects built on it. Two of its behaviours could not be kept
/// honest that way, and both are now:
///
/// * **Its retention sweep and its cursor refusal.** Neither existed until
///   this check needed them, and both are mirrors of the reference's.
/// * **Its feed seek.** Over a dense, append-ordered ledger a seek to the
///   first greater sequence and a skip of that many rows name the same entry,
///   so `sequence > from` could be replaced by a count and the undriven matrix
///   would stay green — measured, and still true. **Only a gap separates
///   them**, a retention drop is what makes one, and this is the check that
///   reads across a gap. Measured again with the skip in place, assertion 3's
///   refusal branch reports it: under a skip the walk from a cursor whose
///   continuation is intact never reaches the head. `contract_tests`'
///   `a_subject_carrying_its_own_ledger_is_still_itself_under_a_drive` is
///   where that is asserted, and its subject is chosen for touching no feed
///   path of its own.
///
/// # Assertions no subject reaches, and why each stays
///
/// Neutering any of these changed no row of either matrix, which is the
/// measurement rather than an expectation.
///
/// * **The refusal probe's second branch**, which fires when a read is refused
///   with some error other than
///   [`UsageCollectorPluginError::CursorBeyondRetention`]. The row names the
///   variant, and a backend answering `Internal` where the contract names a
///   caller-actionable refusal has broken something a plugin author needs told
///   apart from serving a short page: one is a consumer that bootstraps again,
///   the other a consumer that retries the same cursor forever. No subject
///   reaches it because a subject that answered the wrong variant would be
///   modelling a mis-typed error rather than a mis-decided refusal. **A
///   recorded gap**, kept because the two failures read completely differently
///   to whoever gets the report.
/// * **Assertion 3's delivery branch**, which fires when an intact
///   continuation is answered without the entry in front of the cursor. The
///   skip above was the nearest thing to a subject for it and it reached the
///   refusal branch instead. **A recorded gap**, kept because "answered `Ok`
///   and carried nothing" is a distinct failure from "refused", and a probe
///   that stopped at the answer would report a lost continuation as a served
///   one.
/// * **Assertion 4's walk-failure branch**, and **the walk-failure report in
///   [`stage`]**. Both fire when a live feed read is refused outright or when
///   its cursor never stands still, over a meter nothing has removed anything
///   from; a subject producing either would produce it for every check that
///   walks — the report comes from [`super::super::feed_walk`], which is a
///   reader rather than a check and says so. **A recorded gap shared with
///   every caller of that walk**, and not this check's to close alone.
/// * **The two drive-failure reports**, in [`empty_the_swept_meter`] and
///   [`sweep_and_read_each_cursor`]. **No subject can reach these and it is
///   not for want of one.** They fire when [`ContractRetention::drop_before`]
///   answers `Err`, which is the suite failing to set a scenario up rather
///   than a backend answering an SPI call — which is why both are reported as
///   [`HARNESS_FAULT`] and why that error type is a `String` rather than a
///   plugin error. **Unreachable by construction, recorded rather than
///   removed**, because a drive that started failing silently would leave
///   assertions 1 and 2 asserting a refusal over a ledger nothing had been
///   removed from.
/// * **The submission reports** in [`submit`]. No subject in the suite refuses
///   a well-formed record carrying a fresh idempotency key: the two that
///   refuse anything refuse a *collision*, and this check submits none. **A
///   recorded gap shared with every other check's submission guard.**
/// * **The seven fixture guards** in [`FeedRetentionFixtures::guards`]. Each
///   was inverted and each fires, so none is dead; neutering all seven
///   together changes no row. They are the suite's own facts rather than the
///   plugin's, so no backend outcome can make one fire and no subject could be
///   built that did. **Unreachable by any plugin, deliberately**, which is
///   what [`HARNESS_FAULT`] is for. They fire for an edit to this module,
///   which is the only thing that can break them and the reason they are here.
///
/// # What this check does not reach
///
/// It asserts nothing about **which** entries a served page carries beyond the
/// one entry assertion 3 names, and nothing about the order they arrive in.
/// Those are `feed-snapshot-and-replay`'s and `feed-completeness`'s, and a
/// second check asserting them would report one mistake under two names.
///
/// It dispatches no bounded replay and compares no two positions — DESIGN's
/// `feed-position-bounded` row is the one about a position's size. It writes
/// no invalidation and drives no concurrency.
///
/// It also asserts nothing about `FeedStart::Oldest`. That start mode is
/// exempt from the refusal, and the exemption is
/// [`feed_bootstrap_position`](super::feed_bootstrap_position())'s row rather
/// than this one's: DESIGN gives it *"never refused on the retention floor"*
/// there, and this check reaches only the refusal that clause is an exemption
/// from.
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

    // A refusal stops the check. Every assertion below is a statement about a
    // cursor over a ledger that holds entries, and over an empty one they hold
    // vacuously; a check that passes because nothing was stored is the one
    // outcome worse than a failing one.
    let mut violations = submit(
        plugin,
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
/// ([`empty_the_swept_meter`]). The third runs whatever the first two did, and
/// the `match` below is written that way on purpose rather than as an early
/// return.
async fn the_refusal_reads_what_was_removed(
    plugin: &dyn UsageCollectorPluginV1,
    retention: &dyn ContractRetention,
    fixtures: &FeedRetentionFixtures,
) -> Vec<ContractViolation> {
    let mut violations = match stage_the_swept_ledger(plugin, fixtures).await {
        Ok(cursors) => sweep_and_read_each_cursor(plugin, retention, fixtures, &cursors).await,
        Err(violations) => violations,
    };
    // Unconditional, and the `match` above is written to make it so: the
    // emptying is the only thing that puts this meter back, so a run that gave
    // up on its own assertions must not also leave the next run a ledger its
    // cursors would be issued behind rather than in front of.
    violations.extend(empty_the_swept_meter(retention, fixtures).await);
    violations
}

/// Drives the sweep the three assertions are about and reads each cursor back.
///
/// The drop is the only thing that happens between the staging and the reads,
/// so a difference in how the three cursors are answered can be nothing but
/// the sweep.
///
/// A failure to drive is reported as a [`HARNESS_FAULT`] rather than against
/// the plugin, for the reason [`ContractRetention::drop_before`]'s error type
/// gives: a backend that could not be driven has not answered an SPI call
/// wrongly, and putting its name on this would be blaming it for a scenario
/// the suite failed to set up.
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
/// loss the scope admits, a cursor, the loss the scope withholds, a cursor,
/// and the entry the sweep leaves behind. Each cursor is the head of the
/// ledger at the moment it is taken, so exactly the entries submitted after it
/// are in front of it.
///
/// `Err` carries the violations that stopped the staging, which are a refused
/// submission or a walk that could not reach the head. Either leaves the
/// ledger in a shape the three assertions are not about, so none of them runs.
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

/// Submits one entry and answers with the position the feed has reached over
/// the swept meter.
///
/// The walk's exit is the cursor standing still, which is what makes the
/// position the head rather than wherever a page happened to stop:
/// [`super::super::feed_walk`] follows the cursor to a fixpoint, and DESIGN
/// §3.1's Feed order invariant has *"A page reaching the settled head returns
/// its cursor at the head"*. Everything submitted after this call is
/// therefore in front of the position it answered with.
///
/// The walk runs under [`contract_scope`], the same grant the assertions read
/// under, and over a ledger one of whose entries that grant withholds. That is
/// deliberate: a position denotes a prefix of the ledger rather than of what a
/// grant admits, so a walk that could not get past an entry it may not carry
/// would be a backend whose cursor counts admitted entries — which
/// `feed-snapshot-and-replay` owns and reports.
async fn stage(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FeedRetentionFixtures,
    entry: &UsageRecord,
    role: &str,
) -> Result<FeedPosition, Vec<ContractViolation>> {
    let refused = submit(plugin, std::slice::from_ref(entry), role).await;
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
/// obligation is the same one both times and a report that said it differently
/// would read as two rules rather than one applied twice.
/// `what_was_removed` is what tells the two apart in a report, and it is the
/// whole of the difference the row draws: the first cursor has an entry the
/// grant admits removed after it, the second has only an entry the grant never
/// admitted.
///
/// **A short page is the failure the row names**, and it is the dangerous one:
/// a refusal is an error a consumer can act on, and a short page is a gap
/// it cannot detect. A refusal carrying some other variant is reported
/// separately, because a plugin author fixing a mis-typed error and one fixing
/// a mis-decided refusal are doing different work.
///
/// The read is a single page rather than a walk. A walk would fold the refusal
/// into its own exit condition and report it as a failed walk, leaving nothing
/// for this check to say about which of the two obligations a backend broke.
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
/// [`FEED_RETENTION_DECLARED_FLOOR`] would permit removing and the sweep did
/// not. That is DESIGN's *"including one older than the floor where the plugin
/// retains longer than it"*, and it follows from the mark recording what was
/// **actually removed** rather than what a floor would permit removing.
///
/// **The delivery is asserted as well as the answer**, and the two are one
/// obligation rather than two: a page that answers `Ok` and hands back a
/// cursor at the head without the entry in front of it has not served that
/// continuation, it has lost it. The read is a walk rather than a page for the
/// reason [`super::super::feed_walk`] exists — a short page is conforming, so
/// a single page proves nothing about what a continuation carries — and the
/// walk's own refusal report is what carries the `Err` half.
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
/// check contributes to a [`run_all`](super::super::run_all) run. It is the
/// floor under the other three: a backend that refused a resumed read whatever
/// its retention had done would satisfy assertions 1 and 2 and break every
/// consumer there is.
///
/// The meter is the one no drop of this check's is driven over, because a
/// retention mark only ever rises and a meter swept on an earlier run is not a
/// ledger nothing has been removed from on a later one.
///
/// **It is taken last**, after the driven half rather than before it. Under a
/// drive this check's other meter then carries a mark, so a backend whose mark
/// is keyed on nothing but the sweep — refusing a cursor over one type because
/// another was swept — is refused here. `usage-collector-v1.yaml` states the
/// obligation that would break: removal *"is read per subscribed GTS type, so
/// a cursor can be refused for an entry the caller's own scope excluded"*.
///
/// Nothing is asserted about what the page carries. The row is about whether a
/// cursor is refused, and what a served page carries is
/// `feed-snapshot-and-replay`'s and `feed-completeness`'s. One consequence is
/// worth naming: a backend whose bootstrap read begins at the head hands this
/// probe a cursor at the head, which is then served and reported by nothing
/// here. That defect is `feed-bootstrap-position`'s, and a guard here would
/// report one mistake under two names.
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
/// **This is the price of being a check that purges**, and
/// [`feed_bootstrap_position`](super::feed_bootstrap_position()) pays it a
/// different way. The suite's premise is that a repeated run re-delivers
/// identical entries and a conforming backend absorbs them, so the ledger a
/// check reads is the ledger it read before. A purged entry breaks that
/// premise: it is gone, so re-delivering it is an insert, and an insert joins
/// the feed at the head rather than back where it was. That check answers by
/// re-delivering its two entries in its own order, because what it asserts is
/// which of them a read begins at. This one asserts what lies *after* cursors
/// it issues, and there is only one ledger from which that is the same on
/// every run: the one its own staging builds, from nothing.
///
/// So the drop is driven to [`FEED_RETENTION_RESTORING_FLOOR`], above every
/// covered period this check writes to this meter, and nothing is
/// re-delivered. [`ContractRetention`]'s module docs are where the obligation
/// is stated and
/// `the_reference_backend_conforms_to_a_repeated_run_under_a_retention_drive`
/// is what holds every driven check to it.
///
/// **It is the only drop driven here that is not the assertions' own, and it
/// is made after them rather than also before them.** A second, defensive drop
/// at the top of the driven half would look free and is not: every cursor this
/// check issues comes from a `FeedStart::Oldest` walk, that start mode is the
/// one DESIGN §3.3 exempts from the retention refusal, and a walk taken over a
/// meter this check had *already* swept would exercise the exemption
/// `feed-bootstrap-position` owns. A backend that refuses `FeedStart::Oldest`
/// after a sweep would then be reported by this check as well, and one mistake
/// would be reported under two names. Taking every cursor before this check
/// has driven anything is what keeps its own reads inside the part of the
/// contract it is not testing.
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
/// Every entry is submitted whatever the ones before it did: the assertions
/// read ledgers every entry belongs to, and a submission that stopped at its
/// first refusal would leave a shorter ledger than the report describes.
/// `role` names what the submission was for, because this check submits at
/// five different points and a refusal at each leaves a different ledger.
async fn submit(
    plugin: &dyn UsageCollectorPluginV1,
    entries: &[UsageRecord],
    role: &str,
) -> Vec<ContractViolation> {
    let mut violations = Vec::new();
    for entry in entries {
        if let Err(err) = plugin.create_usage_record(entry.clone()).await {
            violations.push(violation(
                FEED_RETENTION_REFUSAL,
                format!(
                    "`create_usage_record` refused {role} (entry {id}, tenant {tenant}, covered \
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
/// **The covered periods are deliberately not in admission order**, and that
/// is the one thing about these fixtures worth reading twice. The feed's order
/// is the order entries were admitted in; a retention floor is a bound on the
/// covered period. The two are independent, and DESIGN says so by having
/// backfill exist at all — an entry accepted now may cover a period long past.
/// This check needs an entry that is **after** a cursor in the feed's order
/// and **below** a floor in covered period, and only that independence makes
/// one possible. So the two entries the sweep removes are submitted third and
/// fifth while carrying the two earliest covered periods of the six.
///
/// Everything else is held fixed: one quantity, one resource, one meter per
/// role. The two attributes that vary do the work — the covered period decides
/// what the sweep removes, and the tenant decides what the grant admits.
///
/// Seven guards keep the check from passing by construction, and all seven are
/// the suite's own facts rather than the plugin's, so all seven are reported as
/// [`HARNESS_FAULT`]. They are [`FeedRetentionFixtures::guards`]'.
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
     -> Result<UsageRecord, String> {
        fixture_record_on(
            swept.clone(),
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
            intact.clone(),
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
    fn swept_ledger(&self) -> [&UsageRecord; 4] {
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
                swept = self.swept.as_str(),
            ));
        }

        let entries: Vec<&UsageRecord> = self
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
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture) spells
/// out: in a ledger with no delete path, an edited fixture must take a fresh
/// identity rather than inherit an accepted entry's. **This check does have a
/// delete path**, as
/// [`feed_bootstrap_position`](super::feed_bootstrap_position()) does, and the
/// reasoning survives it unchanged: the delete is driven over a covered-period
/// floor rather than over an identity, so an edited fixture is not the thing
/// it removes.
fn feed_retention_key(role: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{FEED_RETENTION_REFUSAL}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}
