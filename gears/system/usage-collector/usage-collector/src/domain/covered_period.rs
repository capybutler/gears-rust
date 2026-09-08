//! The ingestion path's covered-period bounds.
//!
//! Three durations and two pure functions over them: one admits or refuses
//! an entry, the other picks the PDP action the admitted entry is
//! authorized against. Both read **only the end of the covered period** —
//! the instant that makes consumption current or historical
//! (`cpt-cf-usage-collector-adr-backfill-isolation`, "Why the three bounds
//! are asymmetric") — together with the route the entry arrived on. Neither
//! reads `window_start`, the length of the period, the arrival instant, or
//! the entry kind.
//!
//! That last exclusion is the one that surprises. A withdrawal copies its
//! target's period, so an invalidation of a closed month has a `window_end`
//! a month old and is rejected on the live path exactly as a fresh
//! measurement of that month would be. `origin = backfill` on a correction
//! is therefore the ordinary case.
//!
//! `now` is a parameter rather than a call, so every entry of one batch is
//! judged against a single instant and a test needs no clock control.

use time::{Duration, OffsetDateTime};
use usage_collector_sdk::{RecordOrigin, UsageCollectorError};

use crate::domain::authz::usage_record;

/// Default future tolerance (5 minutes), the value DESIGN
/// `cpt-cf-usage-collector-fr-live-future-time-bound` publishes.
///
/// The three constants below are `pub(crate)` for the same reason
/// [`DEFAULT_METADATA_SIZE_CAP_BYTES`] is: the domain owns the number and
/// `crate::config` anchors its own default on it, rather than each
/// spelling the value and drifting.
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
    /// `UsageCollectorConfig`'s own defaults are the same three constants,
    /// so this agrees with `UsageCollectorConfig::default()
    /// .covered_period_bounds()` by construction rather than by
    /// coincidence. `config_tests` keeps the published numbers asserted on
    /// the config side, which is what stops the pair drifting silently.
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
/// `Duration`s — the difference of the two instants against the tolerance —
/// rather than between an instant and `now + tolerance`. That is deliberate
/// and is why no `checked_add` appears here: `OffsetDateTime` spans years
/// -9999..=9999, the difference of any two values in that span fits a
/// `Duration`, so the subtraction cannot overflow, whereas adding a
/// configured tolerance to an instant near the end of the range can.
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
/// `backfill_window` at all. Deriving it instead from the arithmetic — a
/// live-admitted entry is inside the past tolerance, therefore inside the
/// window — would couple two independently configured keys, and a
/// deployment that widened the past tolerance past the window would start
/// demanding an elevated grant for ordinary live emission.
///
/// Reads `window_end` and the origin, matching what
/// [`enforce_covered_period_bounds`] reads, so one batch can carry entries
/// bound to both actions. That is safe by construction: `action`
/// participates in `AttributionTupleKey`'s hash/eq, so the two cannot
/// collapse onto a single PDP decision.
#[must_use]
pub fn ingestion_action(
    bounds: &CoveredPeriodBounds,
    origin: RecordOrigin,
    now: OffsetDateTime,
    window_end: OffsetDateTime,
) -> &'static str {
    // Arm order is load-bearing, and the guard's pattern carries the
    // live-path exclusion structurally: `backfill_window` is reachable only
    // under `RecordOrigin::Backfill`. (The two `CREATE` arms are merged
    // because `clippy::match_same_arms` is denied workspace-wide; the
    // behaviour is the same as spelling them separately.)
    match origin {
        RecordOrigin::Backfill if now - window_end > bounds.backfill_window => {
            usage_record::actions::BACKFILL
        }
        RecordOrigin::Live | RecordOrigin::Backfill => usage_record::actions::CREATE,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "covered_period_tests.rs"]
mod covered_period_tests;
