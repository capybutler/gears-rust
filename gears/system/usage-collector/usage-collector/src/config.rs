//! Configuration for the usage-collector gear.
//!
//! Carries the storage-plugin vendor selector, the Type Resolver cache
//! knobs, the metadata size cap, and the covered-period bounds. Read once at
//! `Gear::init` via `ctx.config_or_default()`; changing the binding requires
//! a gear restart. No usage-type declaration is accepted here:
//! `types-registry` owns every declaration and this gear registers no
//! usage-type surface at all
//! (`cpt-cf-usage-collector-adr-registry-owned-typing`).

use serde::Deserialize;

/// Gear configuration for `[usage-collector]`.
///
/// Read once at `Gear::init` via `ctx.config_or_default()`; changing the
/// binding requires a gear restart.
// @cpt-dod:cpt-cf-usage-collector-dod-slo-threshold-provenance:p1
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UsageCollectorConfig {
    /// Vendor selector used to pick a storage-plugin implementation. The host
    /// queries types-registry for matching plugin instances and selects the
    /// lowest priority number — lazily, on the first dispatch, so no
    /// `types-registry` query happens at `init`.
    pub vendor: String,

    /// Operational-metrics configuration (`[usage_collector.metrics]`) — see
    /// [`MetricsConfig`].
    pub metrics: MetricsConfig,

    /// How long a resolved GTS type declaration is served before the Type
    /// Resolver refreshes it, in seconds.
    ///
    /// Fold, unit and metadata surface are immutable for a type's life, so
    /// this is not a correctness window for them. It bounds how long a
    /// withdrawn declaration keeps resolving, and keeps the cache honest once
    /// `types-registry` admits mutable major-only identifiers.
    pub type_cache_ttl_secs: u64,

    /// Ceiling on cached declarations. One entry per meter, not per entry,
    /// so realistic deployments sit far below the default.
    pub type_cache_capacity: usize,

    /// Cap on an entry's serialized metadata map, in bytes.
    ///
    /// DESIGN 3.1 makes the cap per deployment. It is enforced alongside the
    /// declared-shape check, not instead of it: a payload can sit inside the
    /// cap and still carry an undeclared key.
    ///
    /// Defaults to
    /// [`DEFAULT_METADATA_SIZE_CAP_BYTES`](crate::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES),
    /// the same constant `Service::new` hard-codes, so wiring this value
    /// through to `validate_submit_record_metadata` is a no-op for the default
    /// deployment rather than a silent tightening.
    pub metadata_size_cap_bytes: usize,

    /// How far into the future a covered period may end, in seconds.
    ///
    /// The live path rejects a period ending further ahead than this, and
    /// **so does the backfill route** — that route lifts the past bound only.
    /// The bound protects against a defective or clock-skewed emitter opening
    /// a period that does not yet exist
    /// (`cpt-cf-usage-collector-adr-backfill-isolation`).
    ///
    /// Defaults to the value
    /// `cpt-cf-usage-collector-fr-live-future-time-bound` publishes.
    pub live_future_tolerance_secs: u64,

    /// How far into the past a covered period may end on the **live** path,
    /// in seconds.
    ///
    /// A period ending further back is rejected with a message naming the
    /// backfill route, which is where such an entry belongs. The default
    /// covers emitter outage and retry lag; anything older is history.
    ///
    /// The bound belongs to the path, not to the entry kind, so it governs an
    /// invalidation over the period it copies exactly as it governs a
    /// measurement — a withdrawal of a closed month travels the backfill
    /// route.
    ///
    /// Defaults to 48 hours.
    pub live_past_tolerance_secs: u64,

    /// How far back the backfill route reaches without elevated
    /// authorization, in seconds.
    ///
    /// The route admits any period the future bound allows. This window
    /// decides only *which* PDP action each entry is authorized against:
    /// inside it, `create`; beyond it, the `backfill` action. It bounds the
    /// recomputation obligation a materialised aggregate carries.
    ///
    /// A deployment must not admit a window wider than the raw retention its
    /// storage profile guarantees for the target GTS type.
    ///
    /// Defaults to 90 days.
    ///
    /// This doc comment is the deployment contract statement of the retention
    /// floor (`cpt-cf-usage-collector-dod-retention-floor-formula`: "The
    /// system's deployment contract MUST state the retention floor as the
    /// configured backfill window plus one operational replay horizon"), a
    /// plugin-readiness condition surfaced at review rather than a gear-side
    /// sweep. The evaluation
    /// (`cpt-cf-usage-collector-algo-retention-floor-evaluation`) and its
    /// conformance lifecycle
    /// (`cpt-cf-usage-collector-state-retention-floor-conformance`) are, by
    /// those identifiers' own text, operator processes: neither this struct nor
    /// `UsageCollectorConfig::validate` reads a replay horizon or any GTS
    /// type's retention policy, and `backfill_window_secs` is read once at
    /// `Gear::init`.
    // @cpt-algo:cpt-cf-usage-collector-algo-retention-floor-evaluation:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-retention-floor-formula:p2
    // @cpt-state:cpt-cf-usage-collector-state-retention-floor-conformance:p2
    // @cpt-flow:cpt-cf-usage-collector-flow-backfill-widen-window:p2
    pub backfill_window_secs: u64,

    /// Per-request entry cap on both ingestion routes. An empty or over-cap
    /// submission is rejected whole, before any entry is validated.
    ///
    /// Defaults to
    /// [`DEFAULT_MAX_BATCH_RECORDS`](crate::domain::service::DEFAULT_MAX_BATCH_RECORDS).
    pub max_batch_records: usize,

    /// Per-subject ingestion allowance
    /// (`[usage_collector.ingestion_quota]`), DESIGN §3.8.
    pub ingestion_quota: IngestionQuotaConfig,

    /// The retry delay a `ServiceUnavailable` carries where nothing else
    /// supplied one, in seconds: a hintless plugin `Transient`, or a dispatch
    /// that reached no plugin at all. A plugin-supplied hint always wins over
    /// this default (DESIGN §3.8, §3.3 Error Contract).
    ///
    /// Defaults to [`DEFAULT_UNAVAILABLE_RETRY_AFTER_SECS`][default], the
    /// value DESIGN §3.8 publishes.
    ///
    /// [default]: crate::domain::service::DEFAULT_UNAVAILABLE_RETRY_AFTER_SECS
    pub unavailable_retry_after_secs: u64,

    /// The delay a `Conflict(TargetNotConverged)` carries, in seconds
    /// (DESIGN §3.8, §3.3 Error Contract). A deployment sets it from the
    /// active plugin's published convergence bound plus the query-path lag
    /// bound; the gear enforces neither directly, since both are
    /// documentation-only (§3.10) rather than a runtime-observable quantity.
    ///
    /// Defaults to
    /// [`DEFAULT_TARGET_NOT_CONVERGED_RETRY_AFTER_SECS`][default], the
    /// value DESIGN §3.8 publishes.
    ///
    /// [default]: crate::domain::service::DEFAULT_TARGET_NOT_CONVERGED_RETRY_AFTER_SECS
    pub target_not_converged_retry_after_secs: u64,
}

impl Default for UsageCollectorConfig {
    fn default() -> Self {
        Self {
            vendor: "constructorfabric".to_owned(),
            metrics: MetricsConfig::default(),
            type_cache_ttl_secs: 300,
            type_cache_capacity: 10_000,
            metadata_size_cap_bytes: crate::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES,
            live_future_tolerance_secs:
                crate::domain::covered_period::DEFAULT_LIVE_FUTURE_TOLERANCE_SECS,
            live_past_tolerance_secs:
                crate::domain::covered_period::DEFAULT_LIVE_PAST_TOLERANCE_SECS,
            backfill_window_secs: crate::domain::covered_period::DEFAULT_BACKFILL_WINDOW_SECS,
            max_batch_records: crate::domain::service::DEFAULT_MAX_BATCH_RECORDS,
            ingestion_quota: IngestionQuotaConfig::default(),
            unavailable_retry_after_secs:
                crate::domain::service::DEFAULT_UNAVAILABLE_RETRY_AFTER_SECS,
            target_not_converged_retry_after_secs:
                crate::domain::service::DEFAULT_TARGET_NOT_CONVERGED_RETRY_AFTER_SECS,
        }
    }
}

/// Per-subject ingestion allowance (`[usage_collector.ingestion_quota]`),
/// DESIGN §3.8. Every value is a positive count; see
/// [`UsageCollectorConfig::validate`].
///
/// The bucket these values configure
/// ([`domain::quota::IngestionQuota`](crate::domain::quota::IngestionQuota))
/// is per-replica and in memory, so the effective limit of an `N`-replica
/// deployment is `N` times what is set here.
// @cpt-flow:cpt-cf-usage-collector-flow-quota-apportion-replicas:p2
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IngestionQuotaConfig {
    /// Token refill rate for the calling subject across all tenants.
    /// Defaults to `2_000`.
    pub sustained_entries_per_sec: u64,
    /// Bucket capacity for the calling subject across all tenants. Must be at
    /// least `max_batch_records`. Defaults to `4_000`.
    pub burst_entries: u64,
    /// How long a **fully refilled** bucket is retained before eviction. A
    /// partially drained bucket is never evicted — evicting one would return
    /// free allowance. Defaults to `900`.
    pub idle_eviction_secs: u64,
}

impl Default for IngestionQuotaConfig {
    fn default() -> Self {
        Self {
            sustained_entries_per_sec: 2_000,
            burst_entries: 4_000,
            idle_eviction_secs: 900,
        }
    }
}

/// Operational-metrics configuration for `[usage_collector.metrics]`.
///
/// Carries the leading namespace segment of every instrument name (`uc_` by
/// default), which DESIGN §3.11.5 declares "substitutable at adapter
/// construction", and nothing else: the rest of the metrics pipeline (OTLP
/// export, cardinality limit, backend selection) is owned by `ToolKit`'s
/// `[opentelemetry]` block.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Instrument-name prefix. When empty (the default), the effective
    /// prefix is `uc` — the literal namespace segment used throughout the
    /// DESIGN §3.11.5 inventory. This is NOT derived from the gear name
    /// (`usage-collector` → `usage_collector`); the rendered Prometheus
    /// names are `uc_*`.
    pub prefix: String,
}

impl MetricsConfig {
    /// The instrument-name prefix to build instruments with: the configured
    /// `prefix` if non-blank, else the `uc` default.
    #[must_use]
    pub fn effective_prefix(&self) -> &str {
        let trimmed = self.prefix.trim();
        if trimmed.is_empty() { "uc" } else { trimmed }
    }

    /// Validates the configured instrument-name prefix at bootstrap.
    ///
    /// The effective prefix (see [`effective_prefix`]) becomes the leading
    /// segment of every `uc_*` instrument name via `{prefix}_...`, so it must
    /// be a valid Prometheus/OpenTelemetry name prefix,
    /// `[A-Za-z_][A-Za-z0-9_]*`. Surrounding whitespace is trimmed by
    /// [`effective_prefix`]; interior non-name characters are rejected.
    /// Failing here surfaces a misconfigured prefix at `Gear::init` instead of
    /// as silently broken or dropped telemetry at runtime.
    ///
    /// [`effective_prefix`]: Self::effective_prefix
    ///
    /// # Errors
    ///
    /// Returns an error if the effective prefix does not match
    /// `[A-Za-z_][A-Za-z0-9_]*`.
    pub fn validate(&self) -> anyhow::Result<()> {
        let prefix = self.effective_prefix();
        let mut chars = prefix.chars();
        let valid = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            anyhow::bail!(
                "[usage_collector.metrics].prefix must match [A-Za-z_][A-Za-z0-9_]* \
                 (got {:?}); it is the leading segment of every uc_* instrument name",
                self.prefix
            );
        }
        Ok(())
    }
}

impl UsageCollectorConfig {
    /// Projects the configured covered-period bounds into the
    /// [`CoveredPeriodBounds`] the Ingestion Gateway enforces.
    ///
    /// Infallible, and deliberately so: [`Self::validate`] already guarantees
    /// at bootstrap that each key is non-zero and fits an `i64`, which is what
    /// [`time::Duration::seconds`] needs. A fallible projection would force
    /// every ingestion call site to handle an error bootstrap has already made
    /// unreachable — so the `i64` conversions saturate rather than panic.
    ///
    /// [`CoveredPeriodBounds`]: crate::domain::covered_period::CoveredPeriodBounds
    #[must_use]
    pub fn covered_period_bounds(&self) -> crate::domain::covered_period::CoveredPeriodBounds {
        use crate::domain::covered_period::seconds;
        crate::domain::covered_period::CoveredPeriodBounds {
            future_tolerance: time::Duration::seconds(seconds(self.live_future_tolerance_secs)),
            live_past_tolerance: time::Duration::seconds(seconds(self.live_past_tolerance_secs)),
            backfill_window: time::Duration::seconds(seconds(self.backfill_window_secs)),
        }
    }

    /// Validates the configuration at bootstrap.
    ///
    /// Rejects an empty or whitespace-only `vendor` selector so the failure
    /// surfaces at `Gear::init` rather than lazily on the first dispatch when
    /// plugin selection finds no match. Also rejects a zero type-cache TTL
    /// or capacity: a zero TTL turns every ingestion into a types-registry
    /// round-trip, which is exactly the coupling the cache exists to remove,
    /// and a zero capacity cannot hold even a single resolved declaration.
    ///
    /// The covered-period bounds are checked here too. Each must be non-zero
    /// and must fit an `i64`, because a bound is compared as a duration of
    /// `i64` seconds; a `u64::MAX` tolerance is a configuration mistake, not
    /// an infinite bound. Those guarantees are what make
    /// [`Self::covered_period_bounds`] infallible, so this is the one place a
    /// bad bound can still be refused.
    ///
    /// `backfill_window_secs` must not be narrower than
    /// `live_past_tolerance_secs`. The live path's past-tolerance rejection
    /// tells an emitter to resubmit on the backfill route, so a narrower
    /// window would point an ordinary emitter at a route where that same
    /// period needs elevated authorization. The bounds are otherwise
    /// deliberately asymmetric
    /// (`cpt-cf-usage-collector-adr-backfill-isolation`); this is the one
    /// ordering among them that has to hold.
    ///
    /// Each `ingestion_quota` key is a positive count, and `burst_entries`
    /// must be at least `max_batch_records`: the entry cap runs before the
    /// charge and bounds a submission's cost at `max_batch_records`, so a
    /// smaller capacity would leave a maximal batch unable to fit the bucket
    /// at any token level and the quota rejection's retry delay would name a
    /// wait that never arrives (DESIGN §3.2, §3.8).
    ///
    /// # Errors
    ///
    /// Returns an error if `vendor` is empty or whitespace-only, if the
    /// metrics prefix is not a valid instrument-name prefix (see
    /// [`MetricsConfig::validate`]), if `type_cache_ttl_secs` /
    /// `type_cache_capacity` is zero, if a covered-period bound is zero or
    /// does not fit an `i64`, if `backfill_window_secs` is narrower than
    /// `live_past_tolerance_secs`, if `max_batch_records` is zero, if an
    /// `ingestion_quota` key is zero, if `ingestion_quota.burst_entries` is
    /// below `max_batch_records`, or if `unavailable_retry_after_secs` /
    /// `target_not_converged_retry_after_secs` is zero.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.vendor.trim().is_empty() {
            anyhow::bail!("[usage_collector].vendor must not be empty or whitespace-only");
        }
        self.metrics.validate()?;
        if self.type_cache_ttl_secs == 0 {
            anyhow::bail!(
                "[usage_collector].type_cache_ttl_secs must be greater than 0: \
                 a zero TTL makes every ingestion a types-registry round-trip"
            );
        }
        if self.type_cache_capacity == 0 {
            anyhow::bail!("[usage_collector].type_cache_capacity must be greater than 0");
        }
        if self.live_future_tolerance_secs == 0 {
            anyhow::bail!(
                "[usage_collector].live_future_tolerance_secs must be greater than 0: \
                 a zero future tolerance refuses a covered period ending a \
                 microsecond from now, on every ingestion path"
            );
        }
        if self.live_past_tolerance_secs == 0 {
            anyhow::bail!(
                "[usage_collector].live_past_tolerance_secs must be greater than 0: \
                 a zero past tolerance refuses every covered period on the live \
                 path that does not end in the future"
            );
        }
        if self.backfill_window_secs == 0 {
            anyhow::bail!(
                "[usage_collector].backfill_window_secs must be greater than 0: \
                 a zero window leaves no period the backfill route admits without \
                 elevated authorization"
            );
        }
        // Before the ordering check: a u64::MAX past tolerance is also "wider
        // than the window", and reporting the ordering there would point at
        // the wrong key.
        for (key, secs) in [
            (
                "live_future_tolerance_secs",
                self.live_future_tolerance_secs,
            ),
            ("live_past_tolerance_secs", self.live_past_tolerance_secs),
            ("backfill_window_secs", self.backfill_window_secs),
        ] {
            if i64::try_from(secs).is_err() {
                anyhow::bail!(
                    "[usage_collector].{key} ({secs}) must fit in an i64: a bound is \
                     compared as a duration of i64 seconds, so a value this large is \
                     a configuration mistake rather than an infinite bound"
                );
            }
        }
        // @cpt-dod:cpt-cf-usage-collector-dod-live-time-bounds:p1
        if self.backfill_window_secs < self.live_past_tolerance_secs {
            anyhow::bail!(
                "[usage_collector].backfill_window_secs ({}) must not be narrower than \
                 live_past_tolerance_secs ({}): the live path's past-tolerance rejection \
                 tells an emitter to resubmit on the backfill route, so a narrower window \
                 would point an ordinary emitter at a route where that same period needs \
                 elevated authorization",
                self.backfill_window_secs,
                self.live_past_tolerance_secs
            );
        }
        if self.max_batch_records == 0 {
            anyhow::bail!(
                "[usage_collector].max_batch_records must be greater than 0: a zero cap \
                 refuses every ingestion request"
            );
        }
        // After the cap's own zero rejection, so a zero cap reports against
        // `max_batch_records` rather than against the capacity rule below.
        // @cpt-dod:cpt-cf-usage-collector-dod-quota-per-replica-state:p2
        for (key, value) in [
            (
                "ingestion_quota.sustained_entries_per_sec",
                self.ingestion_quota.sustained_entries_per_sec,
            ),
            (
                "ingestion_quota.burst_entries",
                self.ingestion_quota.burst_entries,
            ),
            (
                "ingestion_quota.idle_eviction_secs",
                self.ingestion_quota.idle_eviction_secs,
            ),
        ] {
            if value == 0 {
                anyhow::bail!("[usage_collector].{key} must be greater than 0");
            }
        }
        let burst = self.ingestion_quota.burst_entries;
        let cap = u64::try_from(self.max_batch_records).unwrap_or(u64::MAX);
        // @cpt-dod:cpt-cf-usage-collector-dod-quota-per-replica-state:p2
        // @cpt-flow:cpt-cf-usage-collector-flow-quota-apportion-replicas:p2
        if burst < cap {
            anyhow::bail!(
                "[usage_collector].ingestion_quota.burst_entries ({burst}) must be at \
                 least max_batch_records ({cap}): otherwise a maximal batch could never \
                 be admitted and the entry cap would name a limit no submission can satisfy"
            );
        }
        // Zero is refused for both retry-delay keys below. A zero delay means
        // "retry immediately", which defeats the purpose DESIGN §3.8 gives
        // the hint: "The default stops a caller hot-looping without implying a
        // precision the gear does not have." A deployment that wants no pause
        // is free to retry on its own schedule without this gear's
        // `Retry-After` endorsing a busy loop.
        if self.unavailable_retry_after_secs == 0 {
            anyhow::bail!(
                "[usage_collector].unavailable_retry_after_secs must be greater than 0: a zero \
                 delay means \"retry immediately\", which defeats the purpose of the hint -- it \
                 stops a caller hot-looping without implying a precision the gear does not have"
            );
        }
        if self.target_not_converged_retry_after_secs == 0 {
            anyhow::bail!(
                "[usage_collector].target_not_converged_retry_after_secs must be greater than 0: \
                 a zero delay means \"retry immediately\", which would hot-loop a caller against \
                 a target the gear expects to still need time to converge"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "config_tests.rs"]
mod config_tests;
