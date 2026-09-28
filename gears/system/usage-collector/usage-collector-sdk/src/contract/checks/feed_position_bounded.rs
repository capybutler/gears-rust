//! The DESIGN §3.3 `feed-position-bounded` check.
//!
//! See [`feed_position_bounded`] for what it asserts. The module holds the
//! twenty-four entries it writes, the four meters it writes them to, the ten
//! tenants it attributes them to and the one grant every read here
//! dispatches. The reads themselves are [`super::super::feed_walk`]'s,
//! shared with the four other feed checks.
//!
//! **It is the only check in the suite whose rule is about a position's
//! encoding rather than about what a page carries**, and that shows in what
//! it reads: it asserts nothing whatever about the entries a page delivered.
//! [`FeedPosition::len`] is the whole of its subject matter, and that method
//! exists for it.
//!
//! Two neighbours are worth telling apart.
//! [`feed_snapshot_and_replay`](super::feed_snapshot_and_replay()) also
//! compares two positions by size, and compares them across two **grants**
//! over one subscription; this check holds the grant fixed and varies the
//! **subscription**, which is the object DESIGN states the rule over.
//! `contract_tests`' `a_feed_position_denotes_the_same_ledger_prefix_under_every_grant`
//! and `a_bounded_page_limit_hands_two_grants_two_positions_that_each_resume`
//! pin a position's other two properties — that it denotes one ledger prefix
//! and that it resumes — and nothing here re-states either.

use std::collections::BTreeSet;

use toolkit_odata::ast;
use uuid::Uuid;

use crate::contract::feed_walk::{WalkStop, feed_walk};
use crate::contract::fixtures::{
    CONTRACT_ACCEPTED_AT, check_meter, check_window_from, contract_tenant, fixture_record_on,
    tenant_disjunction, violation,
};
use crate::contract::{ContractViolation, FEED_POSITION_BOUNDED, HARNESS_FAULT};
use crate::feed::{FeedPosition, FeedStart};
use crate::models::{IdempotencyKey, MeterTypeId, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;

/// The start of the covered period every entry of this check carries.
///
/// The offset [`check_window_from`] tables for this check, for the reason
/// that accessor gives.
///
/// It buys as little here as it does for
/// [`feed_completeness`](super::feed_completeness()), and for the same
/// reason: **this check dispatches no range at all.** Every read it makes is
/// a feed page, and a feed page selects by subscription rather than by
/// covered period, so the separation that does the work is [`check_meter`].
/// The offset is kept because the separation runs the other way as well:
/// `super::super::run_all` dispatches every check against one backend that
/// never removes an entry, so these twenty-four entries are there for every
/// check that *does* read a range, and a period no other check's range
/// covers is what keeps them out of those reads.
const FEED_POSITION_WINDOW_FROM: time::OffsetDateTime =
    check_window_from(FEED_POSITION_BOUNDED, "main");

/// The end of that covered period. All twenty-four entries carry it: they
/// are separated by their meter, their tenant and their idempotency key, and
/// nothing here asks a covered period to tell two entries apart.
const FEED_POSITION_WINDOW_END: time::OffsetDateTime =
    FEED_POSITION_WINDOW_FROM.saturating_add(time::Duration::hours(1));

/// The quantity every entry of this check carries.
///
/// Its value is asserted nowhere: the only figures this check compares are
/// two byte counts. It is exactly representable in a binary float all the
/// same, which keeps `contract_mutants`'s `Defect::QuantityThroughFloat`
/// from changing anything on the way in and so from reaching this check for
/// a rule that belongs to `quantity-round-trip`.
const FEED_POSITION_QUANTITY: &str = "1.5";

/// How many tenants the **broad** meter's entries are attributed to:
/// **eight**, one per entry.
///
/// DESIGN's row asks for *"a subscription spanning many tenants"* against
/// *"one spanning few"*, and eight against [`FEED_POSITION_FEW_TENANTS`]'s
/// two is the contrast. The figure is chosen against the shape of the defect
/// rather than in the abstract: a position keyed per tenant carries one
/// component per tenant, so eight components against two is a difference no
/// plausible framing can swallow. Three against two would be a difference of
/// one component, which a plugin padding its position to a fixed block, or
/// length-prefixing it with a variable-width integer, could mask — and this
/// check would then report nothing against a backend whose position really
/// does grow with breadth.
///
/// More than eight changes nothing about that and only lengthens a report.
/// It also has a ceiling that is not this check's to set: the position bound
/// is [`MAX_FEED_POSITION_BYTES`](crate::feed::MAX_FEED_POSITION_BYTES), and
/// a subject modelling a per-tenant key has to stay inside it to be a
/// subject about size rather than about refusal.
const FEED_POSITION_MANY_TENANTS: u32 = 8;

/// How many tenants the **narrow** meter's entries are attributed to:
/// **two**.
///
/// The row says *"few"* rather than *"one"*, and two rather than one is the
/// reading that keeps the narrow side an ordinary subscription. A
/// single-tenant subscription is the one case a plugin might reasonably
/// special-case — a per-tenant key degenerates to a single component there —
/// so a comparison against it would leave a backend that emits a compact
/// position for one tenant and a vector for two reported for the wrong
/// reason. Two is the fewest that is not that case.
const FEED_POSITION_FEW_TENANTS: u32 = 2;

/// How many entries each subscription this check reads holds: **eight**.
///
/// Equal on all three reads, and that equality is what makes the comparison
/// about breadth. A position that grew with the *length* of the ledger it
/// positions rather than with the breadth of the subscription would be a
/// different defect, and holding the entry count fixed is what keeps this
/// check from reporting one as the other.
///
/// It is [`FEED_POSITION_MANY_TENANTS`] rather than a figure of its own
/// because the broad meter takes one entry per tenant, which is the simplest
/// ledger that spans that many.
const FEED_POSITION_ENTRIES: u32 = FEED_POSITION_MANY_TENANTS;

/// How many of those entries each half of the split pair holds: **four**.
///
/// Spelled out rather than divided, and the relation to
/// [`FEED_POSITION_ENTRIES`] is a compile-time assertion below. The two
/// halves have to add up to exactly the narrow meter's count or the second
/// comparison varies the ledger's length alongside the number of GTS types
/// named, which is the one thing that comparison is meant to hold still.
const FEED_POSITION_SPLIT_HALF: u32 = 4;

/// The first index this check takes from [`contract_tenant`]'s block.
///
/// [`latest_tie_break`](super::latest_tie_break()) holds 0 and 1 and
/// [`feed_snapshot_and_replay`](super::feed_snapshot_and_replay()) 2 and 3,
/// so this check takes the ten from 4 upwards: the broad meter's
/// [`FEED_POSITION_MANY_TENANTS`] and then the narrow side's
/// [`FEED_POSITION_FEW_TENANTS`].
///
/// The block is held clear of the suite's four named tenant ids by a
/// compile-time assertion in [`super::super::fixtures`], so these ten cannot
/// collide with a premise another check rests on. Sharing an index with
/// another check would be harmless in any case — what a tenant owns is owned
/// on a meter, and every meter here is [`check_meter`]'s — but a block of
/// this check's own keeps the grant below readable.
const FEED_POSITION_FIRST_TENANT: u32 = 4;

/// The page limit every read this check dispatches: **sixteen**, twice the
/// entries any one of them meets.
///
/// The margin is deliberate and it is not an optimisation. A short page is
/// conforming — `limit` bounds what a page *carries* and promises nothing
/// about everything settled fitting on one — so this check follows the
/// cursor rather than assuming otherwise, and the margin is what makes the
/// ordinary case a single page all the same.
///
/// **The same limit is dispatched by every read this check compares.** A
/// page limit changes where a page boundary falls, so two reads taken at two
/// limits could be issued positions that differ for a reason that is about
/// the limit rather than about the subscription.
const FEED_POSITION_PAGE_LIMIT: u64 = 16;

// The four facts the constants above have to satisfy for the two comparisons
// below to be about what they say they are about. Established when the crate
// compiles, because each of them is a property of these literals alone and a
// runtime guard for one would report the suite's own arithmetic to a plugin
// author as a harness fault.
const _: () = {
    assert!(
        FEED_POSITION_MANY_TENANTS > FEED_POSITION_FEW_TENANTS,
        "the broad meter's entries must span more tenants than the narrow one's, or the \
         comparison DESIGN's row asks for - many against few - is a comparison of two \
         subscriptions of one breadth"
    );
    assert!(
        FEED_POSITION_FEW_TENANTS >= 2,
        "the narrow side must span more than one tenant: a single-tenant subscription is the one \
         case a per-tenant key degenerates in, and a backend that special-cases it would be \
         reported for the wrong reason"
    );
    assert!(
        FEED_POSITION_SPLIT_HALF * 2 == FEED_POSITION_ENTRIES,
        "the two halves of the split pair have to add up to the narrow meter's entry count, or \
         the two-type subscription holds a different number of entries from the one-type one and \
         the second comparison varies the ledger's length as well as its breadth"
    );
    assert!(
        FEED_POSITION_PAGE_LIMIT > FEED_POSITION_ENTRIES as u64,
        "the page limit must exceed the entries any one read meets, or a conforming backend's \
         first page stops on the limit and the reads are compared at boundaries the limit chose"
    );
};

/// One meter and the entries this check wrote to it.
struct StockedMeter {
    /// The meter, derived for this check's exclusive use.
    meter: MeterTypeId,
    /// The entries written to it, in submission order.
    entries: Vec<UsageRecord>,
}

impl StockedMeter {
    /// The distinct tenants its entries are attributed to.
    fn tenants(&self) -> BTreeSet<Uuid> {
        self.entries.iter().map(|entry| entry.tenant_id).collect()
    }

    /// A subscription naming this meter alone.
    fn subscription(&self) -> Vec<MeterTypeId> {
        vec![self.meter.clone()]
    }
}

/// The four meters this check writes and the twenty-four entries on them.
struct FeedPositionFixtures {
    /// [`FEED_POSITION_ENTRIES`] entries under
    /// [`FEED_POSITION_MANY_TENANTS`] tenants, one each.
    many: StockedMeter,
    /// [`FEED_POSITION_ENTRIES`] entries under
    /// [`FEED_POSITION_FEW_TENANTS`] tenants, dealt round-robin.
    few: StockedMeter,
    /// The same entry count and the same tenants as [`Self::few`], split
    /// across two meters — half each.
    ///
    /// A subscription naming both is one type wider than a subscription
    /// naming [`Self::few`], and identical to it in every other way this
    /// check can hold fixed: the same number of entries, over the same
    /// tenants, over one covered period. That is what makes the second
    /// comparison about the number of GTS types named and nothing else.
    split: [StockedMeter; 2],
}

impl FeedPositionFixtures {
    /// Every entry this check submits, in submission order.
    fn all(&self) -> Vec<&UsageRecord> {
        let mut entries: Vec<&UsageRecord> = self.many.entries.iter().collect();
        entries.extend(self.few.entries.iter());
        for half in &self.split {
            entries.extend(half.entries.iter());
        }
        entries
    }

    /// The subscription naming both halves of the split pair.
    fn split_subscription(&self) -> Vec<MeterTypeId> {
        self.split.iter().map(|half| half.meter.clone()).collect()
    }

    /// The four meters, in the order they are described.
    fn meters(&self) -> Vec<&MeterTypeId> {
        let mut meters = vec![&self.many.meter, &self.few.meter];
        meters.extend(self.split.iter().map(|half| &half.meter));
        meters
    }

    /// How many entries the split pair holds between them.
    fn split_entries(&self) -> usize {
        self.split.iter().map(|half| half.entries.len()).sum()
    }

    /// The distinct tenants the split pair's entries are attributed to.
    fn split_tenants(&self) -> BTreeSet<Uuid> {
        self.split
            .iter()
            .flat_map(|half| half.tenants().into_iter())
            .collect()
    }

    /// The five facts this check's two comparisons read, established rather
    /// than assumed.
    ///
    /// All five are the suite's own arithmetic rather than the plugin's, so
    /// all five are reported as [`HARNESS_FAULT`].
    ///
    /// * **The four meters are four.** Two of them being one value would
    ///   make a subscription naming both a subscription naming one, and the
    ///   second comparison would then compare a subscription with itself.
    /// * **The broad meter spans [`FEED_POSITION_MANY_TENANTS`] tenants and
    ///   the narrow one [`FEED_POSITION_FEW_TENANTS`].** This is the premise
    ///   DESIGN's row names — *"a subscription spanning many tenants"* —
    ///   and without it the first comparison is between two subscriptions
    ///   of the same breadth, which a backend keyed per tenant passes.
    /// * **The split pair spans exactly the narrow meter's tenants.** If it
    ///   spanned others, the second comparison would vary the tenant count
    ///   as well as the type count and a per-tenant key would be reported
    ///   under a message about GTS types.
    /// * **All three subscriptions hold the same number of entries.** A
    ///   position that grew with the ledger's length rather than with the
    ///   subscription's breadth is a different defect, and an unequal count
    ///   is what would let this check report one as the other.
    /// * **The twenty-four entries derive twenty-four distinct ids.** They
    ///   do — the meter, the tenant and the idempotency key are three of the
    ///   six identity inputs — but if two ever collided, one submission
    ///   would be absorbed as a retry of the other and a subscription would
    ///   hold fewer entries than the count above claims.
    fn guards(&self) -> Result<(), String> {
        let meters = self.meters();
        for (index, meter) in meters.iter().enumerate() {
            if meters[..index].contains(meter) {
                return Err(format!(
                    "two of this check's four meters are one value ({meter}), so a subscription \
                     naming both names one and the comparison over the number of GTS types named \
                     compares a subscription with itself",
                    meter = meter.as_str(),
                ));
            }
        }

        let many = self.many.tenants();
        if many.len() != FEED_POSITION_MANY_TENANTS as usize {
            return Err(format!(
                "this check's broad meter holds entries under {actual} tenants and the comparison \
                 it anchors is about a subscription spanning {FEED_POSITION_MANY_TENANTS}",
                actual = many.len(),
            ));
        }
        let few = self.few.tenants();
        if few.len() != FEED_POSITION_FEW_TENANTS as usize {
            return Err(format!(
                "this check's narrow meter holds entries under {actual} tenants and the \
                 comparison it anchors is about a subscription spanning {FEED_POSITION_FEW_TENANTS}",
                actual = few.len(),
            ));
        }
        if self.split_tenants() != few {
            return Err(
                "this check's split pair holds entries under a different set of tenants from its \
                 narrow meter, so the comparison between them would vary how many tenants a \
                 subscription spans as well as how many GTS types it names, and a position keyed \
                 per tenant would be reported under a message about types"
                    .to_owned(),
            );
        }

        let (broad, narrow, split) = (
            self.many.entries.len(),
            self.few.entries.len(),
            self.split_entries(),
        );
        if broad != narrow || narrow != split {
            return Err(format!(
                "this check's three subscriptions hold {broad}, {narrow} and {split} entries. \
                 They have to hold the same number: a position may not grow with a \
                 subscription's breadth, and a check whose broader subscription also holds more \
                 entries cannot tell a position that grew with the breadth from one that grew \
                 with the ledger"
            ));
        }

        let entries = self.all();
        for (index, entry) in entries.iter().enumerate() {
            if entries[..index].iter().any(|other| other.id == entry.id) {
                return Err(format!(
                    "two of this check's entries derive one id ({id}); the second submission \
                     would be absorbed as a retry of the first and a subscription would hold \
                     fewer entries than this check's guards claim",
                    id = entry.id,
                ));
            }
        }
        Ok(())
    }
}

/// One position a read was issued, with what the read carried alongside it.
struct IssuedPosition {
    /// The position itself. Only [`FeedPosition::len`] is read.
    position: FeedPosition,
    /// How many entries the read delivered before it stopped. **Reported,
    /// never asserted** — see [`feed_position_bounded`]'s "The premise is
    /// the ledger, not the delivery".
    carried: usize,
    /// How many distinct tenants those entries were attributed to. Reported
    /// on the same terms.
    tenants: usize,
}

/// How the broad subscription is named in a report.
const MANY_TENANTS_READ: &str = "a subscription naming one meter whose entries span many tenants";

/// How the narrow subscription is named in a report.
const FEW_TENANTS_READ: &str = "a subscription naming one meter whose entries span few tenants";

/// How the two-type subscription is named in a report.
const TWO_TYPES_READ: &str =
    "a subscription naming two meters whose entries span those same few tenants";

/// `feed-position-bounded` — *"A position issued for a subscription spanning
/// many tenants encodes to the same size as one spanning few, so the wire
/// cursor holding it stays inside its bound (§3.3)."* (DESIGN §3.3, "Plugin
/// contract tests", line 1326.)
///
/// **That row is a pointer rather than the rule**, in the sense
/// [`feed_completeness`](super::feed_completeness()) uses of its own. The
/// rule is DESIGN §3.1's `FeedPosition` row (line 535): a position's
/// structure *"is likewise plugin-internal, but its **encoded size** is not:
/// the gateway carries it inside a length-bounded wire cursor
/// ([§3.3](#33-api-contracts)) whose size may not grow with a subscription's
/// breadth. A position keyed per tenant does not meet that bound, so a
/// plugin needs a key it can compare across a whole subscription — which key
/// that is stays its own choice."* §3.3 states the same thing from the
/// cursor's side (lines 1397-1401): *"The wire cursor is length-bounded —
/// `Cursor` in `usage-collector-v1.yaml` — and that bound covers the
/// plugin's encoded position, which may not grow with the breadth of the
/// subscription it positions … §3.10 has each plugin show how its position
/// stays inside it."* And §3.10's deployment-guide item 3 puts the
/// obligation on the plugin author (lines 2094-2097): *"how its position
/// stays inside the cursor bound however wide a subscription grows (§3.3): a
/// position keyed per tenant does not, so this is where a plugin shows the
/// key it orders by compares across a whole subscription"*.
///
/// # Two phrasings of breadth, and which governs
///
/// The row says *"spanning many tenants"*; §3.1, §3.3 and §3.10 say *"a
/// subscription's breadth"*, *"the breadth of the subscription it
/// positions"* and *"however wide a subscription grows"*. A subscription is
/// a set of GTS types ([`FeedSubscription`](crate::feed::FeedSubscription)),
/// so the three unqualified phrasings reach a second axis the row does not
/// name: a position that grows with how many **types** are subscribed.
///
/// **The row governs, and it governs the first comparison.** It is §3.3's
/// row and this is §3.3's check, so the contrast the row names — many
/// tenants against few, with everything else held equal — is the one this
/// check anchors on, and it is the comparison whose fixtures are built to
/// isolate it. The second comparison is the unqualified rule's rather than
/// the row's, and it is here because the row is an instance of that rule and
/// not a narrowing of it: a plugin author reading §3.10's obligation
/// *"however wide a subscription grows"* off a green suite would otherwise
/// be reading it off a suite in which every read named one type. Where the
/// two reach differently, the row decides what this check must assert and
/// the rule decides what it may.
///
/// # What is asserted
///
/// 1. **A subscription whose entries span many tenants is issued a position
///    that encodes to the same size as one whose entries span few.** The two
///    subscriptions name one GTS type each, hold the same number of entries
///    over one covered period, and are read under one grant at one page
///    limit, so how many tenants those entries span is the only thing that
///    differs.
/// 2. **A subscription naming two GTS types is issued a position that
///    encodes to the same size as one naming a single type.** The two hold
///    the same entries between them, under the same tenants — the pair's two
///    meters carry half each — so how many types the subscription names is
///    the only thing that differs.
///
/// Both compare [`FeedPosition::len`], which is the figure that method
/// exists for, and neither compares the positions themselves.
///
/// # The bound itself is not asserted, and cannot be
///
/// DESIGN's row ends *"so the wire cursor holding it stays inside its
/// bound"*, and this check does **not** assert that a position is at most
/// [`MAX_FEED_POSITION_BYTES`](crate::feed::MAX_FEED_POSITION_BYTES) bytes.
/// Two reasons, and the second is the stronger:
///
/// * A bound assertion would pass against exactly the defect this check
///   exists for. A position keyed per tenant over a handful of tenants is
///   nowhere near five hundred and twelve bytes, so a backend that fails the
///   rule outright would be reported as keeping it.
/// * It is **unreachable**. A [`FeedPosition`] is built only through
///   [`FeedPosition::new`](crate::feed::FeedPosition::new), which answers
///   `FeedPositionInvalid::TooLarge` above that bound, so no plugin can hand
///   this check a position to report. The argument is encoded in the type
///   rather than restated as an assertion that could never fire.
///
/// What the bound does is make the *equality* above the whole of the rule: a
/// size that does not move with breadth is a size a plugin can plan against,
/// and the number it plans against is that constant's.
///
/// # The premise is the ledger, not the delivery
///
/// *"A subscription spanning many tenants"* is a fact about the entries
/// stored under the subscription's GTS types, and this check establishes it
/// where it is made: the twenty-four entries are submitted first and any
/// refusal is reported and stops the check, and
/// [`FeedPositionFixtures::guards`] holds the tenant spans, the entry counts
/// and the identities before a single submission goes out.
///
/// **Nothing here asserts what a page carried.** The figures are collected
/// and rendered into a report, and they are rendered rather than asserted
/// because what a feed page delivers is four other checks' rule and none of
/// it is this one's. A backend whose feed delivers nothing at all still
/// issues the positions this check measures, and still breaks this rule or
/// keeps it. That the restraint also keeps this check out of
/// `Defect::AFeedBootstrapReadStartsAtTheHead`'s row is a consequence of it
/// rather than the reason for it: an entry assertion here would report that
/// subject under a message about position size, which is the wrong report
/// for a backend whose bootstrap starts at the head.
///
/// # One grant, and why it names every tenant
///
/// All three reads dispatch one grant naming all ten tenants this check
/// writes under. Holding it fixed is what keeps a difference in position
/// size attributable to the subscription: a position is obliged to denote
/// the same ledger prefix under every grant (§3.1), so a check that varied
/// the grant alongside the subscription would be varying two things at once.
/// A grant naming every tenant is the one that withholds nothing, so each
/// read's page carries the entries its own subscription holds and the
/// figures in a report describe the ledger rather than the grant.
///
/// # Each read stops at its first delivery
///
/// The reads are [`feed_walk`]s at [`WalkStop::FirstDelivery`] rather than
/// bare page reads, and the walk is what turns a live page carrying no
/// continuation into a report instead of a missing position — that shape is
/// stated once, in [`super::super::feed_walk`], and this check does not
/// restate it. `FirstDelivery` rather than `AtTheHead` because a position
/// issued anywhere is a position, and following the cursor to the head would
/// buy nothing this check reads: it never asserts that a walk finished, and
/// a conforming position's size is the same at every point of a feed.
///
/// # Surviving a repeated run
///
/// `super::super::run_all` is dispatched against one persistent backend that
/// writes entries and never removes them, and
/// `the_reference_backend_conforms_to_a_repeated_run` requires a second
/// dispatch to be as green as the first. Every identity here is keyed on its
/// meter's role and its index, so a repeated run re-submits the same
/// twenty-four identities and a conforming backend absorbs all of them.
///
/// Nothing below names a ledger length, a page count, a position value or a
/// delivered order. The two comparisons are between two byte counts the
/// backend produced in the same run, which is as true of a ledger that
/// settled on the first run as of one settling now.
///
/// # Which assertions a subject reaches, measured
///
/// Both assertions were inverted and confirmed to fire against the reference
/// backend, so neither is dead. What each *catches* was then measured by
/// neutering it and running the discrimination matrix. Two subjects reach
/// this check, both in `contract_mutants`, and **each assertion isolates one
/// of them**:
///
/// * **`Defect::AFeedPositionIsKeyedPerTenant`** — the subject DESIGN names
///   in those words — is reported by assertion 1 **alone**. Its position
///   carries one component per tenant the subscribed types hold entries
///   under, so the broad subscription's position is six components longer
///   than the narrow one's and the split pair's is exactly as long as the
///   narrow meter's. Neutering assertion 1 leaves that subject's row empty.
/// * **`Defect::AFeedPositionIsKeyedPerSubscribedType`** is reported by
///   assertion 2 **alone**, and neutering it leaves that subject's row empty
///   too. Its position carries one component per type named, so the two
///   single-type reads agree and the two-type read does not.
///
/// The second subject is the reason assertion 2 is here rather than
/// recorded as a gap: a plugin that keys its position per subscribed type
/// breaks §3.1's rule and is invisible to the row's own contrast, both of
/// whose subscriptions name one type.
///
/// # Two reports no subject reaches, and why each stays
///
/// Neutering either changed no row of the matrix, which is the measurement
/// rather than an expectation. Both are kept for the report they produce
/// rather than for a defect they catch, and both have the same shape: they
/// are what stands between a backend that cannot be measured and a check
/// that passes because nothing was measured.
///
/// * **The submission report.** No subject in the matrix refuses an entry
///   of this check: the two that refuse a withdrawal need one and this
///   check writes none, and the two that refuse an admission refuse a
///   second entry under an identity already claimed, which these
///   twenty-four are held clear of by construction and by a guard. Without
///   the report, a refused submission would leave a subscription holding
///   fewer entries than the guards above claim and the comparisons would go
///   ahead over a ledger that was never written. **Kept for the report it
///   produces**, which is the ordinary failure a plugin author needs before
///   the qualified one.
/// * **The read report.** Unreached for the same kind of reason: no subject
///   refuses a feed read over a meter nothing has swept, and all three of
///   this check's reads begin at `FeedStart::Oldest`, which every retention
///   refusal in the matrix exempts by construction. Without it a refused
///   read would leave this check reporting nothing at all, which reads as a
///   green check rather than as an unanswered one. **Kept for the report it
///   produces** — and it is the only thing this check can say about a
///   backend whose `read_feed_page` is a stub, which is what the
///   `TimescaleDB` plugin's is until its own slice 3 lands.
///
/// # What this check does not reach
///
/// It compares no two positions for **equality** and resumes from none, so
/// neither of a position's other two properties is asserted here.
/// `contract_tests`' `a_feed_position_denotes_the_same_ledger_prefix_under_every_grant`
/// holds the reference backend to the first and
/// `a_bounded_page_limit_hands_two_grants_two_positions_that_each_resume` to
/// the second; a green run **here** says nothing about either.
///
/// It varies the subscription and holds the grant fixed, so a position whose
/// size moved with the **grant** passes everything here.
/// [`feed_snapshot_and_replay`](super::feed_snapshot_and_replay())'s clause
/// four is where that comparison is made, and its own docs record that no
/// subject reaches it.
pub async fn feed_position_bounded(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    let fixtures = match feed_position_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own `{FEED_POSITION_BOUNDED}` \
                     fixtures, so nothing was submitted. This is a fault in the suite, not in \
                     the plugin under test: {detail}"
                ),
            )];
        }
    };

    // A refusal stops the check. Every comparison below is between two
    // subscriptions of different breadth, and a subscription is only as
    // broad as the entries stored under it: over a ledger that was never
    // written the two positions agree for a reason that is not the
    // plugin's, and a check that passes because nothing was stored is the
    // one outcome worse than a failing one.
    let violations = every_entry_was_accepted(plugin, &fixtures).await;
    if !violations.is_empty() {
        return violations;
    }

    let grant = feed_position_grant();
    let over_many = issued_position(
        plugin,
        &fixtures.many.subscription(),
        &grant,
        MANY_TENANTS_READ,
    )
    .await;
    let over_few = issued_position(
        plugin,
        &fixtures.few.subscription(),
        &grant,
        FEW_TENANTS_READ,
    )
    .await;
    let over_two_types = issued_position(
        plugin,
        &fixtures.split_subscription(),
        &grant,
        TWO_TYPES_READ,
    )
    .await;

    let (many, few, two_types) = match (over_many, over_few, over_two_types) {
        (Ok(many), Ok(few), Ok(two_types)) => (many, few, two_types),
        (many, few, two_types) => {
            return [many, few, two_types]
                .into_iter()
                .filter_map(Result::err)
                .collect();
        }
    };

    let mut violations = a_wider_tenant_span_does_not_grow_the_position(&many, &few);
    violations.extend(a_second_subscribed_type_does_not_grow_the_position(
        &two_types, &few,
    ));
    violations
}

/// Assertion one: the position issued for a subscription spanning many
/// tenants encodes to the same size as the one issued for a subscription
/// spanning few.
///
/// This is DESIGN's row, and the two reads differ in exactly the thing the
/// row names.
fn a_wider_tenant_span_does_not_grow_the_position(
    many: &IssuedPosition,
    few: &IssuedPosition,
) -> Vec<ContractViolation> {
    if many.position.len() == few.position.len() {
        return Vec::new();
    }
    vec![violation(
        FEED_POSITION_BOUNDED,
        format!(
            "a feed read over a subscription whose entries span \
             {FEED_POSITION_MANY_TENANTS} tenants was issued a {many_len}-byte position, and a \
             read over a subscription whose entries span {FEED_POSITION_FEW_TENANTS} a \
             {few_len}-byte one. A position issued for a subscription spanning many tenants \
             encodes to the same size as one spanning few. The two subscriptions name one GTS \
             type each and hold {FEED_POSITION_ENTRIES} entries each over one covered period, and \
             both reads ran under one grant naming every tenant either of them is attributed to, \
             at one page limit - so how many tenants those entries span is the only thing that \
             differs. The gateway carries a position inside a length-bounded wire cursor whose \
             size may not grow with a subscription's breadth, and a position keyed per tenant \
             does not meet that bound: a plugin needs a key it can compare across a whole \
             subscription, and which key that is stays its own choice. A consumer whose \
             subscription is wide runs out of cursor where a consumer whose subscription is \
             narrow does not, and the wide one is the one that is charging. The two positions are \
             deliberately not compared for equality - a position's guarantee is resumability \
             rather than identity - and only their sizes are. For context and asserted nowhere: \
             the wide read carried {many_carried} entries under {many_tenants} tenants and the \
             narrow one {few_carried} under {few_tenants}.",
            many_len = many.position.len(),
            few_len = few.position.len(),
            many_carried = many.carried,
            many_tenants = many.tenants,
            few_carried = few.carried,
            few_tenants = few.tenants,
        ),
    )]
}

/// Assertion two: naming a second GTS type does not grow the position
/// either.
///
/// The row names the tenant axis and this one is the rule's:
/// *"the breadth of the subscription it positions"* (§3.3), *"however wide a
/// subscription grows"* (§3.10). A subscription is a set of GTS types, so
/// this is that sentence read literally.
fn a_second_subscribed_type_does_not_grow_the_position(
    two_types: &IssuedPosition,
    one_type: &IssuedPosition,
) -> Vec<ContractViolation> {
    if two_types.position.len() == one_type.position.len() {
        return Vec::new();
    }
    vec![violation(
        FEED_POSITION_BOUNDED,
        format!(
            "a feed read over a subscription naming two GTS types was issued a \
             {two_len}-byte position and a read over a subscription naming one a {one_len}-byte \
             one. A position's encoded size may not grow with the breadth of the subscription it \
             positions, and a subscription is the set of GTS types one consumer reads: DESIGN \
             section 3.10 obliges a plugin to show how its position stays inside the cursor bound \
             however wide a subscription grows. The two subscriptions hold \
             {FEED_POSITION_ENTRIES} entries between them either way, under the same \
             {FEED_POSITION_FEW_TENANTS} tenants and over one covered period - the two-type \
             subscription's meters carry half of them each - so how many types are named is the \
             only thing that differs. A position carrying one component per subscribed type runs \
             a consumer out of cursor as it adds a meter, which is the growth this bound forbids \
             even where no tenant count moves. For context and asserted nowhere: the two-type \
             read carried {two_carried} entries under {two_tenants} tenants and the one-type read \
             {one_carried} under {one_tenants}.",
            two_len = two_types.position.len(),
            one_len = one_type.position.len(),
            two_carried = two_types.carried,
            two_tenants = two_types.tenants,
            one_carried = one_type.carried,
            one_tenants = one_type.tenants,
        ),
    )]
}

/// Submits this check's twenty-four entries, reporting every refusal.
///
/// Sequential rather than concurrent: nothing here is about interleaving,
/// and `feed-completeness` is the check whose row asks for it.
///
/// Every entry is submitted whatever the ones before it did, so one refusal
/// does not hide the rest — the report a plugin author reads should name the
/// whole of what was refused rather than the first of it.
///
/// A refusal is reported against the check rather than as a
/// [`HARNESS_FAULT`]: the submission is well formed, and a backend that will
/// not accept it has broken something this check cannot then go on to
/// observe.
async fn every_entry_was_accepted(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FeedPositionFixtures,
) -> Vec<ContractViolation> {
    let mut violations = Vec::new();
    for entry in fixtures.all() {
        if let Err(err) = plugin.create_usage_record(entry.clone()).await {
            violations.push(violation(
                FEED_POSITION_BOUNDED,
                format!(
                    "`create_usage_record` refused this check's entry {id} (tenant {tenant}, \
                     meter {meter}), so the ledger its comparisons read was never written: \
                     {err}. Each of those comparisons is between the positions two subscriptions \
                     of different breadth are issued, and a subscription is only as broad as the \
                     entries stored under it.",
                    id = entry.id,
                    tenant = entry.tenant_id,
                    meter = entry.gts_type_id.as_str(),
                ),
            ));
        }
    }
    violations
}

/// The position one subscription's feed issues, and what the read carried.
///
/// `role` names the subscription in a report, because a refused read over
/// the broad meter and a refused read over the narrow one are different
/// failures.
async fn issued_position(
    plugin: &dyn UsageCollectorPluginV1,
    subscription: &[MeterTypeId],
    grant: &ast::Expr,
    role: &str,
) -> Result<IssuedPosition, ContractViolation> {
    let walk = feed_walk(
        plugin,
        subscription,
        grant,
        FeedStart::Oldest,
        FEED_POSITION_PAGE_LIMIT,
        WalkStop::FirstDelivery,
    )
    .await
    .map_err(|detail| {
        violation(
            FEED_POSITION_BOUNDED,
            format!(
                "a feed read over {role} could not be taken, so this check was issued no \
                 position to measure over it and could say nothing about how a position's size \
                 moves with a subscription's breadth: {detail}"
            ),
        )
    })?;

    let tenants: BTreeSet<Uuid> = walk.delivered.iter().map(|entry| entry.tenant_id).collect();
    Ok(IssuedPosition {
        position: walk.stopped_at,
        carried: walk.delivered.len(),
        tenants: tenants.len(),
    })
}

/// The grant all three reads dispatch: `tenant_id eq <t>` over every one of
/// the ten tenants this check writes under, right-associated.
///
/// This is the shape `authz::scope_to_odata_filter` projects for a grant
/// whose constraint names several tenants, and naming all ten is what makes
/// it withhold nothing from any of the three reads.
fn feed_position_grant() -> ast::Expr {
    let rest: Vec<Uuid> = (1..FEED_POSITION_MANY_TENANTS + FEED_POSITION_FEW_TENANTS)
        .map(|index| contract_tenant(FEED_POSITION_FIRST_TENANT + index))
        .collect();
    tenant_disjunction(contract_tenant(FEED_POSITION_FIRST_TENANT), &rest)
}

/// The [`FEED_POSITION_MANY_TENANTS`] tenants the broad meter's entries are
/// attributed to.
fn many_tenants() -> Vec<Uuid> {
    (0..FEED_POSITION_MANY_TENANTS)
        .map(|index| contract_tenant(FEED_POSITION_FIRST_TENANT + index))
        .collect()
}

/// The [`FEED_POSITION_FEW_TENANTS`] tenants the narrow meter and the split
/// pair are attributed to, taken from just past the broad meter's block.
fn few_tenants() -> Vec<Uuid> {
    (0..FEED_POSITION_FEW_TENANTS)
        .map(|index| {
            contract_tenant(FEED_POSITION_FIRST_TENANT + FEED_POSITION_MANY_TENANTS + index)
        })
        .collect()
}

/// Builds the four meters and the twenty-four entries on them.
///
/// Every meter is this check's own, which is the separation that matters
/// here: a feed read selects by subscription, so an entry of this check's on
/// the suite's shared meter would be an entry some *other* check's page has
/// to account for, and an entry of some other check's on one of these meters
/// would be one this check's reads are issued a position over and cannot
/// account for itself.
///
/// The guards are [`FeedPositionFixtures::guards`]'.
fn feed_position_fixtures() -> Result<FeedPositionFixtures, String> {
    let quantity = UsageQuantity::parse(FEED_POSITION_QUANTITY).map_err(|err| {
        format!("the check's own quantity `{FEED_POSITION_QUANTITY}` does not parse: {err}")
    })?;
    let few = few_tenants();

    let fixtures = FeedPositionFixtures {
        many: stocked("many", &many_tenants(), FEED_POSITION_ENTRIES, quantity)?,
        few: stocked("few", &few, FEED_POSITION_ENTRIES, quantity)?,
        split: [
            stocked("split_first", &few, FEED_POSITION_SPLIT_HALF, quantity)?,
            stocked("split_second", &few, FEED_POSITION_SPLIT_HALF, quantity)?,
        ],
    };
    fixtures.guards()?;
    Ok(fixtures)
}

/// One meter of this check's, stocked with `count` entries dealt round-robin
/// across `tenants`.
///
/// Round-robin rather than in blocks, so a meter holding more entries than
/// tenants spans all of them whatever the count — which is the fact
/// [`FeedPositionFixtures::guards`] reads back.
fn stocked(
    role: &str,
    tenants: &[Uuid],
    count: u32,
    quantity: UsageQuantity,
) -> Result<StockedMeter, String> {
    let meter = check_meter(FEED_POSITION_BOUNDED, role)?;
    if tenants.is_empty() {
        return Err(format!(
            "the check's `{role}` meter was handed no tenant to attribute its entries to, so the \
             subscription naming it would span none and there would be no breadth to compare"
        ));
    }
    let mut entries = Vec::with_capacity(count as usize);
    for index in 0..count {
        let tenant = tenants
            .get(index as usize % tenants.len())
            .copied()
            .ok_or_else(|| {
                format!("the check's `{role}` meter could not deal entry {index} to a tenant")
            })?;
        entries.push(fixture_record_on(
            meter.clone(),
            tenant,
            &feed_position_key(role, index)?,
            quantity,
            CONTRACT_ACCEPTED_AT,
            FEED_POSITION_WINDOW_FROM,
            FEED_POSITION_WINDOW_END,
        )?);
    }
    Ok(StockedMeter { meter, entries })
}

/// The idempotency key one of this check's entries submits under.
///
/// Keyed on the meter's role and the entry's index for the reason
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture) spells
/// out: in a ledger with no delete path, an edited fixture must take a fresh
/// identity rather than inherit an accepted entry's. It is also what makes a
/// repeated run re-deliver these identities rather than mint more, which is
/// what keeps each of this check's subscriptions the same size on every run.
fn feed_position_key(role: &str, index: u32) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{FEED_POSITION_BOUNDED}-{role}-{index}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}
