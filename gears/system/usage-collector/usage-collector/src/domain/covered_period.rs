//! The ingestion path's covered-period bounds.
//!
//! Two pure functions over the configured durations: one admits or refuses
//! an entry, the other picks the PDP action the admitted entry is authorized
//! against. Both read **only the end of the covered period** — the instant
//! that makes consumption current or historical
//! (`cpt-cf-usage-collector-adr-backfill-isolation`) — together with the
//! route the entry arrived on. Neither reads `window_start`, the length of
//! the period, the arrival instant, or the entry kind.
//!
//! The last exclusion is the one that surprises: a withdrawal copies its
//! target's period, so an invalidation of a closed month is rejected on the
//! live path exactly as a fresh measurement of that month would be, which
//! makes `origin = backfill` the ordinary case for a correction.
//!
//! `now` is a parameter rather than a call, so every entry of one batch is
//! judged against a single instant and a test needs no clock control.

use time::{Duration, OffsetDateTime};
use usage_collector_sdk::{RecordOrigin, UsageCollectorError};

use crate::domain::authz::usage_record;

/// Default future tolerance (5 minutes), the value DESIGN
/// `cpt-cf-usage-collector-fr-live-future-time-bound` publishes.
///
/// `pub(crate)`, with its two siblings, for the same reason
/// [`DEFAULT_METADATA_SIZE_CAP_BYTES`] is: the domain owns the number and
/// `crate::config` anchors its own default on it rather than respelling it.
///
/// [`DEFAULT_METADATA_SIZE_CAP_BYTES`]: crate::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES
pub(crate) const DEFAULT_LIVE_FUTURE_TOLERANCE_SECS: u64 = 300;

/// Default live past tolerance (48 hours). Covers emitter outage and retry
/// lag; anything older is history and belongs on the backfill route.
pub(crate) const DEFAULT_LIVE_PAST_TOLERANCE_SECS: u64 = 172_800;

/// Default backfill window (90 days). Bounds the recomputation obligation a
/// materialised aggregate carries.
pub(crate) const DEFAULT_BACKFILL_WINDOW_SECS: u64 = 7_776_000;

/// The three configured covered-period bounds.
///
/// Projected from [`crate::config::UsageCollectorConfig`] — see
/// [`UsageCollectorConfig::covered_period_bounds`] for the keys and for why
/// the projection cannot fail.
///
/// [`UsageCollectorConfig::covered_period_bounds`]: crate::config::UsageCollectorConfig::covered_period_bounds
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoveredPeriodBounds {
    /// How far ahead of `now` a covered period may end. Applies on **both**
    /// routes: the backfill route lifts the past bound and nothing else.
    pub future_tolerance: Duration,

    /// How far behind `now` a covered period may end on the **live** route
    /// only. The backfill route exists for exactly the periods this bound
    /// refuses.
    pub live_past_tolerance: Duration,

    /// How far back the backfill route reaches without elevated
    /// authorization.
    ///
    /// Read by [`ingestion_action`] and by nothing else: it selects a PDP
    /// action rather than admitting or refusing an entry, so no entry is
    /// ever rejected for crossing it — an entry beyond it is authorized
    /// against `backfill` instead of `create`.
    pub backfill_window: Duration,
}

impl Default for CoveredPeriodBounds {
    /// The published defaults, for a `Service` built without a configured
    /// block — tests and pre-init contexts.
    ///
    /// `UsageCollectorConfig`'s own defaults are these same constants, so
    /// this agrees with `UsageCollectorConfig::default().covered_period_bounds()`
    /// by construction. `config_tests` asserts the published numbers on the
    /// config side.
    fn default() -> Self {
        Self {
            future_tolerance: Duration::seconds(seconds(DEFAULT_LIVE_FUTURE_TOLERANCE_SECS)),
            live_past_tolerance: Duration::seconds(seconds(DEFAULT_LIVE_PAST_TOLERANCE_SECS)),
            backfill_window: Duration::seconds(seconds(DEFAULT_BACKFILL_WINDOW_SECS)),
        }
    }
}

/// A configured bound's seconds as the `i64` [`Duration::seconds`] takes.
///
/// Saturates rather than panics, and `UsageCollectorConfig::validate`
/// refuses at bootstrap any value that could saturate it — which is what
/// makes `UsageCollectorConfig::covered_period_bounds` infallible.
pub(crate) fn seconds(secs: u64) -> i64 {
    i64::try_from(secs).unwrap_or(i64::MAX)
}

/// Reject a covered period the ingestion path does not admit.
///
/// Reads `window_end` and nothing else. Both comparisons are between two
/// `Duration`s — the difference of the instants against the tolerance —
/// rather than between an instant and `now + tolerance`, which is why no
/// `checked_add` appears: the difference of any two `OffsetDateTime`s fits a
/// `Duration`, whereas adding a configured tolerance to an instant near the
/// end of the range can overflow.
///
/// # Errors
///
/// * [`UsageCollectorError::InvalidArgument`] with reason `FUTURE_WINDOW`
///   when the period ends further ahead than `future_tolerance`, on either
///   route.
/// * [`UsageCollectorError::InvalidArgument`] with reason `PAST_WINDOW`
///   when it ends further back than `live_past_tolerance` on the **live**
///   route. The detail names the backfill route, which is where such an
///   entry belongs.
///
/// Realizes feature 2.4's covered-period validation for the future/past
/// tolerance checks and the end-only read. The period's own well-formedness
/// preconditions — offset presence, UTC normalization, start-not-after-end
/// ordering, the microsecond precision ceiling — are enforced earlier, in
/// `usage_collector_sdk::CreateUsageRecord`'s private `project()`.
///
/// **Does NOT realize `cpt-cf-usage-collector-algo-backfill-window-bound` /
/// `cpt-cf-usage-collector-dod-backfill-window-bound`.** For
/// `RecordOrigin::Backfill` there is no past-tolerance branch at all, so a
/// backfilled entry's period end is checked against `future_tolerance` only
/// and is never rejected for being old. `backfill_window` is read solely by
/// [`ingestion_action`], to choose a PDP action; nothing anywhere in the gear
/// rejects a covered period for exceeding it. See that function's doc.
// @cpt-algo:cpt-cf-usage-collector-algo-covered-period-validation:p1
// @cpt-dod:cpt-cf-usage-collector-dod-covered-period-validation:p1
// @cpt-dod:cpt-cf-usage-collector-dod-live-time-bounds:p1
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub fn enforce_covered_period_bounds(
    bounds: &CoveredPeriodBounds,
    origin: RecordOrigin,
    now: OffsetDateTime,
    window_end: OffsetDateTime,
) -> Result<(), UsageCollectorError> {
    if window_end - now > bounds.future_tolerance {
        return Err(UsageCollectorError::covered_period_beyond_future_tolerance(
            window_end,
            now,
            bounds.future_tolerance,
        ));
    }
    if origin == RecordOrigin::Live && now - window_end > bounds.live_past_tolerance {
        return Err(UsageCollectorError::covered_period_before_past_tolerance(
            window_end,
            now,
            bounds.live_past_tolerance,
        ));
    }
    Ok(())
}

/// The PDP action this entry is authorized against.
///
/// Reaching past the backfill window is the elevated case, and the action
/// is what makes it one: an operator grants `backfill` to an import job and
/// not to an ordinary emitter. Inside the window a backfilled entry needs
/// no grant a live entry does not.
///
/// The live arm returns `actions::CREATE` without consulting
/// `backfill_window`. Deriving the live answer from the arithmetic would
/// happen to be correct today — `config::validate` refuses
/// `backfill_window < live_past_tolerance` — but that invariant lives on
/// [`UsageCollectorConfig`], not on [`CoveredPeriodBounds`], which any caller
/// can construct directly.
///
/// Reads `window_end` and the origin, matching [`enforce_covered_period_bounds`],
/// so one batch can carry entries bound to both actions: `action`
/// participates in `AttributionTupleKey`'s hash/eq, so the two cannot
/// collapse onto a single PDP decision.
///
/// [`UsageCollectorConfig`]: crate::config::UsageCollectorConfig
///
/// **Ruling G23 — recorded here, not built.** This is `backfill_window`'s
/// **only** functional reader in the gear, and reading it here is
/// authorization routing, not a rejection.
/// `cpt-cf-usage-collector-algo-backfill-window-bound` and
/// `cpt-cf-usage-collector-dod-backfill-window-bound` (feature 2.8) describe
/// a **hard bound enforced before persistence** — "reject a period ending
/// further back than the window ... MUST NOT expose any override reaching
/// past the window." That rejection exists nowhere in this gear: an entry
/// whose period ends arbitrarily far in the past is admitted on the backfill
/// route exactly as one inside the window, provided the caller holds the
/// `backfill` permission this function routes it to.
///
/// **The documents assert the opposite throughout `docs/`** — a PRD MUST and
/// MUST NOT, a feature-document MUST, ADR-0012's decision sentence, the
/// published `OpenAPI` contract (which also specifies the rejection's wire
/// shape), and `DESIGN.md` in its backfill-import sequence diagram ("backfill
/// window check, hard bound, no override"), its `backfill_window_secs`
/// configuration row, and its v1-deferrals table.
///
/// **One site depends on the unenforced bound rather than merely asserting
/// it.** `plugins/timescaledb-usage-collector-plugin/docs/DESIGN.md`, §3.6's
/// feed sequence, **Retention refusal → Positions within H**, argues: "an
/// entry
/// retention deletes was accepted at least H + S before its drop: its
/// `window_end` is no earlier than `accepted_at` less the backfill window
/// … so a mark never refuses a position within H." That step *assumes* this
/// bound. Without it, an entry whose `window_end` is arbitrarily older than
/// its `accepted_at` is admitted on the backfill route, and the feed's "a
/// mark never refuses a position within H" completeness claim does not
/// follow. A correctness argument resting on an unbuilt check, not a stale
/// sentence.
///
/// **The gear's own code says the opposite, independently:**
/// `config.rs`'s `backfill_window_secs` doc ("a zero window leaves no period
/// the backfill route admits **without elevated authorization**"),
/// `routes/usage_records.rs`'s backfill route description ("require elevated
/// authorization"), and `service_tests.rs`'s
/// `one_backfill_batch_mixing_window_sides_authorizes_two_distinct_actions`
/// ("crossing the backfill window selects a different verb; it does NOT
/// reject the entry").
///
/// **G22 and G23 are two halves of one design conflict.** DESIGN's model: the
/// **route** selects the PEP action and the **window** is a hard bound a
/// caller cannot cross. This gear's model is the mirror image: the **window**
/// selects the action (this function) and **nothing** bounds the period — see
/// `authz.rs`'s `scope_admits_attribution_tuple` doc for G22's half.
///
/// **Building the hard bound would make this function's `BACKFILL` arm dead
/// code**, since the bound would refuse the very periods that arm routes,
/// silently retiring the elevated-permission path DESIGN's own two-action
/// model depends on. Which of the two mirrored models wins is a design
/// decision rather than a bug. The feature-2.8 identifiers this falsifies
/// stay unticked and unmarked, with the gap named here and at
/// `service.rs`'s `Service::backfill_usage_records`.
///
/// **It reaches feature 2.5 too**, whose identifiers stay unticked for it:
/// `cpt-cf-usage-collector-algo-invalidation-route-binding` defers the window
/// bound to feature 2.8, `cpt-cf-usage-collector-dod-invalidation-on-import-route`
/// requires "its window bound ... **MUST** apply to an invalidation exactly
/// as [it applies] to an ordinary record", and
/// `cpt-cf-usage-collector-flow-invalidate-backfilled-record` /
/// `cpt-cf-usage-collector-flow-resubmit-withdrawal` each carry an error
/// scenario where that bound refuses a withdrawal's copied period. There is
/// no such bound to defer to. The invalidation half this module *does*
/// satisfy is DESIGN's "The copied period is bounded by the path, not by the
/// entry kind", because neither function here reads the entry kind.
#[must_use]
pub fn ingestion_action(
    bounds: &CoveredPeriodBounds,
    origin: RecordOrigin,
    now: OffsetDateTime,
    window_end: OffsetDateTime,
) -> &'static str {
    // The window comparison is nested inside `Backfill`, so the live arm
    // cannot reach `bounds` at all.
    match origin {
        RecordOrigin::Live => usage_record::actions::CREATE,
        RecordOrigin::Backfill => {
            if now - window_end > bounds.backfill_window {
                usage_record::actions::BACKFILL
            } else {
                usage_record::actions::CREATE
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "covered_period_tests.rs"]
mod covered_period_tests;
