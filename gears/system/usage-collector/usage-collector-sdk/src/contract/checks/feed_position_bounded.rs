//! The DESIGN §3.3 `feed-position-bounded` check.
//!
//! See [`feed_position_bounded`] for what it asserts. The module holds the
//! entries it writes, the meters and tenants it spreads them over, and the
//! one grant every read dispatches; the reads themselves are
//! [`super::super::feed_walk`]'s.
//!
//! **It is the only check in the suite whose rule is about a position's
//! encoding rather than about what a page carries**, so it asserts nothing
//! about the entries a page delivered. [`FeedPosition::len`] is the whole of
//! its subject matter.
//!
//! [`feed_snapshot_and_replay`](super::feed_snapshot_and_replay()) also
//! compares two positions by size, but across two **grants** over one
//! subscription; this check holds the grant fixed and varies the
//! **subscription**, the object DESIGN states the rule over.

use std::collections::BTreeSet;

use toolkit_odata::ast;
use uuid::Uuid;

use crate::contract::feed_walk::{WalkStop, feed_walk};
use crate::contract::fixtures::{
    CONTRACT_ACCEPTED_AT, check_meter, check_window_from, contract_tenant, fixture_record_on,
    seed_usage_record, tenant_disjunction, violation,
};
use crate::contract::{ContractViolation, FEED_POSITION_BOUNDED, HARNESS_FAULT};
use crate::feed::{FeedPosition, FeedStart};
use crate::models::IdempotencyKey;
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::stored::{MeterRef, StoredUsageRecord};

/// The start of the covered period every entry of this check carries.
///
/// The offset [`check_window_from`] tables for this check, for the reason
/// that accessor gives.
///
/// It buys as little here as for
/// [`feed_completeness`](super::feed_completeness()): **this check dispatches
/// no range at all**, so the separation that does the work is
/// [`check_meter`]. The offset is kept because the separation runs the other
/// way too — `super::super::run_all` dispatches every check against one
/// backend that never removes an entry, so a period no other check's range
/// covers is what keeps these entries out of those reads.
const FEED_POSITION_WINDOW_FROM: time::OffsetDateTime =
    check_window_from(FEED_POSITION_BOUNDED, "main");

/// The end of that covered period. Every entry carries it: they are separated
/// by meter, tenant and idempotency key, and nothing here asks a covered
/// period to tell two entries apart.
const FEED_POSITION_WINDOW_END: time::OffsetDateTime =
    FEED_POSITION_WINDOW_FROM.saturating_add(time::Duration::hours(1));

/// The quantity every entry of this check carries.
///
/// Its value is asserted nowhere — the only figures compared are byte counts
/// — but it is exactly representable in a binary float all the same, which
/// keeps `contract_mutants`'s `Defect::QuantityThroughFloat` from reaching
/// this check for a rule that belongs to `quantity-round-trip`.
const FEED_POSITION_QUANTITY: &str = "1.5";

/// How many tenants the **broad** meter's entries are attributed to — one
/// per entry.
///
/// DESIGN's row asks for *"a subscription spanning many tenants"* against
/// *"one spanning few"*, and this against [`FEED_POSITION_FEW_TENANTS`] is
/// the contrast. The figure is chosen against the shape of the defect: a
/// position keyed per tenant carries one component per tenant, so the gap has
/// to be wide enough that padding to a fixed block, or a variable-width
/// length prefix, cannot mask it. Going wider only lengthens a report, and
/// has a ceiling that is not this check's to set — a subject modelling a
/// per-tenant key has to stay inside
/// [`MAX_FEED_POSITION_BYTES`](crate::feed::MAX_FEED_POSITION_BYTES) to be a
/// subject about size rather than about refusal.
const FEED_POSITION_MANY_TENANTS: u32 = 8;

/// How many tenants the **narrow** meter's entries are attributed to.
///
/// The row says *"few"* rather than *"one"*, and more than one is what keeps
/// the narrow side an ordinary subscription: a single-tenant subscription is
/// the one case a plugin might reasonably special-case, since a per-tenant
/// key degenerates to a single component there. Two is the fewest that is not
/// that case.
const FEED_POSITION_FEW_TENANTS: u32 = 2;

/// How many entries each subscription this check reads holds.
///
/// Equal on every read, and that equality is what makes the comparison about
/// breadth: a position growing with the *length* of the ledger rather than
/// the breadth of the subscription is a different defect.
///
/// It is [`FEED_POSITION_MANY_TENANTS`] rather than a figure of its own
/// because the broad meter takes one entry per tenant, the simplest ledger
/// spanning that many.
const FEED_POSITION_ENTRIES: u32 = FEED_POSITION_MANY_TENANTS;

/// How many of those entries each half of the split pair holds.
///
/// Spelled out rather than divided, with the relation to
/// [`FEED_POSITION_ENTRIES`] asserted at compile time below: the halves have
/// to add up to exactly the narrow meter's count, or the second comparison
/// varies the ledger's length alongside the number of GTS types named.
const FEED_POSITION_SPLIT_HALF: u32 = 4;

/// The first index this check takes from [`contract_tenant`]'s block.
///
/// [`latest_tie_break`](super::latest_tie_break()) and
/// [`feed_snapshot_and_replay`](super::feed_snapshot_and_replay()) hold the
/// indices below it, so this check takes its block from 4 upwards: the broad
/// meter's [`FEED_POSITION_MANY_TENANTS`] and then the narrow side's
/// [`FEED_POSITION_FEW_TENANTS`].
///
/// The block is held clear of the suite's named tenant ids by a compile-time
/// assertion in [`super::super::fixtures`]. Sharing an index with another
/// check would be harmless — what a tenant owns is owned on a meter, and
/// every meter here is [`check_meter`]'s — but a block of this check's own
/// keeps the grant below readable.
const FEED_POSITION_FIRST_TENANT: u32 = 4;

/// The page limit every read this check dispatches: twice the entries any one
/// of them meets.
///
/// The margin is not an optimisation. A short page is conforming — `limit`
/// bounds what a page *carries* and promises nothing about everything settled
/// fitting on one — so this check follows the cursor, and the margin is what
/// makes the ordinary case a single page all the same.
///
/// **The same limit is dispatched by every read this check compares.** A page
/// limit changes where a page boundary falls, so two reads at two limits
/// could be issued positions differing for a reason about the limit rather
/// than the subscription.
const FEED_POSITION_PAGE_LIMIT: u64 = 16;

// What the constants above have to satisfy for the comparisons below to be
// about what they say. Established at compile time, because each is a
// property of these literals alone and a runtime guard would report the
// suite's own arithmetic to a plugin author as a harness fault.
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
    meter: MeterRef,
    /// The entries written to it, in submission order.
    entries: Vec<StoredUsageRecord>,
}

impl StockedMeter {
    /// The distinct tenants its entries are attributed to.
    fn tenants(&self) -> BTreeSet<Uuid> {
        self.entries.iter().map(|entry| entry.tenant_id).collect()
    }

    /// A subscription naming this meter alone.
    fn subscription(&self) -> Vec<MeterRef> {
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
    /// A subscription naming both is one type wider than one naming
    /// [`Self::few`] and identical in every other way this check can hold
    /// fixed — same entry count, same tenants, one covered period — which is
    /// what makes the second comparison about the type count alone.
    split: [StockedMeter; 2],
}

impl FeedPositionFixtures {
    /// Every entry this check submits, in submission order.
    fn all(&self) -> Vec<(&MeterRef, &StoredUsageRecord)> {
        let mut entries: Vec<(&MeterRef, &StoredUsageRecord)> = self
            .many
            .entries
            .iter()
            .map(|entry| (&self.many.meter, entry))
            .collect();
        entries.extend(
            self.few
                .entries
                .iter()
                .map(|entry| (&self.few.meter, entry)),
        );
        for half in &self.split {
            entries.extend(half.entries.iter().map(|entry| (&half.meter, entry)));
        }
        entries
    }

    /// The subscription naming both halves of the split pair.
    fn split_subscription(&self) -> Vec<MeterRef> {
        self.split.iter().map(|half| half.meter.clone()).collect()
    }

    /// The four meters, in the order they are described.
    fn meters(&self) -> Vec<&MeterRef> {
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

    /// The facts this check's two comparisons read, established rather than
    /// assumed. All are the suite's own arithmetic rather than the plugin's,
    /// so all are reported as [`HARNESS_FAULT`].
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
    /// * **Every subscription holds the same number of entries.** A
    ///   position that grew with the ledger's length rather than with the
    ///   subscription's breadth is a different defect, and an unequal count
    ///   is what would let this check report one as the other.
    /// * **Every entry derives a distinct id.** It does — meter, tenant and
    ///   idempotency key are all identity inputs — but a collision would have
    ///   one submission absorbed as a retry of the other, leaving a
    ///   subscription smaller than the count above claims.
    fn guards(&self) -> Result<(), String> {
        let meters = self.meters();
        for (index, meter) in meters.iter().enumerate() {
            if meters[..index].contains(meter) {
                return Err(format!(
                    "two of this check's four meters are one value ({meter}), so a subscription \
                     naming both names one and the comparison over the number of GTS types named \
                     compares a subscription with itself",
                    meter = meter.id.as_str(),
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
        for (index, (_, entry)) in entries.iter().enumerate() {
            if entries[..index]
                .iter()
                .any(|(_, other)| other.id == entry.id)
            {
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
/// contract tests".)
///
/// **That row is a pointer rather than the rule**, in the sense
/// [`feed_completeness`](super::feed_completeness()) uses of its own. The
/// rule is DESIGN §3.1's `FeedPosition` row: a position's structure *"is
/// likewise plugin-internal, but its **encoded size** is not: the gateway
/// carries it inside a length-bounded wire cursor
/// ([§3.3](#33-api-contracts)) whose size may not grow with a subscription's
/// breadth. A position keyed per tenant does not meet that bound, so a
/// plugin needs a key it can compare across a whole subscription — which key
/// that is stays its own choice."* §3.3 states the same from the cursor's
/// side: *"The wire cursor is length-bounded — `Cursor` in
/// `usage-collector-v1.yaml` — and that bound covers the plugin's encoded
/// position, which may not grow with the breadth of the subscription it
/// positions … §3.10 has each plugin show how its position stays inside
/// it."* §3.10's deployment-guide item 3 puts the obligation on the plugin
/// author.
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
/// **The row governs, and it governs the first comparison** — many tenants
/// against few, everything else held equal. The second comparison is the
/// unqualified rule's, and it is here because the row is an instance of that
/// rule rather than a narrowing of it: a plugin author reading §3.10's
/// *"however wide a subscription grows"* off a green suite would otherwise be
/// reading it off a suite in which every read named one type.
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
///   exists for: a position keyed per tenant over a handful of tenants is
///   nowhere near the bound, so a backend failing the rule outright would be
///   reported as keeping it.
/// * It is **unreachable**. A [`FeedPosition`] is built only through
///   [`FeedPosition::new`](crate::feed::FeedPosition::new), which answers
///   `FeedPositionInvalid::TooLarge` above that bound, so no plugin can hand
///   this check a position to report.
///
/// What the bound does is make the *equality* above the whole of the rule: a
/// size that does not move with breadth is a size a plugin can plan against,
/// and the number it plans against is that constant's.
///
/// # The premise is the ledger, not the delivery
///
/// *"A subscription spanning many tenants"* is a fact about the entries
/// stored under the subscription's GTS types, so the entries are submitted
/// first and any refusal is reported and stops the check, with
/// [`FeedPositionFixtures::guards`] holding the tenant spans, entry counts
/// and identities before a single submission goes out.
///
/// **Nothing here asserts what a page carried.** What a feed page delivers is
/// other checks' rule, and a backend whose feed delivers nothing at all still
/// issues the positions this check measures. The restraint also keeps this
/// check out of `Defect::AFeedBootstrapReadStartsAtTheHead`'s row, where an
/// entry assertion would report that subject under a message about position
/// size.
///
/// # One grant, and why it names every tenant
///
/// Every read dispatches one grant naming every tenant this check writes
/// under. Holding it fixed is what keeps a difference in position size
/// attributable to the subscription: a position denotes the same ledger
/// prefix under every grant (§3.1), so varying the grant alongside the
/// subscription would vary two things at once. A grant naming every tenant
/// withholds nothing, so the figures in a report describe the ledger.
///
/// # Each read stops at its first delivery
///
/// The reads are [`feed_walk`]s at [`WalkStop::FirstDelivery`] rather than
/// bare page reads; see [`super::super::feed_walk`] for what the walk buys.
/// `FirstDelivery` rather than `AtTheHead` because a position issued anywhere
/// is a position: this check never asserts that a walk finished, and a
/// conforming position's size is the same at every point of a feed.
///
/// # Surviving a repeated run
///
/// `super::super::run_all` is dispatched against one persistent backend that
/// never removes entries, and `the_reference_backend_conforms_to_a_repeated_run`
/// requires a second dispatch to be as green as the first. Every identity here
/// is keyed on its meter's role and its index, so a repeated run re-submits
/// the same identities and a conforming backend absorbs them.
///
/// Nothing below names a ledger length, a page count, a position value or a
/// delivered order: the comparisons are between two byte counts the backend
/// produced in the same run.
///
/// # Which assertions a subject reaches, measured
///
/// Both assertions were inverted and confirmed to fire against the reference
/// backend, so neither is dead. What each *catches* was measured by neutering
/// it and running the discrimination matrix; two subjects reach this check,
/// both in `contract_mutants`, and **each assertion isolates one**:
///
/// * **`Defect::AFeedPositionIsKeyedPerTenant`** — the subject DESIGN names
///   in those words — is reported by assertion 1 **alone**. Its position
///   carries one component per tenant the subscribed types hold entries
///   under, so the broad subscription's position is longer than the narrow
///   one's while the split pair's matches the narrow meter's.
/// * **`Defect::AFeedPositionIsKeyedPerSubscribedType`** is reported by
///   assertion 2 **alone**. Its position carries one component per type
///   named, so the two single-type reads agree and the two-type read does
///   not. It is why assertion 2 is here rather than recorded as a gap: that
///   defect is invisible to the row's own contrast, both of whose
///   subscriptions name one type.
///
/// # Two reports no subject reaches, and why each stays
///
/// Neutering either changed no row of the matrix. Both are kept for the
/// report they produce rather than a defect they catch: they are what stands
/// between a backend that cannot be measured and a check that passes because
/// nothing was measured.
///
/// * **The submission report.** No subject in the matrix refuses an entry of
///   this check. Without it, a refused submission would leave a subscription
///   holding fewer entries than the guards claim and the comparisons would go
///   ahead over a ledger that was never written.
/// * **The read report.** Unreached for the same kind of reason: no subject
///   refuses a feed read over a meter nothing has swept, and every read here
///   begins at `FeedStart::Oldest`, which every retention refusal in the
///   matrix exempts. Without it a refused read would leave this check
///   reporting nothing, which reads as green rather than unanswered — and it
///   is the only thing this check can say about a backend whose
///   `read_feed_page` is a stub.
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

    // A refusal stops the check: a subscription is only as broad as the
    // entries stored under it, so over a ledger that was never written the
    // two positions agree for a reason that is not the plugin's.
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
/// DESIGN's row, with the two reads differing in exactly what it names.
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
/// The row names the tenant axis; this is the rule's other one — *"the
/// breadth of the subscription it positions"* (§3.3) — read literally, since
/// a subscription is a set of GTS types.
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

/// Submits this check's entries, reporting every refusal.
///
/// Sequential rather than concurrent: nothing here is about interleaving, and
/// `feed-completeness` is the check whose row asks for it. Every entry is
/// submitted whatever the ones before it did, so one refusal does not hide
/// the rest.
///
/// A refusal is reported against the check rather than as a
/// [`HARNESS_FAULT`]: the submission is well formed, and a backend that will
/// not accept it has broken something this check cannot then observe.
async fn every_entry_was_accepted(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FeedPositionFixtures,
) -> Vec<ContractViolation> {
    let mut violations = Vec::new();
    for (meter, entry) in fixtures.all() {
        if let Err(err) = seed_usage_record(plugin, meter, entry.clone()).await {
            violations.push(violation(
                FEED_POSITION_BOUNDED,
                format!(
                    "`create_usage_records` refused this check's entry {id} (tenant {tenant}, \
                     meter {meter}), so the ledger its comparisons read was never written: \
                     {err}. Each of those comparisons is between the positions two subscriptions \
                     of different breadth are issued, and a subscription is only as broad as the \
                     entries stored under it.",
                    id = entry.id,
                    tenant = entry.tenant_id,
                    meter = meter.id.as_str(),
                ),
            ));
        }
    }
    violations
}

/// The position one subscription's feed issues, and what the read carried.
///
/// `role` names the subscription in a report: a refused read over the broad
/// meter and one over the narrow meter are different failures.
async fn issued_position(
    plugin: &dyn UsageCollectorPluginV1,
    subscription: &[MeterRef],
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

/// The grant every read dispatches: `tenant_id eq <t>` over every tenant this
/// check writes under, right-associated.
///
/// The shape `authz::scope_to_odata_filter` projects for a grant whose
/// constraint names several tenants; naming them all is what makes it
/// withhold nothing from any read.
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

/// Builds this check's meters and the entries on them.
///
/// Every meter is this check's own, which is the separation that matters: a
/// feed read selects by subscription, so an entry of this check's on the
/// shared meter would land in another check's page, and another check's entry
/// on one of these meters would be positioned over here and unaccounted for.
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
/// tenants spans all of them whatever the count — the fact
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
            &meter,
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
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture) gives:
/// in a ledger with no delete path, an edited fixture must take a fresh
/// identity. It also makes a repeated run re-deliver these identities rather
/// than mint more, keeping each subscription the same size on every run.
fn feed_position_key(role: &str, index: u32) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{FEED_POSITION_BOUNDED}-{role}-{index}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}
