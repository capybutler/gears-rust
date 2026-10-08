//! Unit tests for the bounded retry: the combinator's mechanics, the predicate
//! that decides what is worth re-running, and the jittered schedule. Pure (no
//! DB) throughout — the `should_retry` predicate and a zero backoff are
//! injected so each case is deterministic and instant.
//!
//! The combinator wraps the whole `create_batch_inner` call so a rare deadlock
//! victim (`40P01` → outer `Transient`) self-heals. That it is wired in with
//! *these* parameters, and that a self-healed transient moves the retry counter,
//! is `super::record_store`'s to assert; what is here is the machinery.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use usage_collector_sdk::UsageCollectorPluginError;

use super::{
    MAX_BATCH_ATTEMPTS, batch_retry_backoff, batch_retry_backoff_base, is_retryable_batch_error,
    with_retry,
};
use crate::infra::storage::test_support::unit_record;

/// Backoff fed to the combinator in tests: never actually sleep.
fn no_backoff(_attempt: u32) -> Duration {
    Duration::ZERO
}

#[tokio::test]
async fn with_retry_calls_operation_once_on_immediate_success() {
    let calls = AtomicU32::new(0);
    let retries = AtomicU32::new(0);
    let result: Result<u32, u32> = with_retry(
        3,
        no_backoff,
        |_err| true, // would retry, but the operation succeeds first try
        |_attempt, _err| {
            retries.fetch_add(1, Ordering::SeqCst);
        },
        || {
            calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok::<u32, u32>(7))
        },
    )
    .await;

    assert_eq!(result, Ok(7), "first-try success is returned verbatim");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "success on the first attempt runs the operation exactly once"
    );
    assert_eq!(
        retries.load(Ordering::SeqCst),
        0,
        "on_retry must not fire when the first attempt succeeds (no false retry signal)"
    );
}

#[tokio::test]
async fn with_retry_retries_a_retryable_error_then_returns_the_eventual_ok() {
    let calls = AtomicU32::new(0);
    // Fail (retryably) twice, then succeed on the third attempt.
    let result: Result<u32, u32> = with_retry(
        5,
        no_backoff,
        |_err| true,
        |_attempt, _err| {},
        || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            std::future::ready(if n < 3 { Err(n) } else { Ok(n) })
        },
    )
    .await;

    assert_eq!(result, Ok(3), "the eventual Ok is returned");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "two retryable failures then success -> N+1 = 3 calls"
    );
}

#[tokio::test]
async fn with_retry_invokes_on_retry_once_before_each_retry() {
    let retries = std::sync::Mutex::new(Vec::<u32>::new());
    // Fail (retryably) twice, then succeed on the third attempt.
    let calls = AtomicU32::new(0);
    let result: Result<u32, u32> = with_retry(
        5,
        no_backoff,
        |_err| true,
        |attempt, err| retries.lock().unwrap().push(attempt * 10 + *err),
        || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            std::future::ready(if n < 3 { Err(n) } else { Ok(n) })
        },
    )
    .await;

    assert_eq!(result, Ok(3), "the eventual Ok is returned");
    // on_retry fires once per retry, receiving the failed 1-based attempt number
    // and the error it failed with (encoded here as attempt*10 + err: attempt 1
    // failed with err 1 -> 11; attempt 2 failed with err 2 -> 22).
    assert_eq!(
        *retries.lock().unwrap(),
        vec![11, 22],
        "on_retry fires before each retry with the failed attempt number and error"
    );
}

#[tokio::test]
async fn with_retry_stops_at_max_attempts_and_returns_the_last_error() {
    let calls = AtomicU32::new(0);
    let result: Result<u32, u32> = with_retry(
        3,
        no_backoff,
        |_err| true, // always retryable, but the cap bounds it
        |_attempt, _err| {},
        || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            std::future::ready(Err::<u32, u32>(n))
        },
    )
    .await;

    assert_eq!(result, Err(3), "the last error is returned unchanged");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "a forever-retryable error runs exactly max_attempts times"
    );
}

#[tokio::test]
async fn with_retry_does_not_retry_a_non_retryable_error() {
    let calls = AtomicU32::new(0);
    let retries = AtomicU32::new(0);
    let result: Result<u32, u32> = with_retry(
        3,
        no_backoff,
        |_err| false, // predicate rejects every error → no retry
        |_attempt, _err| {
            retries.fetch_add(1, Ordering::SeqCst);
        },
        || {
            calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Err::<u32, u32>(42))
        },
    )
    .await;

    assert_eq!(result, Err(42), "the non-retryable error is returned as-is");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a non-retryable error returns after a single attempt"
    );
    assert_eq!(
        retries.load(Ordering::SeqCst),
        0,
        "on_retry must not fire when the error is not retried"
    );
}

// --- The `create_batch` retry predicate ---

#[test]
fn batch_retry_predicate_retries_only_transient() {
    // Transient (the deadlock victim, serialization failure, connection blip
    // all collapse to this) → retry.
    assert!(
        is_retryable_batch_error(&UsageCollectorPluginError::transient(
            "deadlock victim; whole txn rolled back"
        )),
        "an outer Transient must be retried"
    );

    // Non-retryable buckets → no retry.
    assert!(
        !is_retryable_batch_error(&UsageCollectorPluginError::internal("invariant break")),
        "Internal must not be retried"
    );
    assert!(
        !is_retryable_batch_error(&UsageCollectorPluginError::idempotency_conflict(
            "k",
            unit_record(uuid::Uuid::from_u128(1), "k", 1)
        )),
        "IdempotencyConflict must not be retried"
    );
}

// --- The `create_batch` backoff schedule ---

#[test]
fn batch_retry_backoff_base_is_short_and_non_decreasing() {
    // The deterministic pre-jitter schedule: a deadlock victim can retry almost
    // immediately, so it stays small and never shrinks between attempts.
    let mut prev = Duration::ZERO;
    for attempt in 1..MAX_BATCH_ATTEMPTS {
        let d = batch_retry_backoff_base(attempt);
        assert!(
            d >= prev,
            "base backoff must not decrease (attempt {attempt})"
        );
        assert!(
            d <= Duration::from_millis(100),
            "base backoff stays small for a deadlock victim (attempt {attempt}): {d:?}"
        );
        prev = d;
    }
}

#[test]
fn batch_retry_backoff_applies_full_jitter_within_base() {
    // Full jitter: every sampled backoff lands in `[0, base]`, so batches that
    // deadlocked together spread across the window instead of retrying in
    // lockstep (thundering herd). Sampled repeatedly since the value is random
    // per call; the run must also observe at least one sub-base value, proving
    // the jitter is actually applied and not a no-op.
    for attempt in 1..MAX_BATCH_ATTEMPTS {
        let base = batch_retry_backoff_base(attempt);
        let mut seen_below_base = false;
        for _ in 0..1_000 {
            let d = batch_retry_backoff(attempt);
            assert!(
                d <= base,
                "jittered backoff must not exceed its base (attempt {attempt}): {d:?} > {base:?}"
            );
            if d < base {
                seen_below_base = true;
            }
        }
        assert!(
            seen_below_base,
            "full jitter must sometimes back off less than the base (attempt {attempt})"
        );
    }
}
