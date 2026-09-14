use std::time::Duration;

use time::OffsetDateTime;

use super::{Decision, KeepReason, drop_decision};
use crate::domain::ports::RetentionError;

const DAY: u64 = 86_400;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("valid instant")
}

#[allow(clippy::unnecessary_wraps)]
fn days(n: u64) -> Result<Duration, RetentionError> {
    Ok(Duration::from_secs(n * DAY))
}

#[test]
fn a_chunk_past_its_types_retention_is_dropped() {
    let time_end = now() - time::Duration::days(100);
    assert_eq!(drop_decision(time_end, &[days(30)], now()), Decision::Drop);
}

#[test]
fn a_chunk_inside_its_types_retention_is_kept() {
    let time_end = now() - time::Duration::days(10);
    assert_eq!(
        drop_decision(time_end, &[days(30)], now()),
        Decision::Keep(KeepReason::NotExpired)
    );
}

#[test]
fn a_deadline_exactly_now_is_kept() {
    // The deadline is the first instant the retention no longer covers; the
    // chunk goes only once that instant is in the past.
    let time_end = now() - time::Duration::days(30);
    assert_eq!(
        drop_decision(time_end, &[days(30)], now()),
        Decision::Keep(KeepReason::NotExpired)
    );
}

#[test]
fn a_shared_chunk_is_held_to_its_longest_retention() {
    let time_end = now() - time::Duration::days(100);
    assert_eq!(
        drop_decision(time_end, &[days(30), days(400)], now()),
        Decision::Keep(KeepReason::NotExpired),
        "one type still inside its retention keeps the whole chunk"
    );
    assert_eq!(
        drop_decision(time_end, &[days(30), days(60)], now()),
        Decision::Drop
    );
}

#[test]
fn an_unresolved_type_keeps_a_chunk_its_neighbour_would_drop() {
    let time_end = now() - time::Duration::days(100);
    assert_eq!(
        drop_decision(
            time_end,
            &[
                days(1),
                Err(RetentionError::Unavailable("registry down".to_owned()))
            ],
            now()
        ),
        Decision::Keep(KeepReason::Unavailable),
        "never drop without a definite retention for every type in the chunk"
    );
}

#[test]
fn a_chunk_no_type_maps_to_is_kept() {
    let time_end = now() - time::Duration::days(1_000);
    assert_eq!(
        drop_decision(time_end, &[], now()),
        Decision::Keep(KeepReason::NoType)
    );
}

#[test]
fn each_resolution_failure_keeps_under_its_own_reason() {
    let time_end = now() - time::Duration::days(1_000);
    for (err, reason) in [
        (
            RetentionError::Unavailable("x".to_owned()),
            KeepReason::Unavailable,
        ),
        (RetentionError::NotFound, KeepReason::NotFound),
        (RetentionError::MissingTrait, KeepReason::MissingTrait),
        (
            RetentionError::InvalidTrait("x".to_owned()),
            KeepReason::InvalidTrait,
        ),
    ] {
        assert_eq!(
            drop_decision(time_end, &[Err(err)], now()),
            Decision::Keep(reason)
        );
    }
}

#[test]
fn a_retention_too_long_to_add_to_an_instant_has_not_expired() {
    let time_end = now() - time::Duration::days(1_000);
    assert_eq!(
        drop_decision(time_end, &[Ok(Duration::MAX)], now()),
        Decision::Keep(KeepReason::NotExpired)
    );
}

#[test]
fn only_not_expired_is_a_resolved_keep() {
    assert!(!KeepReason::NotExpired.is_unresolved());
    for reason in [
        KeepReason::NoType,
        KeepReason::Unavailable,
        KeepReason::NotFound,
        KeepReason::MissingTrait,
        KeepReason::InvalidTrait,
    ] {
        assert!(reason.is_unresolved(), "{reason:?}");
    }
}

#[test]
fn unresolved_reasons_carry_the_metric_label_values() {
    assert_eq!(KeepReason::Unavailable.as_label(), "unavailable");
    assert_eq!(KeepReason::NotFound.as_label(), "not_found");
    assert_eq!(KeepReason::MissingTrait.as_label(), "missing_trait");
    assert_eq!(KeepReason::InvalidTrait.as_label(), "invalid_trait");
    assert_eq!(KeepReason::NoType.as_label(), "no_type");
}
