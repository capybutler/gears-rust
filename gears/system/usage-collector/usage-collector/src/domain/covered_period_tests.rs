//! Unit tests for the ingestion path's covered-period bounds.
//!
//! Pure: no `Service`, no plugin, no clock. Every offset below is
//! unambiguously inside or outside its bound — one hour against five
//! minutes, 72 hours against 48 — because a test placed on the boundary
//! would pin the comparison's strictness, which no document fixes, and
//! would flake on a loaded runner besides.

use time::{Duration, OffsetDateTime};
use usage_collector_sdk::{RecordOrigin, UsageCollectorError, ValidationReason};

use super::{CoveredPeriodBounds, enforce_covered_period_bounds};

/// A fixed instant, supplied rather than read from the clock, so every
/// entry of a batch is judged against one `now` and no test races the
/// clock. `2026-06-11T12:00:00Z`.
fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_780_142_400).expect("valid instant")
}

/// The published defaults: 5 minutes, 48 hours, 90 days.
fn default_bounds() -> CoveredPeriodBounds {
    CoveredPeriodBounds {
        future_tolerance: Duration::minutes(5),
        live_past_tolerance: Duration::hours(48),
        backfill_window: Duration::days(90),
    }
}

/// A covered-period end `offset` from [`now`]. Negative is in the past.
fn ending(offset: Duration) -> OffsetDateTime {
    now() + offset
}

fn reason_of(err: &UsageCollectorError) -> &ValidationReason {
    match err {
        UsageCollectorError::InvalidArgument { reason, .. } => reason,
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

#[test]
fn the_live_path_admits_a_period_ending_inside_both_tolerances() {
    assert!(
        enforce_covered_period_bounds(
            &default_bounds(),
            RecordOrigin::Live,
            now(),
            ending(Duration::hours(-1)),
        )
        .is_ok()
    );
}

#[test]
fn the_live_path_rejects_a_period_ending_beyond_the_future_tolerance() {
    let err = enforce_covered_period_bounds(
        &default_bounds(),
        RecordOrigin::Live,
        now(),
        ending(Duration::hours(1)),
    )
    .expect_err("an hour ahead is beyond a five-minute tolerance");
    assert_eq!(*reason_of(&err), ValidationReason::FutureWindow);
}

#[test]
fn the_live_path_rejects_a_period_ending_beyond_the_past_tolerance() {
    let err = enforce_covered_period_bounds(
        &default_bounds(),
        RecordOrigin::Live,
        now(),
        ending(Duration::hours(-72)),
    )
    .expect_err("72 hours back is beyond a 48-hour tolerance");
    assert_eq!(*reason_of(&err), ValidationReason::PastWindow);
}

#[test]
fn the_live_path_admits_a_period_that_closed_a_minute_ago() {
    // ADR confirmation case 2: a monthly accrual meter emits as soon as its
    // month closes, and that is ordinary live consumption however long the
    // period was.
    //
    // The "however long" half cannot be pinned here — this function is
    // handed a `window_end` and never sees a start, so a rule phrased about
    // the period's LENGTH is not even expressible against it. That
    // exclusion is enforced one layer up, where the Service picks which
    // instant to hand over:
    // `covered_period_bounds_tests::a_month_long_period_that_just_closed_is_ordinary_live_consumption`.
    assert!(
        enforce_covered_period_bounds(
            &default_bounds(),
            RecordOrigin::Live,
            now(),
            ending(Duration::minutes(-1)),
        )
        .is_ok(),
    );
}

#[test]
fn the_backfill_route_admits_what_the_live_past_tolerance_rejects() {
    let a_year_ago = ending(Duration::days(-365));
    let live =
        enforce_covered_period_bounds(&default_bounds(), RecordOrigin::Live, now(), a_year_ago)
            .expect_err("a year back is beyond the live past tolerance");
    assert_eq!(*reason_of(&live), ValidationReason::PastWindow);
    assert!(
        enforce_covered_period_bounds(
            &default_bounds(),
            RecordOrigin::Backfill,
            now(),
            a_year_ago,
        )
        .is_ok(),
        "the route exists for exactly the periods the past bound rejects, \
         and reaching past its own window is an authorization question \
         rather than an admission one",
    );
}

#[test]
fn the_backfill_route_still_rejects_a_period_ending_in_the_future() {
    // The route differs from POST /records in four respects and the future
    // bound is not one of them. Lifting it here would let a defective
    // emitter open a period that does not yet exist, on a route whose whole
    // purpose is history.
    let err = enforce_covered_period_bounds(
        &default_bounds(),
        RecordOrigin::Backfill,
        now(),
        ending(Duration::hours(1)),
    )
    .expect_err("the future bound governs both routes");
    assert_eq!(*reason_of(&err), ValidationReason::FutureWindow);
}
