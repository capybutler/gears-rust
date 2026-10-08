//! Unit tests for the ingestion path's covered-period bounds.
//!
//! Pure: no `Service`, no plugin, no clock. Almost every offset below is
//! unambiguously inside or outside its bound — one hour against five
//! minutes, 72 hours against 48 — because pinning a comparison's
//! strictness where no document fixes it turns a free implementation
//! choice into a test that has to be edited to change it.
//!
//! The backfill window is the one exception, and it is exact rather than
//! generous on purpose. `cpt-cf-usage-collector-adr-backfill-isolation`
//! fixes that strictness: an entry "ending further back than the
//! configured backfill window" is the elevated case, so the boundary
//! itself is NOT. Nothing races here either — [`now`] is a fixed instant
//! these functions take as a parameter, so the boundary case is as
//! deterministic as every other one.

use time::{Duration, OffsetDateTime};
use usage_collector_sdk::{RecordOrigin, UsageCollectorError, ValidationReason};

use super::{CoveredPeriodBounds, enforce_covered_period_bounds, ingestion_action};
use crate::domain::authz::usage_record::actions;

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
    // The route differs from POST /records in the four respects the
    // contract enumerates — the registered description names the three of
    // them the gear implements — and the future bound is not one of
    // either list. Lifting it here would let a defective emitter open a
    // period that does not yet exist, on a route whose whole purpose is
    // history.
    let err = enforce_covered_period_bounds(
        &default_bounds(),
        RecordOrigin::Backfill,
        now(),
        ending(Duration::hours(1)),
    )
    .expect_err("the future bound governs both routes");
    assert_eq!(*reason_of(&err), ValidationReason::FutureWindow);
}

#[test]
fn the_action_is_create_inside_the_backfill_window_and_backfill_beyond_it() {
    // The elevated action exists FOR the beyond-window case. Inside the
    // window a backfilled entry needs no grant a live one does not — the
    // routes differ by which bounds apply and by the origin they stamp,
    // not by who may call them.
    assert_eq!(
        ingestion_action(
            &default_bounds(),
            RecordOrigin::Backfill,
            now(),
            ending(Duration::days(-30)),
        ),
        actions::CREATE,
        "30 days back is well inside the 90-day window",
    );
    assert_eq!(
        ingestion_action(
            &default_bounds(),
            RecordOrigin::Backfill,
            now(),
            ending(Duration::days(-120)),
        ),
        actions::BACKFILL,
        "120 days back reaches past the 90-day window, which is the whole \
         reason the elevated action exists",
    );
}

#[test]
fn the_backfill_window_boundary_itself_is_not_the_elevated_case() {
    // The ADR grants the elevated action to a period ending "further back
    // than the configured backfill window", which makes the comparison
    // strict and the boundary ordinary. The sibling test above straddles
    // the window at 30 and 120 days and cannot tell `>` from `>=`; this is
    // the case that can, and `>=` is the likelier typo of the two.
    //
    // Exact, not approximate: `now` is a parameter, so
    // `now - window_end == backfill_window` holds to the nanosecond and
    // nothing here reads a clock.
    let bounds = default_bounds();
    let window_end = ending(-bounds.backfill_window);
    assert_eq!(
        now() - window_end,
        bounds.backfill_window,
        "the fixture must sit exactly ON the bound, or it pins nothing",
    );
    assert_eq!(
        ingestion_action(&bounds, RecordOrigin::Backfill, now(), window_end),
        actions::CREATE,
        "an entry ending exactly at the window has not reached PAST it, so \
         it needs no grant an ordinary emission does not",
    );
}

#[test]
fn the_live_path_authorizes_create_whatever_the_backfill_window_says() {
    // The live path does not read `backfill_window` at all. Deriving that
    // from the arithmetic — a live-admitted entry is inside 48 hours, so it
    // is inside 90 days too — would happen to hold, because
    // `config::validate` refuses a window narrower than the past tolerance.
    // But that invariant lives on `UsageCollectorConfig`, and this function
    // takes a `CoveredPeriodBounds` any caller can construct directly, so
    // the derivation would depend on a config-validation invariant this
    // type does not carry. Hence an input the live admission bound would
    // itself reject: `ingestion_action` must answer it anyway.
    assert_eq!(
        ingestion_action(
            &default_bounds(),
            RecordOrigin::Live,
            now(),
            ending(Duration::days(-365)),
        ),
        actions::CREATE,
    );
}
