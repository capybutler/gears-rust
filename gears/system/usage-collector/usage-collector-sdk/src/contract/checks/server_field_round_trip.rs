//! The DESIGN §3.3 `server-field-round-trip` check.
//!
//! See [`server_field_round_trip`] for what it asserts; the module holds the
//! three entries it stores, the divergent retry it absorbs, and the
//! field-by-field comparison that exists because the suite's shared one
//! cannot see three of the four fields in question.

use crate::contract::fixtures::{
    CONTRACT_ACCEPTED_AT, CONTRACT_TENANT_ID, FIXTURE_EPOCH, check_meter, contract_query,
    contract_scope, fixture_invalidation_on, fixture_record_on, violation,
};
use crate::contract::{ContractViolation, HARNESS_FAULT, SERVER_FIELD_ROUND_TRIP};
use crate::feed::{FeedPosition, FeedStart};
use crate::models::{IdempotencyKey, MeterTypeId, RecordOrigin, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::time_range::TimeRange;

/// The start of the covered period every entry of this check carries, and
/// the inclusive lower bound of the range it reads them back over.
///
/// Two hundred and ten days past [`FIXTURE_EPOCH`], for the reason
/// [`WINDOW_SELECTION_FROM`](super::window_end_selection::WINDOW_SELECTION_FROM)
/// gives, and clear of the day-0, 30, 60, 90, 120, 150 and 180 offsets the
/// other checks take. It buys something narrower here than elsewhere: this
/// check looks its entries up by `id` and never counts the rows a range
/// returns, so a stray entry is not miscounted — but it would take a slot on
/// a bounded page, and enough of them would truncate one of this check's own
/// entries away and have it reported absent
/// ([`SERVER_FIELD_PAGE_LIMIT`] carries the other half of that argument).
const SERVER_FIELD_WINDOW_FROM: time::OffsetDateTime =
    FIXTURE_EPOCH.saturating_add(time::Duration::days(210));

/// The end of that covered period. Every entry of this check carries it: the
/// entries are separated by their idempotency keys and their entry type, and
/// nothing here asks a range to tell two of them apart.
const SERVER_FIELD_WINDOW_END: time::OffsetDateTime =
    SERVER_FIELD_WINDOW_FROM.saturating_add(time::Duration::hours(1));

/// The exclusive upper bound of the range this check reads over, an hour past
/// [`SERVER_FIELD_WINDOW_END`].
///
/// The range holds **both** covered-period bounds, deliberately. This check
/// asserts nothing about period selection, and a range that began at
/// [`SERVER_FIELD_WINDOW_END`] would make the list half unanswerable for a
/// backend selecting on `window_start` — coupling this check to
/// `window-end-selection`'s rule for no reason of its own.
const SERVER_FIELD_WINDOW_TO: time::OffsetDateTime =
    SERVER_FIELD_WINDOW_END.saturating_add(time::Duration::hours(1));

/// The quantity every entry of this check carries.
///
/// Its value is asserted nowhere: the comparison here reads the four
/// server-assigned fields and nothing else. It is exactly representable in a
/// binary float all the same, which keeps the one other subject that rewrites
/// a field on the way in — `contract_mutants`'s `Defect::QuantityThroughFloat`
/// — from changing anything this check can see, and in particular from
/// turning the absorbed retry below into an `IdempotencyConflict` for a
/// reason that is not this check's rule.
const SERVER_FIELD_QUANTITY: &str = "5.5";

/// The reason the withdrawal this check submits states.
///
/// Its own rather than the shared one, because [`fixture_invalidation_on`] is
/// the builder that leaves the acceptance instant open and a reason code is
/// the other thing it asks for. No check reads it; the vocabulary is open.
const SERVER_FIELD_REASON_CODE: &str = "server-field-round-trip-withdrawal";

/// The acceptance instant the retried record is stamped with.
///
/// Four instants, one per submission, all distinct and all distinct from
/// [`CONTRACT_ACCEPTED_AT`]. The distinctness is the assertion: a backend that
/// answered one stored entry's instant for another's, or the suite's shared
/// constant for all of them, is caught only where the instants differ.
const SERVER_FIELD_RECORD_ACCEPTED_AT: time::OffsetDateTime =
    CONTRACT_ACCEPTED_AT.saturating_add(time::Duration::hours(1));

/// The acceptance instant of the record the withdrawal below names.
const SERVER_FIELD_WITHDRAWN_ACCEPTED_AT: time::OffsetDateTime =
    CONTRACT_ACCEPTED_AT.saturating_add(time::Duration::hours(2));

/// The acceptance instant of the withdrawal itself.
///
/// Distinct from its target's on purpose. An invalidation repeats every
/// caller-supplied field of the entry it withdraws (DESIGN §3.1, Faithful
/// copy), and `accepted_at` is not one of them: it is stamped for the
/// withdrawal's own request, so a backend copying the target's instant onto it
/// is wrong and nothing but two different instants can say so.
const SERVER_FIELD_INVALIDATION_ACCEPTED_AT: time::OffsetDateTime =
    CONTRACT_ACCEPTED_AT.saturating_add(time::Duration::hours(3));

/// The acceptance instant the **retry** carries, which is not the one the
/// stored entry carries.
///
/// This is the divergence the fourth assertion turns on, and it is a
/// well-formed submission rather than a malformed one: `accepted_at` is
/// server-assigned and stamped once per request, so a gateway that re-stamped
/// before dispatching a re-delivery produces exactly this. The plugin answers
/// with what it stored.
const SERVER_FIELD_RETRY_ACCEPTED_AT: time::OffsetDateTime =
    CONTRACT_ACCEPTED_AT.saturating_add(time::Duration::hours(4));

/// The read limit this check dispatches, on the ledger page and on each feed
/// page alike: twice the three entries it expects.
///
/// The margin is the assertion, for the reason
/// [`DEDUP_PAGE_LIMIT`](super::dedup_identity_over_window::DEDUP_PAGE_LIMIT)
/// gives, read here against what this check does with a page. A limit set to
/// the expected three would let a page carrying an unexpected row truncate an
/// expected one away, and this check would then report a plugin for losing an
/// entry it had in fact answered with — a violation attributed to the plugin
/// for the suite's own choice of limit.
const SERVER_FIELD_PAGE_LIMIT: u64 = 6;

/// How many feed pages this check will follow before it gives up.
///
/// The feed is **followed** rather than read once, because a short page is
/// conforming: `limit` bounds what a page carries and promises nothing about
/// everything settled fitting on one, and DESIGN's `feed-retention-refusal`
/// row speaks of a cursor *"refused rather than served as a short page"*, so
/// short pages are a shape the contract knows. A single read would report a
/// backend that pages in small steps as having lost entries.
///
/// The loop's real exit is the cursor standing still: a page whose `next`
/// repeats the position the read began at has delivered everything after it.
/// This budget bounds the **loop**, not the read. A backend whose cursor
/// advances forever without ever reaching its own head is a defect belonging
/// to the feed checks, and this one declines to hang waiting for it.
const SERVER_FIELD_FEED_PAGES: usize = 256;

/// The three entries this check stores, the retry it sends afterwards, and
/// the meter they are read back over.
struct ServerFieldFixtures {
    /// The meter all three entries are written to and the only meter this
    /// check subscribes to.
    meter: MeterTypeId,
    /// The ordinary measurement the divergent retry below re-delivers.
    ///
    /// Not the entry the withdrawal names, and that separation is
    /// load-bearing: a retry of a *withdrawn* record is the one assertion
    /// `record-and-invalidation-distinct-identity` uniquely makes, and
    /// repeating it here would make this check fail against that check's own
    /// subject for a rule that is not this one's.
    record: UsageRecord,
    /// The measurement the withdrawal names. Its only job is to give
    /// [`Self::invalidation`] a target.
    withdrawn: UsageRecord,
    /// The withdrawal of [`Self::withdrawn`], and the only entry in the
    /// fixture set carrying an `invalidates` for this check to read. A plain
    /// record carries none.
    invalidation: UsageRecord,
    /// The re-delivery of [`Self::record`]: the same six identity inputs and
    /// the same caller-supplied fields, carrying a different `accepted_at`
    /// and a different `origin`.
    ///
    /// **This check never counts the rows its meter holds, and where an
    /// assertion that did were placed would decide which subjects it fails.**
    /// This submission is a re-delivery, so a backend with no unique
    /// constraint on the dedup identity — `contract_mutants`'s
    /// `Defect::LedgerHasNoUniqueConstraint` — stores it as a second row,
    /// carrying this entry's `accepted_at` and `origin`, which is exactly
    /// what the assertions below require a read *not* to answer with.
    ///
    /// Two facts keep this check out of that subject's matrix row, and both
    /// were measured by adding a row count and reading what the matrix
    /// reported. The first is **ordering in time**: the duplicate does not
    /// exist until [`the_absorbed_retry_answers_with_the_stored_values`]
    /// runs, and only one of this check's five properties runs after it. A
    /// count placed among the first four reports nothing; the same count
    /// placed after the retry puts this check into that subject's row.
    ///
    /// The second is **ordering in the answer**, and it holds only for that
    /// fifth property: it looks its entry up by `id` through
    /// `get_usage_record`, which answers the first matching row, and the
    /// reference ledger is in admission order, so the original answers and
    /// the duplicate behind it is never seen. That is an ordering dependency
    /// rather than a structural one. An assertion added there that counted
    /// rows, or a backend whose point read answered the later row, would join
    /// that subject's row — and either would look like an accident unless
    /// this note is read first.
    retry: UsageRecord,
}

impl ServerFieldFixtures {
    /// The three entries this check stores, each under the name its reports
    /// use. The retry is not among them: it is absorbed rather than stored.
    fn stored(&self) -> [(&'static str, &UsageRecord); 3] {
        [
            ("retried", &self.record),
            ("withdrawn", &self.withdrawn),
            ("invalidation", &self.invalidation),
        ]
    }
}

/// `server-field-round-trip` — *"`id`, `accepted_at`, `origin`, and
/// `invalidates` read back on every path exactly as they were handed to the
/// plugin, including through an absorbed idempotent retry, which returns the
/// stored entry's values rather than the retry's."*
///
/// DESIGN §3.1's "Field ownership" table puts those four in the
/// **server-assigned** group — *"`id`, `accepted_at`, `origin`, and
/// `invalidates` on an invalidation"*, set by *"the Ingestion Gateway, at the
/// single choke point"* — and its "Server-assigned field fidelity" invariant
/// states the plugin's obligation over them: *"A plugin persists the values it
/// was handed on the entry it stores, and every read path returns those. It
/// does not re-derive, default, or refresh one — `accepted_at` in particular
/// is not the store's own insert time."*
///
/// **This is the one check in the suite whose rule
/// [`UsageRecord::caller_supplied_eq`] cannot express**, and the reason is
/// exactly why the rule needs a check of its own: that comparison destructures `id`,
/// `accepted_at` and `origin` as `_`, deliberately, because a caller cannot
/// forge them and a retry carrying different ones is still the same
/// submission. Three of the four fields here are invisible to it. The
/// comparison this check makes is [`server_assigned_eq`], which names all
/// four.
///
/// Five properties are asserted, each reported on its own:
///
/// 1. **The point lookup** returns all four fields as handed, on each of the
///    three stored entries.
/// 2. **The list path** does, over a range holding all three.
/// 3. **The feed page** does. No other check in the suite reads these fields
///    on the feed, and the feed is one of the five read paths the row's "on
///    every path" covers.
/// 4. **An absorbed retry** answers with the **stored** entry's values. The
///    retry carries a different `accepted_at` and a different `origin`, which
///    is a well-formed re-delivery rather than a conflict: both are
///    server-assigned and stamped per request, so a gateway that re-stamped
///    before dispatching a retry produces exactly this, and
///    [`UsageRecord::caller_supplied_eq`] — the comparison a collision is
///    resolved by — does not read either. The SPI says the same in
///    [`create_usage_record`](UsageCollectorPluginV1::create_usage_record):
///    *"An equal submission is absorbed and the **stored** entry is returned,
///    its `accepted_at` and `origin` included."*
/// 5. **After that retry, the stored entry is unchanged.** A backend that
///    answered the retry correctly and then overwrote the row satisfies the
///    fourth property and fails this one; nothing else in the suite reads the
///    row again afterwards.
///
/// The two read paths this check does not reach answer no entry at all, so
/// there is nothing on either for these four fields to read back as. The fold
/// answers buckets of quantities; the reconciliation read answers counters
/// and two watermarks, and its `max_accepted_at` is a fold over a scope's
/// acceptance instants rather than one entry's own field.
pub async fn server_field_round_trip(
    plugin: &dyn UsageCollectorPluginV1,
) -> Vec<ContractViolation> {
    let fixtures = match server_field_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own `{SERVER_FIELD_ROUND_TRIP}` \
                     fixtures, so nothing was submitted. This is a fault in the suite, not in \
                     the plugin under test: {detail}"
                ),
            )];
        }
    };

    // A refusal stops the check. The three entries are one scenario: every
    // property below is a statement about what a stored entry reads back as,
    // and an entry the backend never admitted has nothing to read back.
    for (role, record) in fixtures.stored() {
        if let Err(err) = plugin.create_usage_record(record.clone()).await {
            return vec![violation(
                SERVER_FIELD_ROUND_TRIP,
                format!(
                    "`create_usage_record` refused the {role} entry (record {id}), so no read \
                     path could be asked what it stored for the four server-assigned fields: \
                     {err}",
                    id = record.id,
                ),
            )];
        }
    }

    let mut violations = the_point_lookup_answers_with_them(plugin, &fixtures).await;
    violations.extend(the_list_path_answers_with_them(plugin, &fixtures).await);
    violations.extend(the_feed_answers_with_them(plugin, &fixtures).await);
    violations.extend(the_absorbed_retry_answers_with_the_stored_values(plugin, &fixtures).await);
    violations.extend(the_retry_leaves_the_stored_entry_alone(plugin, &fixtures).await);
    violations
}

/// Property one: the point lookup answers each stored entry with the four
/// fields it was handed.
async fn the_point_lookup_answers_with_them(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &ServerFieldFixtures,
) -> Vec<ContractViolation> {
    let scope = contract_scope();
    let mut violations = Vec::new();
    for (role, expected) in fixtures.stored() {
        match plugin.get_usage_record(expected.id, &scope, false).await {
            Ok(observed) if server_assigned_eq(&observed, expected) => {}
            Ok(observed) => {
                violations.push(mismatch("get_usage_record", role, &observed, expected));
            }
            Err(err) => violations.push(violation(
                SERVER_FIELD_ROUND_TRIP,
                format!(
                    "`get_usage_record` refused the {role} entry ({id}) under a scope pinning \
                     its own tenant, moments after accepting it: {err}. A stored entry has to be \
                     readable before anything can be said about the values it reads back with.",
                    id = expected.id,
                ),
            )),
        }
    }
    violations
}

/// Property two: the ledger page answers each stored entry with the four
/// fields it was handed.
async fn the_list_path_answers_with_them(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &ServerFieldFixtures,
) -> Vec<ContractViolation> {
    match server_field_page(plugin, &fixtures.meter).await {
        Ok(items) => the_page_answers_with_them(&items, fixtures, "list_usage_records"),
        Err(detail) => vec![violation(SERVER_FIELD_ROUND_TRIP, detail)],
    }
}

/// Property three: the feed answers each stored entry with the four fields it
/// was handed.
///
/// Nothing else in the suite reads an entry back off the feed, so this is
/// the only assertion that path carries today. It is also a path a charging
/// consumer reads *instead of* `list_usage_records`, so an entry whose
/// acceptance instant the feed re-derives is one a consumer rates against
/// the wrong instant.
async fn the_feed_answers_with_them(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &ServerFieldFixtures,
) -> Vec<ContractViolation> {
    match server_field_feed(plugin, &fixtures.meter).await {
        Ok(entries) => the_page_answers_with_them(&entries, fixtures, "read_feed_page"),
        Err(detail) => vec![violation(SERVER_FIELD_ROUND_TRIP, detail)],
    }
}

/// Every stored entry is among `delivered`, carrying the four fields it was
/// handed.
///
/// **`id` is how an entry is found here**, so a collection path that answered
/// a stored entry under a different `id` reports as an absent entry rather
/// than as a field mismatch. The report says so: for the two collection paths
/// "this entry is not on the page" and "this entry came back under an id it
/// was not given" are the same observation.
fn the_page_answers_with_them(
    delivered: &[UsageRecord],
    fixtures: &ServerFieldFixtures,
    path: &str,
) -> Vec<ContractViolation> {
    let mut violations = Vec::new();
    for (role, expected) in fixtures.stored() {
        match delivered.iter().find(|entry| entry.id == expected.id) {
            Some(observed) if server_assigned_eq(observed, expected) => {}
            Some(observed) => violations.push(mismatch(path, role, observed, expected)),
            None => violations.push(violation(
                SERVER_FIELD_ROUND_TRIP,
                format!(
                    "`{path}` was asked for this check's own meter and delivered {count} \
                     entries, none of them the {role} entry ({id}). `id` is what an entry is \
                     found by on a \
                     collection path, so an entry answered under an id it was never handed is \
                     indistinguishable from an entry withheld - and the derived identity is \
                     stamped once by the gateway from the six dedup-identity inputs, never \
                     re-derived by the store.",
                    count = delivered.len(),
                    id = expected.id,
                ),
            )),
        }
    }
    violations
}

/// Property four: re-delivering the record under a different `accepted_at`
/// and a different `origin` is absorbed, and answers with the stored values.
///
/// The comparison is against the **record**, not against the retry. That is
/// the whole of the property: the two differ in exactly the two fields a
/// backend is most likely to answer from the submission in hand rather than
/// from the row it holds.
async fn the_absorbed_retry_answers_with_the_stored_values(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &ServerFieldFixtures,
) -> Vec<ContractViolation> {
    match plugin.create_usage_record(fixtures.retry.clone()).await {
        Ok(observed) if server_assigned_eq(&observed, &fixtures.record) => Vec::new(),
        Ok(observed) => vec![violation(
            SERVER_FIELD_ROUND_TRIP,
            format!(
                "record {id} was re-delivered under a different acceptance instant and a \
                 different origin ({sent}), and the absorb answered {observed}. The stored \
                 entry's own values are {expected}, and those are what an absorbed retry \
                 returns: `accepted_at` and `origin` are server-assigned and stamped per \
                 request, so a gateway that re-stamped before dispatching a re-delivery \
                 produces exactly this submission - it is well-formed rather than a conflict, \
                 and neither field is compared when a collision is resolved. Answering the \
                 retry's own values is how a caller loses the one thing an absorb tells it: \
                 which write was first.",
                id = fixtures.record.id,
                sent = server_assigned_fields(&fixtures.retry),
                observed = server_assigned_fields(&observed),
                expected = server_assigned_fields(&fixtures.record),
            ),
        )],
        Err(err) => vec![violation(
            SERVER_FIELD_ROUND_TRIP,
            format!(
                "record {id} was re-delivered verbatim but for its acceptance instant and its \
                 origin, and `create_usage_record` refused it: {err}. Both fields are \
                 server-assigned, neither is an input to the derived identity, and neither is \
                 compared when a collision on that identity is resolved - so this is an \
                 idempotent replay to be absorbed, not a divergent submission to be refused.",
                id = fixtures.record.id,
            ),
        )],
    }
}

/// Property five: the stored entry still carries its own values after the
/// retry was absorbed.
///
/// Separate from property four because a backend can pass that one and fail
/// this: answering the caller from the row it holds and *then* writing the
/// retry's `accepted_at` over it — an `ON CONFLICT … DO UPDATE` that
/// refreshes the column — returns the right answer once and holds the wrong
/// row forever. No other check in the suite reads a row's content back after
/// retrying it, so nothing else would notice.
async fn the_retry_leaves_the_stored_entry_alone(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &ServerFieldFixtures,
) -> Vec<ContractViolation> {
    let scope = contract_scope();
    match plugin
        .get_usage_record(fixtures.record.id, &scope, false)
        .await
    {
        Ok(observed) if server_assigned_eq(&observed, &fixtures.record) => Vec::new(),
        Ok(observed) => vec![violation(
            SERVER_FIELD_ROUND_TRIP,
            format!(
                "record {id} read back as {observed} after a retry carrying {sent} had been \
                 absorbed, and it was stored as {expected}. An absorbed retry writes nothing: \
                 the identity's first write is the survivor and its server-assigned values are \
                 the entry's for as long as it is retained. A backend that answered the retry \
                 from the stored row and then refreshed that row with the retry's values passes \
                 every other assertion this check makes, all of which run before the retry.",
                id = fixtures.record.id,
                observed = server_assigned_fields(&observed),
                sent = server_assigned_fields(&fixtures.retry),
                expected = server_assigned_fields(&fixtures.record),
            ),
        )],
        Err(err) => vec![violation(
            SERVER_FIELD_ROUND_TRIP,
            format!(
                "`get_usage_record` refused record {id} after a retry of it had been absorbed: \
                 {err}. The entry was accepted and the retry of it was not a write, so an absorb \
                 has removed or hidden a stored entry - which no re-delivery may do.",
                id = fixtures.record.id,
            ),
        )],
    }
}

/// Whether two entries agree on the four fields DESIGN §3.1 assigns to the
/// server.
///
/// **This exists because [`UsageRecord::caller_supplied_eq`] cannot serve
/// here.** That is the dedup comparison, and it destructures `id`,
/// `accepted_at` and `origin` as `_` by design: they are server-assigned, a
/// caller cannot forge them, and a re-delivery carrying different ones is
/// still the same submission. Three of the four fields this check is about
/// are invisible to it, so an assertion routed through it would test one
/// field and silently skip three.
///
/// The fourth is read through the grouped
/// [`Invalidation`](crate::models::Invalidation) field, and only its `target`
/// half. The `reason_code` beside it is caller-supplied, so its fidelity
/// belongs to the dedup comparison rather than to this rule.
///
/// The observed side is destructured exhaustively, so a field added to
/// [`UsageRecord`] fails to compile here until someone classifies it — the
/// same guard `caller_supplied_eq` carries on both of its sides.
fn server_assigned_eq(observed: &UsageRecord, expected: &UsageRecord) -> bool {
    let UsageRecord {
        id,
        accepted_at,
        origin,
        invalidation,
        gts_type_id: _,
        tenant_id: _,
        resource_ref: _,
        subject_ref: _,
        metadata: _,
        quantity: _,
        idempotency_key: _,
        window_start: _,
        window_end: _,
    } = observed;
    let UsageRecord {
        id: handed_id,
        accepted_at: handed_accepted_at,
        origin: handed_origin,
        invalidation: handed_invalidation,
        ..
    } = expected;
    id == handed_id
        && accepted_at == handed_accepted_at
        && origin == handed_origin
        && invalidation.as_ref().map(|found| found.target)
            == handed_invalidation.as_ref().map(|found| found.target)
}

/// The four server-assigned fields of one entry, rendered for a report.
///
/// An absent `invalidates` is spelled out rather than shown as an empty slot:
/// on an ordinary measurement there is no withdrawn entry to name, and a
/// reader comparing two renderings has to be able to tell that apart from a
/// reference the backend dropped.
fn server_assigned_fields(record: &UsageRecord) -> String {
    format!(
        "id `{id}`, accepted_at `{accepted_at}`, origin `{origin}` and {invalidates}",
        id = record.id,
        accepted_at = record.accepted_at,
        origin = record.origin.as_str(),
        invalidates = record.invalidation.as_ref().map_or_else(
            || "no withdrawn entry named".to_owned(),
            |found| format!("invalidates `{target}`", target = found.target),
        ),
    )
}

/// One entry answered with server-assigned values other than the ones it was
/// handed.
///
/// The rule is restated in every such report rather than left to this
/// module's docs: a plugin author reads the violation, not the suite.
fn mismatch(
    path: &str,
    role: &str,
    observed: &UsageRecord,
    expected: &UsageRecord,
) -> ContractViolation {
    violation(
        SERVER_FIELD_ROUND_TRIP,
        format!(
            "`{path}` answered the {role} entry with {observed}, and the plugin was handed \
             {expected}. `id`, `accepted_at`, `origin` and an invalidation's `invalidates` are \
             stamped once by the Ingestion Gateway. A plugin persists the values it was handed \
             and every read path returns those; it does not re-derive, default, or refresh one, \
             and `accepted_at` in particular is not the store's own insert time.",
            observed = server_assigned_fields(observed),
            expected = server_assigned_fields(expected),
        ),
    )
}

/// Every entry the range under test comes back with on the ledger path.
///
/// `Err` carries a ready-to-report detail.
async fn server_field_page(
    plugin: &dyn UsageCollectorPluginV1,
    meter: &MeterTypeId,
) -> Result<Vec<UsageRecord>, String> {
    let range = TimeRange::new(SERVER_FIELD_WINDOW_FROM, SERVER_FIELD_WINDOW_TO)
        .map_err(|err| format!("the check could not build its own read range: {err}"))?;
    let page = plugin
        .list_usage_records(
            meter.clone(),
            range,
            &contract_query(SERVER_FIELD_PAGE_LIMIT),
            &[],
        )
        .await
        .map_err(|err| {
            format!(
                "`list_usage_records` failed over the range holding this check's three entries, \
                 so what the ledger path answers for the four server-assigned fields could not \
                 be decided: {err}"
            )
        })?;
    Ok(page.items)
}

/// Every entry the feed delivers for this check's own meter, following the
/// cursor until it stands still.
///
/// The subscription is this check's own meter alone, which is what
/// [`check_meter`] exists for: a feed read selects by meter, so a subscription
/// naming the suite's shared meter would carry whatever the other checks left
/// on it and what this check observed would turn on dispatch order.
///
/// The scope is [`contract_scope`], the same single-tenant compiled scope the
/// ledger path dispatches. The feed takes it as its own parameter rather than
/// inside `query.filter`, which is why it is taken from the fixtures module
/// directly here. Every entry this check stores is inside it, so the read
/// buys shape coverage on the path — a plugin that chokes on a compiled
/// scope there meets one — and no scope *enforcement*. Nothing in this suite
/// asserts that obligation on the feed;
/// [`SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH`](crate::contract::SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH)
/// says so, and names the SPI docs that state it instead.
///
/// `Err` carries a ready-to-report detail.
async fn server_field_feed(
    plugin: &dyn UsageCollectorPluginV1,
    meter: &MeterTypeId,
) -> Result<Vec<UsageRecord>, String> {
    let subscription = [meter.clone()];
    let scope = contract_scope();
    let mut delivered: Vec<UsageRecord> = Vec::new();
    let mut start = FeedStart::Oldest;
    let mut reached: Option<FeedPosition> = None;
    for _ in 0..SERVER_FIELD_FEED_PAGES {
        let page = plugin
            .read_feed_page(&subscription, &scope, start, None, SERVER_FIELD_PAGE_LIMIT)
            .await
            .map_err(|err| {
                format!(
                    "`read_feed_page` failed over a subscription naming this check's own meter, \
                     so what the feed answers for the four server-assigned fields could not be \
                     decided: {err}"
                )
            })?;
        delivered.extend(page.entries);
        // An unbounded read carries a cursor on every page; `None` is the
        // shape a bounded replay returns at its `until`, and this read sends
        // none. Either way there is nothing left to follow.
        let Some(next) = page.next else { break };
        if reached.as_ref() == Some(&next) {
            break;
        }
        reached = Some(next.clone());
        start = FeedStart::After(next);
    }
    Ok(delivered)
}

/// Builds the record the retry re-delivers, the withdrawn pair that supplies
/// an `invalidates`, and the divergent retry itself.
///
/// Five guards keep the check from passing by construction, and all five are
/// the suite's own facts rather than the plugin's — so all five are reported
/// as [`HARNESS_FAULT`] rather than against the plugin. See
/// [`ServerFieldFixtures::guards`] for what each one holds.
fn server_field_fixtures() -> Result<ServerFieldFixtures, String> {
    let meter = check_meter(SERVER_FIELD_ROUND_TRIP, "main")?;
    let quantity = UsageQuantity::parse(SERVER_FIELD_QUANTITY).map_err(|err| {
        format!("the check's own quantity `{SERVER_FIELD_QUANTITY}` does not parse: {err}")
    })?;
    let retried_key = server_field_key("retried")?;

    let record = on_the_backfill_route(fixture_record_on(
        meter.clone(),
        CONTRACT_TENANT_ID,
        &retried_key,
        quantity,
        SERVER_FIELD_RECORD_ACCEPTED_AT,
        SERVER_FIELD_WINDOW_FROM,
        SERVER_FIELD_WINDOW_END,
    )?);
    let retry = fixture_record_on(
        meter.clone(),
        CONTRACT_TENANT_ID,
        &retried_key,
        quantity,
        SERVER_FIELD_RETRY_ACCEPTED_AT,
        SERVER_FIELD_WINDOW_FROM,
        SERVER_FIELD_WINDOW_END,
    )?;
    let withdrawn = fixture_record_on(
        meter.clone(),
        CONTRACT_TENANT_ID,
        &server_field_key("withdrawn")?,
        quantity,
        SERVER_FIELD_WITHDRAWN_ACCEPTED_AT,
        SERVER_FIELD_WINDOW_FROM,
        SERVER_FIELD_WINDOW_END,
    )?;
    let invalidation = on_the_backfill_route(fixture_invalidation_on(
        &withdrawn,
        SERVER_FIELD_REASON_CODE,
        SERVER_FIELD_INVALIDATION_ACCEPTED_AT,
    )?);

    let fixtures = ServerFieldFixtures {
        meter,
        record,
        withdrawn,
        invalidation,
        retry,
    };
    fixtures.guards()?;
    Ok(fixtures)
}

impl ServerFieldFixtures {
    /// The five facts about these fixtures that this check's assertions
    /// read, established rather than assumed.
    ///
    /// * **The retry is a retry.** It derives the record's `id` and departs
    ///   from it in no caller-supplied field, so a conforming backend absorbs
    ///   it. A retry that collided instead would make property four assert
    ///   the shape of a conflict rather than the values of an absorb.
    /// * **The retry diverges where it must.** Its `accepted_at` and its
    ///   `origin` both differ from the stored record's. Agreeing on either
    ///   would make "the answer carries the stored values" indistinguishable
    ///   from "the answer carries the retry's" in that field, and every
    ///   backend would pass it.
    /// * **The four acceptance instants are four values.** A backend that
    ///   answered one entry's instant for another's, or one constant for all
    ///   of them, is caught only where the instants differ.
    /// * **The two records carry two origins.** A fixture set stamped one
    ///   origin throughout is satisfied by a backend that answers that value
    ///   whatever it stored.
    /// * **The withdrawal names its target.** `invalidates` is one of the
    ///   four fields under test, and a fixture carrying no reference would
    ///   have this check assert that a backend round-trips an absence.
    fn guards(&self) -> Result<(), String> {
        if self.retry.id != self.record.id || !self.retry.caller_supplied_eq(&self.record) {
            return Err(format!(
                "the retry ({retry}) is not a re-delivery of the record ({record}): a retry \
                 shares its target's six identity inputs and departs from it in no \
                 caller-supplied field, and one that does not is refused as a conflict rather \
                 than absorbed - so the property this check asserts about an absorb would never \
                 be reached",
                retry = self.retry.id,
                record = self.record.id,
            ));
        }
        if self.retry.accepted_at == self.record.accepted_at
            || self.retry.origin == self.record.origin
        {
            return Err(format!(
                "the retry repeats the record's acceptance instant (`{accepted_at}`) or its \
                 origin (`{origin}`); the assertion is that an absorb answers with the stored \
                 entry's values rather than the retry's, and in a field the two agree on every \
                 backend passes it",
                accepted_at = self.record.accepted_at,
                origin = self.record.origin.as_str(),
            ));
        }
        let instants = std::collections::BTreeSet::from([
            self.record.accepted_at,
            self.withdrawn.accepted_at,
            self.invalidation.accepted_at,
            self.retry.accepted_at,
        ]);
        if instants.len() != 4 {
            return Err(
                "two of this check's four acceptance instants are one value; the check asserts \
                 that each entry reads back with its own, and two entries stamped alike cannot \
                 catch a backend answering one entry's instant for another's"
                    .to_owned(),
            );
        }
        self.origins_and_the_target_hold()
    }

    /// The second half of [`Self::guards`]: two stored origins rather than
    /// one, and a withdrawal that names its target.
    ///
    /// Split out because the two are about the fixture set's shape rather
    /// than about the retry, and because one function asserting all five
    /// reads as a checklist rather than as five separate premises.
    fn origins_and_the_target_hold(&self) -> Result<(), String> {
        if self.record.origin == self.withdrawn.origin {
            return Err(format!(
                "the record and the entry the withdrawal names both carry the origin \
                 `{origin}`; `origin` is one of the four fields under test, and a fixture set \
                 that never varies it is satisfied by a backend that returns that one value \
                 whatever it was handed",
                origin = self.record.origin.as_str(),
            ));
        }
        let target = self
            .invalidation
            .invalidation
            .as_ref()
            .map(|found| found.target);
        if target != Some(self.withdrawn.id) {
            return Err(format!(
                "the withdrawal ({invalidation}) names {observed} as the entry it withdraws, and \
                 it withdraws {withdrawn}; `invalidates` is one of the four fields under test, \
                 and a fixture carrying the wrong reference - or none - would have this check \
                 assert that a backend round-trips a value the suite never meant to hand it",
                invalidation = self.invalidation.id,
                observed = target.map_or_else(
                    || "no entry at all".to_owned(),
                    |target| format!("`{target}`")
                ),
                withdrawn = self.withdrawn.id,
            ));
        }
        Ok(())
    }
}

/// The same entry stamped as having arrived on the bulk-import route.
///
/// [`fixture_record_on`] stamps every entry it projects
/// [`RecordOrigin::Live`], and so does [`fixture_invalidation_on`]. A fixture
/// set that never varied `origin` would assert that the field round-trips
/// while passing against a backend that answers `live` whatever it stored, so
/// this check hands the plugin both values.
///
/// The restamp is applied to the projected entry rather than plumbed through
/// the shared builders because `origin` is not one of the six inputs the
/// derived identity reads — DESIGN §3.1's "Identity derivation" gives them as
/// `(tenant_id, gts_type_id, idempotency_key, window_start, window_end,
/// entry_type)` — so the entry's `id` is the one the builder derived, and the
/// restamped entry is the same entry it derived that `id` for.
///
/// It is also the ordinary shape rather than an exotic one. [`RecordOrigin`]'s
/// own docs say a withdrawal of a period older than the live past tolerance
/// travels the backfill route, "which makes that the normal origin for a
/// correction of closed history, not an unusual one".
fn on_the_backfill_route(record: UsageRecord) -> UsageRecord {
    UsageRecord {
        origin: RecordOrigin::Backfill,
        ..record
    }
}

/// The idempotency key one role of this check's fixture submits under.
///
/// Keyed on the role name for the reason
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture) spells
/// out: in a ledger with no delete path, an edited fixture must take a fresh
/// identity rather than inherit an accepted entry's.
fn server_field_key(role: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{SERVER_FIELD_ROUND_TRIP}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}
