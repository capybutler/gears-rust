//! The ingestion path's covered-period bounds.
//!
//! Three durations and one pure function over them. The function reads
//! **only the end of the covered period** — the instant that makes
//! consumption current or historical
//! (`cpt-cf-usage-collector-adr-backfill-isolation`, "Why the three bounds
//! are asymmetric"). It reads neither `window_start`, nor the length of the
//! period, nor the arrival instant, nor the entry kind.
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
    /// Carried but not yet read: it selects a PDP action rather than
    /// admitting or refusing an entry, and the `backfill` action does not
    /// exist yet. The field lands with its two siblings so the bounds
    /// arrive as one projection of the configured block rather than in two
    /// instalments.
    pub backfill_window: Duration,
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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "covered_period_tests.rs"]
mod covered_period_tests;
