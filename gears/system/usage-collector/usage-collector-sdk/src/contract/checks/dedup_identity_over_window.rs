//! The DESIGN §3.3 `dedup-identity-over-window` check.
//!
//! See [`dedup_identity_over_window`] for what it asserts; the module holds
//! the two submissions it decides the derived identity from.

use uuid::Uuid;

use rust_decimal::Decimal;

use crate::contract::fixtures::{
    CONTRACT_METER_TYPE_ID, CONTRACT_TENANT_ID, FIXTURE_EPOCH, contract_query, fixture_record,
    violation,
};
use crate::contract::{ContractViolation, DEDUP_IDENTITY_OVER_WINDOW, HARNESS_FAULT};
use crate::derive_usage_record_id;
use crate::models::{IdempotencyKey, MeterTypeId, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::time_range::TimeRange;

/// The start of the first of this check's two covered periods, and the
/// inclusive lower bound of the range it reads them back over.
///
/// Sixty days past [`FIXTURE_EPOCH`], for the reason
/// [`WINDOW_SELECTION_FROM`](super::window_end_selection::WINDOW_SELECTION_FROM)
/// gives.
const DEDUP_WINDOW_FROM: time::OffsetDateTime =
    FIXTURE_EPOCH.saturating_add(time::Duration::days(60));

/// The exclusive upper bound of that range, an hour past the end of the
/// later of the two periods so both are selected by their end.
const DEDUP_WINDOW_TO: time::OffsetDateTime =
    DEDUP_WINDOW_FROM.saturating_add(time::Duration::hours(3));

/// The read limit this check dispatches.
///
/// Twice the two entries it expects, and the margin is the assertion. A
/// backend that stored the same-period resubmission as a second row is
/// caught by counting the rows carrying one id, and a limit set to the
/// expected two would truncate that second row away — turning the half of
/// this check that catches a backend with no dedup at all into a pass.
pub const DEDUP_PAGE_LIMIT: u64 = 4;

/// The two submissions this check works with: one idempotency key, two
/// covered periods.
struct DedupFixtures {
    /// The entry over the earlier period.
    first: UsageRecord,
    /// The entry over the later one, submitted under the same key.
    second: UsageRecord,
    /// [`derive_usage_record_id`] over the earlier period's five identity
    /// attributes — derived here rather than read off [`Self::first`], so
    /// the check names the entry the way the gear does.
    first_id: Uuid,
    /// The same derivation over the later period's.
    second_id: Uuid,
}

/// `dedup-identity-over-window` — *"Both period bounds are part of the
/// identity, so a same-key submission over a different period is a distinct
/// entry."*
///
/// **Both halves are asserted, and neither is sufficient alone.**
///
/// The first half is the rule as DESIGN states it: one idempotency key over
/// two different periods is two entries, both admitted and both readable,
/// under the two distinct ids [`derive_usage_record_id`] produces from the
/// five identity attributes. That half on its own is satisfied by a backend
/// that dedups nothing whatsoever — two submissions produced two rows is
/// exactly what no deduplication looks like.
///
/// The second half is what closes that: the same key over the **same**
/// period, every canonical field identical, is an idempotent replay. The
/// stored row comes back and `list_usage_records` shows one entry, not two.
///
/// What the pair catches is a backend keying dedup on
/// `(tenant_id, gts_type_id, idempotency_key)` alone — the obvious schema,
/// and the one the pre-period model had. It answers `IdempotencyConflict`
/// to the second period's submission and so fails the first half, while
/// passing the second and every other check in this suite: the bounds are
/// invisible to it, and nothing else here submits one key over two periods.
pub async fn dedup_identity_over_window(
    plugin: &dyn UsageCollectorPluginV1,
) -> Vec<ContractViolation> {
    let fixtures = match dedup_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own \
                     `{DEDUP_IDENTITY_OVER_WINDOW}` fixtures, so nothing was submitted. This is \
                     a fault in the suite, not in the plugin under test: {detail}"
                ),
            )];
        }
    };
    let mut violations = Vec::new();

    // Half one: one key, two periods, two entries.
    let mut submitted = Vec::new();
    for (ordinal, record, expected) in [
        ("earlier", &fixtures.first, fixtures.first_id),
        ("later", &fixtures.second, fixtures.second_id),
    ] {
        if let Err(err) = plugin.create_usage_record(record.clone()).await {
            violations.push(violation(
                DEDUP_IDENTITY_OVER_WINDOW,
                format!(
                    "`create_usage_record` refused the {ordinal} of two submissions carrying the \
                     idempotency key `{key}` over different covered periods (record {expected}, \
                     period `{start}` to `{end}`): {err}. Both period bounds are part of the \
                     dedup identity, so the two are distinct entries and both must be admitted; \
                     a backend keying dedup on `(tenant_id, gts_type_id, idempotency_key)` alone \
                     reports a conflict here.",
                    key = record.idempotency_key.as_str(),
                    start = record.window_start,
                    end = record.window_end,
                ),
            ));
            continue;
        }
        submitted.push((ordinal, expected));
    }

    let returned = match dedup_page(plugin).await {
        Ok(returned) => returned,
        Err(detail) => {
            violations.push(violation(DEDUP_IDENTITY_OVER_WINDOW, detail));
            return violations;
        }
    };
    for (ordinal, expected) in submitted {
        if returned.contains(&expected) {
            continue;
        }
        violations.push(violation(
            DEDUP_IDENTITY_OVER_WINDOW,
            format!(
                "the {ordinal} of two submissions carrying one idempotency key over different \
                 covered periods was accepted as record {expected}, and a range containing both \
                 periods' ends did not return it. A same-key submission over a different period \
                 is a distinct entry, so both must be readable under the two distinct ids \
                 `derive_usage_record_id` produces."
            ),
        ));
    }

    // Half two: the same key over the *same* period is one entry, not two.
    match plugin.create_usage_record(fixtures.first.clone()).await {
        Ok(stored) if stored == fixtures.first => {}
        Ok(stored) => violations.push(violation(
            DEDUP_IDENTITY_OVER_WINDOW,
            format!(
                "resubmitting record {expected} verbatim answered a different entry ({observed}). \
                 A re-delivery of an accepted entry is an idempotent replay: the stored row comes \
                 back unchanged.",
                expected = fixtures.first_id,
                observed = stored.id,
            ),
        )),
        Err(err) => violations.push(violation(
            DEDUP_IDENTITY_OVER_WINDOW,
            format!(
                "resubmitting record {expected} verbatim, under the same idempotency key over \
                 the same covered period with every canonical field identical, was refused: \
                 {err}. A re-delivery of an accepted entry is an idempotent replay, not a \
                 conflict.",
                expected = fixtures.first_id,
            ),
        )),
    }

    match dedup_page(plugin).await {
        Ok(returned) => {
            let seen = returned
                .iter()
                .filter(|id| **id == fixtures.first_id)
                .count();
            if seen != 1 {
                violations.push(violation(
                    DEDUP_IDENTITY_OVER_WINDOW,
                    format!(
                        "record {expected} was submitted twice over one covered period under one \
                         idempotency key, and `list_usage_records` returned it {seen} times \
                         rather than once. Without this the check passes against a backend that \
                         deduplicates nothing at all, for which two submissions producing two \
                         rows is the correct-looking answer.",
                        expected = fixtures.first_id,
                    ),
                ));
            }
        }
        Err(detail) => violations.push(violation(DEDUP_IDENTITY_OVER_WINDOW, detail)),
    }

    violations
}

/// Builds the two submissions, deriving each entry's id the way the gear
/// does.
///
/// The ids come from [`derive_usage_record_id`] over the five identity
/// attributes rather than being hardcoded or read back off the projected
/// record, and the two guards below are what keep the check honest rather
/// than vacuous:
///
/// * The projection derives the same id it does. They cannot disagree —
///   the projection calls the same function — and if they ever did, every
///   read-back assertion would look for an id nothing was stored under and
///   blame the plugin for the suite's own confusion.
/// * The two periods derive **different** ids. If they did not, "a same-key
///   submission over a different period is a distinct entry" would have
///   nothing to assert and the check would pass by construction.
///
/// Both are the suite's own facts, so both are `Err` here and reported as
/// [`HARNESS_FAULT`] rather than against the plugin.
fn dedup_fixtures() -> Result<DedupFixtures, String> {
    let idempotency_key = IdempotencyKey::new(DEDUP_IDENTITY_OVER_WINDOW)
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))?;
    let meter = MeterTypeId::new(CONTRACT_METER_TYPE_ID)
        .map_err(|err| format!("the check's own meter type id is invalid: {err}"))?;

    let middle = DEDUP_WINDOW_FROM.saturating_add(time::Duration::hours(1));
    let last = DEDUP_WINDOW_FROM.saturating_add(time::Duration::hours(2));
    let periods = [(DEDUP_WINDOW_FROM, middle), (middle, last)];

    let mut records = Vec::with_capacity(periods.len());
    for (window_start, window_end) in periods {
        let derived = derive_usage_record_id(
            CONTRACT_TENANT_ID,
            &meter,
            &idempotency_key,
            window_start,
            window_end,
        );
        let record = fixture_record(&idempotency_key, Decimal::ONE, window_start, window_end)?;
        if record.id != derived {
            return Err(format!(
                "the suite derived {derived} for the period `{window_start}` to `{window_end}` \
                 and projected the same submission as {projected}, so its two ways of naming one \
                 entry disagree",
                projected = record.id,
            ));
        }
        records.push((record, derived));
    }

    let mut records = records.into_iter();
    let (first, first_id) = records
        .next()
        .ok_or("the check built no fixture for its earlier period")?;
    let (second, second_id) = records
        .next()
        .ok_or("the check built no fixture for its later period")?;
    if first_id == second_id {
        return Err(format!(
            "one idempotency key over two different covered periods derived the same id \
             ({first_id}), so there is nothing for this check to assert"
        ));
    }
    Ok(DedupFixtures {
        first,
        second,
        first_id,
        second_id,
    })
}

/// Every `UsageRecord.id` a range containing both periods' ends comes back
/// with, in the order the page carried them.
///
/// A `Vec` rather than a set, because half two counts repetitions: a set
/// would collapse the duplicate row it exists to find.
async fn dedup_page(plugin: &dyn UsageCollectorPluginV1) -> Result<Vec<Uuid>, String> {
    let meter = MeterTypeId::new(CONTRACT_METER_TYPE_ID)
        .map_err(|err| format!("the check's own meter type id is invalid: {err}"))?;
    let range = TimeRange::new(DEDUP_WINDOW_FROM, DEDUP_WINDOW_TO)
        .map_err(|err| format!("the check could not build its own read range: {err}"))?;
    let page = plugin
        .list_usage_records(meter, range, &contract_query(DEDUP_PAGE_LIMIT), &[])
        .await
        .map_err(|err| {
            format!(
                "`list_usage_records` failed over the range containing both covered periods, so \
                 neither half of the dedup identity could be decided: {err}"
            )
        })?;
    Ok(page.items.into_iter().map(|item| item.id).collect())
}
