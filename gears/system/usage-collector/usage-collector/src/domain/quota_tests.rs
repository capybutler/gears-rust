//! Unit tests for the per-subject ingestion token bucket.
//!
//! Pure: no `Service`, no plugin, no sleep. Every instant below is passed
//! in, which is what DESIGN §3.12.1's testing-strategy table asks for in its
//! Unit row ("Quota bucket arithmetic on the injected clock") and what lets
//! the 900-second idle window be exercised in microseconds.
//!
//! The bucket is constructed here directly from an
//! [`IngestionQuotaConfig`], bypassing [`UsageCollectorConfig::validate`] —
//! several cases below need a degenerate configuration (a zero refill rate,
//! a rate near `u64::MAX`) that startup refuses. That is deliberate: the
//! arithmetic must not panic on a value the type admits, whether or not
//! bootstrap would have let it through.
//!
//! [`UsageCollectorConfig::validate`]: crate::config::UsageCollectorConfig::validate

use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::IngestionQuota;
use crate::config::IngestionQuotaConfig;

/// A fixed instant, supplied rather than read from the clock, so every
/// charge of a test is judged against one `now`. `2026-01-01T00:00:00Z`.
///
/// A function rather than a `const`: `time`'s `datetime!` macro needs the
/// crate's `macros` feature, which this gear does not enable, and the gear
/// already builds its test instants this way
/// (`domain/covered_period_tests.rs`).
fn t0() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_767_225_600).expect("valid instant")
}

/// `u64::MAX / 2`, spelled as a shift because `clippy::integer_division` is
/// denied workspace-wide. A rate this large overflows `elapsed * rate` for
/// any gap longer than two seconds.
const HALF_MAX_RATE: u64 = u64::MAX >> 1;

/// A bucket of capacity `burst` refilling at `refill` entries per second,
/// with the published 900-second idle window.
fn quota(burst: u64, refill: u64) -> IngestionQuota {
    IngestionQuota::new(IngestionQuotaConfig {
        sustained_entries_per_sec: refill,
        burst_entries: burst,
        idle_eviction_secs: 900,
    })
}

#[tokio::test]
async fn cost_is_the_entry_count_not_one() {
    // DESIGN §3.2: "A per-request cost of one would let a caller take
    // max_batch_records times its budget by batching."
    // Red against a bucket that charges 1 per call: the third charge would
    // succeed.
    let q = quota(20, 1);
    let s = Uuid::new_v4();
    q.charge(s, 10, t0()).await.expect("first 10 fit");
    q.charge(s, 10, t0())
        .await
        .expect("second 10 exhausts the bucket");
    q.charge(s, 1, t0())
        .await
        .expect_err("nothing left for even one");
}

#[tokio::test]
async fn two_subjects_do_not_share_a_bucket() {
    // Guards the key: a bucket keyed on anything constant across subjects
    // (or on a per-route value) makes the second charge fail.
    let q = quota(10, 1);
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    assert_ne!(a, b, "the oracle rests on these differing");
    q.charge(a, 10, t0())
        .await
        .expect("a exhausts its own bucket");
    q.charge(b, 10, t0())
        .await
        .expect("b has its own, untouched");
}

#[tokio::test]
async fn tokens_refill_at_the_sustained_rate_on_the_passed_in_now() {
    // No sleep: the whole point of the passed-in `now` (DESIGN §3.12.1's
    // "Quota bucket arithmetic on the injected clock").
    // Red against a bucket that refills to full on any elapsed time.
    let q = quota(100, 10); // 10 entries/sec
    let s = Uuid::new_v4();
    q.charge(s, 100, t0()).await.expect("drain it");
    q.charge(s, 1, t0()).await.expect_err("empty at T0");

    q.charge(s, 10, t0() + Duration::seconds(1))
        .await
        .expect("one second refills exactly 10");
    q.charge(s, 1, t0() + Duration::seconds(1))
        .await
        .expect_err("and not 11");
}

#[tokio::test]
async fn a_sub_second_charge_does_not_forfeit_the_accumulated_remainder() {
    // Red against advancing `last_refill` to `now` on every charge: a
    // caller charging more often than once a second would then truncate
    // every gap to zero whole seconds and never refill at all, which is a
    // throttle the configuration does not describe.
    let q = quota(100, 10); // 10 entries/sec
    let s = Uuid::new_v4();
    q.charge(s, 100, t0()).await.expect("drain it");
    for tenth in 1..=9 {
        q.charge(s, 1, t0() + Duration::milliseconds(tenth * 100))
            .await
            .expect_err("still empty inside the first second");
    }
    q.charge(s, 10, t0() + Duration::seconds(1))
        .await
        .expect("the nine sub-second probes consumed none of the elapsed second");
}

#[tokio::test]
async fn a_backwards_clock_step_neither_panics_nor_grants_free_tokens() {
    // Review Focus 1. An NTP correction can move `now` behind
    // `last_refill`. Red against `(now - last_refill).whole_seconds() as
    // u64`, which wraps a negative to an enormous positive and refills the
    // bucket to full.
    let q = quota(10, 1_000);
    let s = Uuid::new_v4();
    q.charge(s, 10, t0()).await.expect("drain it");
    q.charge(s, 1, t0() - Duration::hours(1))
        .await
        .expect_err("a clock that went backwards grants nothing");
}

#[tokio::test]
async fn a_backwards_clock_step_does_not_bank_the_time_it_skipped() {
    // The companion to the test above, and the one that fails if the
    // backwards step is absorbed by moving `last_refill` back to `now`:
    // the hour it rewound would then be refilled a second time, and the
    // charge at T0 — zero seconds after the bucket was drained — would be
    // admitted.
    let q = quota(10, 1_000);
    let s = Uuid::new_v4();
    q.charge(s, 10, t0()).await.expect("drain it");
    q.charge(s, 1, t0() - Duration::hours(1))
        .await
        .expect_err("a clock that went backwards grants nothing");
    q.charge(s, 1, t0())
        .await
        .expect_err("and the rewound hour is not refilled on the way forward");
}

#[tokio::test]
async fn a_long_idle_gap_saturates_at_capacity_rather_than_wrapping() {
    // Review Focus 4. elapsed * rate overflows u64 for a large enough gap;
    // wrapping would leave a tiny token count and throttle a legitimate
    // caller. Red against a plain `*`, which panics in debug and wraps in
    // release.
    let q = quota(100, HALF_MAX_RATE);
    let s = Uuid::new_v4();
    q.charge(s, 100, t0()).await.expect("drain it");
    q.charge(s, 100, t0() + Duration::days(3650))
        .await
        .expect("a decade later the bucket is full, not wrapped");
}

#[tokio::test]
async fn a_partially_drained_idle_bucket_is_not_evicted() {
    // DESIGN §3.2: "evicting a partially drained bucket would return free
    // allowance." Red against an eviction keyed on idleness alone.
    let q = quota(100, 0); // zero refill: drained stays drained
    let s = Uuid::new_v4();
    q.charge(s, 60, t0()).await.expect("drain 60 of 100");

    let later = t0() + Duration::seconds(10_000);
    let other = Uuid::new_v4();
    assert_ne!(s, other, "the oracle rests on these differing");
    q.charge(other, 1, later).await.expect("drives the sweep");

    // If `s` had been evicted it would return full and admit 100.
    q.charge(s, 41, later).await.expect_err("only 40 remained");
}

#[tokio::test]
async fn a_fully_refilled_idle_bucket_is_evicted() {
    // Red against an implementation that never evicts, and — observed —
    // against a sweep that visits only the caller's own entry, since `s` is
    // not the caller here. It is a one-bucket pin and discriminates no
    // further than that: it cannot tell a sweep that reaches *one* other
    // bucket from one that reaches every other bucket, which is what
    // `the_sweep_reaches_every_idle_bucket_not_only_the_one_charged` is for.
    let q = quota(100, 100);
    let s = Uuid::new_v4();
    q.charge(s, 100, t0()).await.expect("drain it");
    assert_eq!(q.active_buckets().await, 1);

    let later = t0() + Duration::seconds(10_000);
    let other = Uuid::new_v4();
    assert_ne!(s, other, "the oracle rests on these differing");
    q.charge(other, 1, later).await.expect("drives the sweep");
    // `s` refilled to capacity long ago and has been idle past the window.
    assert!(
        !q.contains_bucket(s).await,
        "a fully refilled idle bucket is evicted"
    );
}

#[tokio::test]
async fn a_rewound_clock_does_not_suspend_the_sweep() {
    // Red against a throttle that only asks `now - last_sweep >=
    // SWEEP_MIN_INTERVAL`: an NTP correction makes that difference negative,
    // so nothing comes due until the clock has climbed back past the stamp
    // and `stale` survives the charge that should have evicted it. The
    // refill path hardens against exactly this step
    // (`elapsed_whole_seconds`); the throttle has to as well.
    let q = quota(100, 100);

    // A forward jump stamps `last_sweep` two hours ahead of everything that
    // follows it.
    let early = Uuid::new_v4();
    q.charge(early, 1, t0() + Duration::hours(2))
        .await
        .expect("stamps last_sweep in the future");

    // The clock is corrected backwards; this bucket then goes idle.
    let stale = Uuid::new_v4();
    assert_ne!(early, stale, "the oracle rests on these differing");
    q.charge(stale, 1, t0())
        .await
        .expect("charged after the correction");

    // Past the idle window, but still far behind the stamped sweep.
    let live = Uuid::new_v4();
    q.charge(live, 1, t0() + Duration::seconds(1_000))
        .await
        .expect("drives the sweep");
    assert!(
        !q.contains_bucket(stale).await,
        "an idle bucket is evicted even with `last_sweep` ahead of `now`"
    );
    // `early` is not idle at this instant — its own charge is still in the
    // future — so the sweep that ran must have kept it.
    assert!(
        q.contains_bucket(early).await,
        "and a bucket is not idle yet"
    );
}

#[tokio::test]
async fn the_sweep_reaches_every_idle_bucket_not_only_the_one_charged() {
    // Review Focus 2. A one-shot flood from many distinct subjects is the
    // growth case the eviction exists for, and a sweep that only visited
    // the caller's own entry would leave every one of them resident
    // forever. Red against such a self-only sweep: `active_buckets` stays
    // at `FLOOD + 1` instead of falling to the single live caller.
    const FLOOD: usize = 64;
    let q = quota(100, 100);
    for _ in 0..FLOOD {
        q.charge(Uuid::new_v4(), 1, t0())
            .await
            .expect("each flood subject takes one token");
    }
    assert_eq!(q.active_buckets().await, FLOOD, "the flood is resident");

    let live = Uuid::new_v4();
    q.charge(live, 1, t0() + Duration::seconds(10_000))
        .await
        .expect("one charge, long after the flood went idle");
    assert_eq!(
        q.active_buckets().await,
        1,
        "every idle flood bucket is gone and only the live caller remains"
    );
    assert!(q.contains_bucket(live).await, "the live caller is the one");
}

#[tokio::test]
async fn a_rejection_reports_the_allowance_and_the_wait_that_covers_the_deficit() {
    // The two fields Task 4 forwards into `ResourceExhausted`. Red against
    // a rejection reporting the tokens left rather than the configured
    // allowance, and against a delay rounded down: 1 of the 25-token
    // deficit would still be missing after 2 seconds at 12 entries/sec.
    let q = quota(100, 12);
    let s = Uuid::new_v4();
    q.charge(s, 100, t0()).await.expect("drain it");
    // One second refills 12, so 12 of the 37 asked for are available.
    let at = t0() + Duration::seconds(1);
    let rejection = q.charge(s, 37, at).await.expect_err("25 short");
    assert_eq!(rejection.allowance, 100, "the configured allowance");
    assert_eq!(
        rejection.retry_after_seconds, 3,
        "25 tokens at 12/sec rounds up to 3 seconds, not down to 2"
    );
}

#[tokio::test]
async fn a_zero_refill_rate_rejects_without_dividing_by_it() {
    // Startup validation refuses a zero `sustained_entries_per_sec`, but
    // the type admits one and `deficit.div_ceil(rate)` panics on it. Red
    // against an unguarded `div_ceil`: this test aborts with "attempt to
    // divide by zero" instead of returning a rejection.
    let q = quota(100, 0);
    let s = Uuid::new_v4();
    q.charge(s, 100, t0()).await.expect("drain it");
    let rejection = q
        .charge(s, 1, t0() + Duration::seconds(10))
        .await
        .expect_err("a bucket that never refills admits nothing");
    assert_eq!(rejection.allowance, 100, "the configured allowance");
}
