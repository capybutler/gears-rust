//! Per-subject ingestion admission control (DESIGN §3.2).
//!
//! One token bucket per calling subject, keyed on `subject_id` from the
//! ingestion security context — the calling subject's total across all
//! tenants. **Not** the attributed subject on the record, which varies per
//! entry; see DESIGN §3.1 (Domain Model), "Attribution independence", which
//! §3.2 cites for exactly this distinction.
//!
//! There is deliberately no per-tenant tier, no external dependency, and
//! therefore no fail-open/fail-closed question: state is per-replica and in
//! memory, so the effective limit for `N` replicas is `N` times the configured
//! value and the limit is approximate by design.
//!
//! Time enters as a parameter, never as a call. This mirrors
//! `domain/covered_period.rs`, whose module doc gives the reason, and it is
//! what makes idle eviction testable without a 900-second sleep.

use std::collections::HashMap;

use time::{Duration, OffsetDateTime};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::config::IngestionQuotaConfig;
use crate::domain::covered_period::seconds;

/// How often the idle sweep is allowed to run, at most.
///
/// See [`IngestionQuota::sweep_if_due`] for why the sweep is amortised at all.
/// One second is not an arbitrary budget: refill is accounted in whole seconds,
/// so a bucket's token count changes only on a second boundary, and a sweep
/// running more often can at best evict a bucket under a second earlier.
const SWEEP_MIN_INTERVAL: Duration = Duration::seconds(1);

/// One subject's allowance.
// @cpt-state:cpt-cf-usage-collector-state-quota-bucket:p2
#[derive(Debug)]
struct Bucket {
    /// Tokens available as of `last_refill`.
    tokens: u64,
    /// The instant `tokens` was accounted to.
    ///
    /// Advanced by the **whole seconds** a refill consumed rather than set to
    /// `now`, so a caller charging more often than once a second does not
    /// forfeit the sub-second remainder on every call and starve its own
    /// refill. It therefore also never moves backwards when `now` does.
    last_refill: OffsetDateTime,
}

/// A refused submission and the delay after which one of that size fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaRejection {
    /// The configured per-subject allowance (`burst_entries`), for the
    /// operator-facing detail — not the tokens that happened to be left.
    pub allowance: u64,
    /// Seconds until a submission of this size would fit.
    pub retry_after_seconds: u64,
}

/// Everything behind the one lock: the buckets, and when they were last
/// swept. `last_sweep` shares the bucket map's lock because the sweep reads
/// and writes the map, so a separate lock would buy nothing and could let two
/// charges agree that neither owes a sweep.
#[derive(Debug)]
struct State {
    buckets: HashMap<Uuid, Bucket>,
    /// `None` until the first charge.
    last_sweep: Option<OffsetDateTime>,
}

/// Per-subject ingestion token buckets, charged the submitted entry count.
///
/// Keyed map over `tokio::sync::RwLock<HashMap<..>>`, following the gear's
/// existing precedent in `domain::type_resolver`; this adds no concurrency
/// dependency of its own.
// @cpt-dod:cpt-cf-usage-collector-dod-quota-per-replica-state:p2
#[derive(Debug)]
pub struct IngestionQuota {
    config: IngestionQuotaConfig,
    state: RwLock<State>,
}

impl IngestionQuota {
    /// Creates an empty set of buckets governed by `config`.
    #[must_use]
    pub fn new(config: IngestionQuotaConfig) -> Self {
        Self {
            config,
            state: RwLock::new(State {
                buckets: HashMap::new(),
                last_sweep: None,
            }),
        }
    }

    /// Charges `subject` `cost` entries as of `now`.
    ///
    /// `cost` is the submitted entry count, not one: a per-request cost would let
    /// a caller take `max_batch_records` times its budget by batching (DESIGN
    /// §3.2). The entry cap runs before this, which is what bounds `cost` at
    /// `max_batch_records` and — with the startup rule that
    /// `burst_entries >= max_batch_records` — what keeps the rejection's retry
    /// delay finite.
    ///
    /// The bucket's refill is accounted even when the charge is refused, so a
    /// refused caller is not charged twice for the same elapsed time.
    ///
    /// # Errors
    ///
    /// Returns [`QuotaRejection`] when the subject's bucket holds fewer than
    /// `cost` tokens. The submission is refused whole; nothing is deducted.
    ///
    /// # Divergence from the feature document
    ///
    /// `features/rate-limiting-reconciliation.md`'s step 9.1
    /// (`inst-charge-short-return`) asks this routine to "**RETURN** the
    /// exhaustion verdict, carrying the cost and the bucket's current level, with
    /// the bucket unchanged", and
    /// `cpt-cf-usage-collector-algo-quota-throttle-outcome`'s declared Input names
    /// the same quantities. This code carries neither and does not leave the
    /// bucket unchanged: [`QuotaRejection`] carries the *configured* `allowance`
    /// and a finite `retry_after_seconds`, and `apply_refill` runs before the
    /// rejection branch below.
    ///
    /// Both identifiers are nonetheless ticked, under **ruling G27**. The document
    /// describes a two-stage decomposition; this code fuses the two stages,
    /// consuming both quantities in place under the lock as
    /// `deficit = cost - tokens` rather than carrying them across a boundary. The
    /// observable contract — a 429 carrying a correct retry delay and the
    /// configured allowance — is identical, so no conformant caller can tell the
    /// two shapes apart.
    ///
    /// The "with the bucket unchanged" clause is the **document's** defect:
    /// honouring it literally means discarding the refill accounting on a refused
    /// charge, which charges a refused caller twice for the same elapsed time.
    /// Amending step 9.1 is an owner's document edit, deliberately not made here.
    // @cpt-algo:cpt-cf-usage-collector-algo-quota-charge:p2
    pub async fn charge(
        &self,
        subject: Uuid,
        cost: u64,
        now: OffsetDateTime,
    ) -> Result<(), QuotaRejection> {
        let mut state = self.state.write().await;
        self.sweep_if_due(&mut state, now);

        let burst = self.config.burst_entries;
        // A subject not currently resident starts full. That is the same
        // state eviction guarantees it would have been in: a bucket is only
        // evicted once it has refilled to capacity.
        let bucket = state.buckets.entry(subject).or_insert(Bucket {
            tokens: burst,
            last_refill: now,
        });
        self.apply_refill(bucket, now);

        if bucket.tokens >= cost {
            bucket.tokens = bucket.tokens.saturating_sub(cost);
            return Ok(());
        }

        // The delay is derived, not stored: with `deficit` tokens missing and
        // a refill rate of `r` per second, a submission of this size fits
        // after `deficit / r` seconds, rounded up — rounding down would name
        // an instant at which the submission is still short.
        let deficit = cost.saturating_sub(bucket.tokens);
        // `sustained_entries_per_sec` is non-zero by startup validation, so
        // `max(1)` is a division guard rather than a policy for a zero rate:
        // a bucket that never refills admits this submission at no delay
        // whatever, so every finite number it could report is equally
        // arbitrary and the one that matters is that it does not panic.
        let rate = self.config.sustained_entries_per_sec.max(1);
        Err(QuotaRejection {
            allowance: burst,
            retry_after_seconds: deficit.div_ceil(rate),
        })
    }

    /// How many buckets are resident, for `uc_ingestion_quota_buckets_active`.
    // @cpt-algo:cpt-cf-usage-collector-algo-quota-bucket-maintenance:p2
    pub async fn active_buckets(&self) -> usize {
        self.state.read().await.buckets.len()
    }

    /// Whether `subject` currently has a bucket.
    ///
    /// The eviction oracle a count cannot provide: a count alone cannot tell
    /// "one evicted, one inserted" from "both resident".
    pub async fn contains_bucket(&self, subject: Uuid) -> bool {
        self.state.read().await.buckets.contains_key(&subject)
    }

    /// Tokens `bucket` would hold at `now`, capped at capacity.
    fn refilled(&self, bucket: &Bucket, now: OffsetDateTime) -> u64 {
        let elapsed = Self::elapsed_whole_seconds(bucket.last_refill, now);
        // A long idle gap times the rate overflows u64 long before it means
        // anything: saturate at capacity, which is where it would land
        // anyway.
        let gained = elapsed.saturating_mul(self.config.sustained_entries_per_sec);
        bucket
            .tokens
            .saturating_add(gained)
            .min(self.config.burst_entries)
    }

    /// Brings `bucket` up to date as of `now`.
    fn apply_refill(&self, bucket: &mut Bucket, now: OffsetDateTime) {
        let elapsed = Self::elapsed_whole_seconds(bucket.last_refill, now);
        bucket.tokens = self.refilled(bucket, now);
        // Advance by what was consumed, never to `now`: the sub-second
        // remainder stays owed to the caller, and a `now` behind
        // `last_refill` (elapsed 0) leaves the instant where it was rather
        // than banking the rewound interval for a second refill.
        let consumed = Duration::seconds(seconds(elapsed));
        bucket.last_refill = bucket.last_refill.saturating_add(consumed);
    }

    /// Whole seconds from `from` to `to`, zero if `to` is not after `from`.
    ///
    /// A backwards clock step (an NTP correction) yields a negative duration.
    /// Treat it as zero elapsed: never a panic, never free tokens.
    fn elapsed_whole_seconds(from: OffsetDateTime, to: OffsetDateTime) -> u64 {
        u64::try_from((to - from).whole_seconds()).unwrap_or(0)
    }

    /// Drops every bucket that is both fully refilled and idle past the
    /// configured window — at most once per [`SWEEP_MIN_INTERVAL`].
    ///
    /// **The sweep visits the whole map, not the caller's own entry.** The growth
    /// case eviction exists for is a one-shot flood from many distinct subjects:
    /// those subjects never charge again, so a self-only sweep would never revisit
    /// any of them and all `N` buckets would stay resident forever, with
    /// `uc_ingestion_quota_buckets_active` climbing without bound.
    ///
    /// A whole-map sweep on *every* charge makes that same flood quadratic — each
    /// of the `N` charges rescanning the `N` entries the flood has already
    /// inserted, under the one write lock. Throttling to one sweep per second
    /// amortises it to `O(buckets)` per second while bounding residency at the
    /// idle window plus that second, because no sweep at any frequency may evict a
    /// bucket before its idle window has elapsed.
    ///
    /// **The throttle is non-monotonic-safe.** `now` is caller-supplied and this
    /// module's threat model already includes an NTP correction, so a `now` behind
    /// `last_sweep` is treated as *due* rather than compared. Comparing would
    /// suspend eviction until the clock climbed back past the stamp — a forward
    /// jump of an hour, corrected an instant later, would buy an hour of unswept
    /// growth. Being due re-anchors `last_sweep` at the corrected instant, which
    /// costs one extra sweep.
    ///
    /// The throttle is not a liveness guarantee on its own: a replica that stops
    /// being charged stops sweeping. That is already true of any sweep driven by
    /// the charge path, which is the only path this gear has — `now` is a
    /// parameter here, not a clock to run a timer off.
    // @cpt-algo:cpt-cf-usage-collector-algo-quota-bucket-maintenance:p2
    fn sweep_if_due(&self, state: &mut State, now: OffsetDateTime) {
        let due = state
            .last_sweep
            .is_none_or(|last| now < last || now - last >= SWEEP_MIN_INTERVAL);
        if !due {
            return;
        }
        state.last_sweep = Some(now);

        let idle_window = Duration::seconds(seconds(self.config.idle_eviction_secs));
        let capacity = self.config.burst_entries;
        state.buckets.retain(|_, bucket| {
            let idle_for = now - bucket.last_refill;
            // Fully refilled *and* idle. Dropping a partially drained bucket
            // would hand back the allowance it had spent, which is what
            // DESIGN §3.2 forbids: the subject would reappear with a full
            // bucket purely by going quiet.
            idle_for < idle_window || self.refilled(bucket, now) < capacity
        });
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "quota_tests.rs"]
mod quota_tests;
