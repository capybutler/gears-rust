//! The bounded, jittered retry the batch write runs under — the combinator,
//! its schedule, and the predicate that decides what is worth re-running.
//!
//! Pure and DB-free: nothing here opens a connection, issues a statement or
//! names a `sqlx` type. `with_retry` is a generic combinator over any
//! fallible future, and the observability seam is a caller-supplied `on_retry`
//! closure precisely so the log line and the counter stay out of it. The three
//! batch-shaped items beside it — `MAX_BATCH_ATTEMPTS`,
//! `batch_retry_backoff` and `is_retryable_batch_error` — are the policy
//! `create_batch` instantiates it with, kept here rather than beside the
//! transaction because they are the retry's own parameters and are tested
//! without one.
//!
//! What this module does **not** decide: whether a given `sqlx` failure is
//! transient at all. That classification is
//! [`crate::infra::storage::error`]'s, and the write path's one reclassification
//! — a PRIMARY KEY collision on a concurrent writer — is
//! [`crate::infra::storage::record_store::PgRecordStore`]'s, because only a
//! write path knows it is in that race.

use std::future::Future;
use std::time::Duration;

use rand::RngExt as _;

use usage_collector_sdk::UsageCollectorPluginError;

/// Total `create_batch` attempts: one initial try plus two retries. A bounded
/// in-process retry so a transient backend error self-heals transparently
/// instead of bubbling an `Err(Transient)` to the host. [`is_retryable_batch_error`]
/// is where the causes that reach this loop are set against the code.
pub const MAX_BATCH_ATTEMPTS: u32 = 3;

/// Deterministic pre-jitter backoff base for the `attempt`-th retry (1-based).
/// A short exponential — 5 ms, 10 ms, … — because a deadlock victim can retry
/// almost immediately: the surviving transaction has already committed or
/// aborted by the time Postgres aborts the victim, so the contended dedup locks
/// are free. The shift is saturated so the schedule cannot overflow however
/// [`MAX_BATCH_ATTEMPTS`] grows.
pub fn batch_retry_backoff_base(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(6);
    Duration::from_millis(5u64 << shift)
}

/// Full jitter: a uniformly random duration in `[0, upper]`. Decorrelates the
/// retry instants of batches that deadlocked on the same dedup locks, so they
/// do not all wake and re-contend at the same moment. An `upper` of at most
/// 1 ms is returned unchanged (nothing meaningful to spread).
fn full_jitter(upper: Duration) -> Duration {
    let upper_ms = u64::try_from(upper.as_millis()).unwrap_or(u64::MAX);
    if upper_ms <= 1 {
        return upper;
    }
    let jitter_ms = rand::rng().random_range(0..=upper_ms);
    Duration::from_millis(jitter_ms)
}

/// Backoff before the `attempt`-th retry (1-based: `batch_retry_backoff(1)`
/// precedes the first retry): the exponential [`batch_retry_backoff_base`] with
/// **full jitter** applied so concurrent deadlock victims spread across the
/// window instead of retrying in lockstep (thundering herd).
pub fn batch_retry_backoff(attempt: u32) -> Duration {
    full_jitter(batch_retry_backoff_base(attempt))
}

/// Retry predicate for [`with_retry`] around `create_batch`: retry **only** an
/// outer [`UsageCollectorPluginError::Transient`].
///
/// The deadlock victim surfaces as an outer `Transient`: `create_batch_inner`
/// runs the whole batch in one transaction, and that transaction rolled back, so
/// the attempt left nothing behind. Serialization failures (`40001`), lock
/// timeouts (`55P03`, including on the partition-key assignment
/// `create_batch_inner` runs per representative in autocommit) and connectivity
/// faults collapse to the same bucket inside the storage helpers. A concurrent
/// writer colliding on the ledger's PRIMARY KEY reaches it by a different route:
/// the arbiter does not cover that index, so `classify_db` leaves the `23505` in
/// `Other` and `record_insert_error` lifts it. Every one of these is **safe** to
/// re-run, this batch being idempotent — a weaker claim than that a re-run
/// succeeds, and the only one this predicate needs. `Internal`,
/// `IdempotencyConflict` and the other typed domain outcomes are non-retryable
/// and returned unchanged. Per-row `Transient` outcomes carried inside an
/// `Ok(vec)` are deliberately not seen here — the batch as a whole succeeded.
pub fn is_retryable_batch_error(err: &UsageCollectorPluginError) -> bool {
    matches!(err, UsageCollectorPluginError::Transient { .. })
}

/// Run `operation`, retrying while `should_retry` accepts its error, for up to
/// `max_attempts` total invocations; sleep `backoff(attempt)` before the
/// `attempt`-th retry. Returns the first `Ok`, or — once retries are exhausted
/// or the error is non-retryable — the last `Err` unchanged.
///
/// `on_retry(attempt, &err)` is invoked exactly once before each retry — after a
/// retryable failure and before the backoff sleep — with the failed 1-based
/// `attempt` number. It is the observability seam (log + retry counter), kept
/// out of this combinator so it stays generic and DB-free. It never fires on a
/// first-attempt success or on a returned (non-retried) error, so a retry can be
/// told apart from a bubbled transient failure.
///
/// `operation` is an `Fn` invoked fresh each attempt, which is the right unit of
/// retry for `create_batch_inner`: every attempt acquires a fresh connection and
/// opens a fresh transaction, so a failed attempt leaves no half-written batch
/// behind — and, the row's `xact_id` coming from the transaction that wrote it,
/// no attempt leaves a feed position a later one contradicts. On success the
/// loop neither sleeps, allocates a backoff, nor calls `on_retry`.
pub async fn with_retry<T, E, Op, Fut>(
    max_attempts: u32,
    backoff: impl Fn(u32) -> Duration,
    should_retry: impl Fn(&E) -> bool,
    on_retry: impl Fn(u32, &E),
    operation: Op,
) -> Result<T, E>
where
    Op: Fn() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempt: u32 = 1;
    loop {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                if attempt >= max_attempts || !should_retry(&err) {
                    return Err(err);
                }
                on_retry(attempt, &err);
                // `toolkit::tokio` is the crate's tokio re-export (matching
                // `toolkit::tokio::spawn` / `select!` elsewhere in this gear);
                // `tokio` itself is only a dev-dependency.
                toolkit::tokio::time::sleep(backoff(attempt)).await;
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "retry_tests.rs"]
mod retry_tests;
