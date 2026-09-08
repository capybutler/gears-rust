//! Configuration for the usage-collector gear.
//!
//! Carries the storage-plugin vendor selector, the Type Resolver cache
//! knobs, the metadata size cap, and the three covered-period bounds. Read
//! once at `Gear::init` via `ctx.config_or_default()`; changing the binding
//! requires a gear restart. The usage-type catalog is
//! plugin-owned (ADR-0012 / foundation.md 0.2.0), so no usage-type
//! declarations are accepted here.
//!
//! **Both halves of that last sentence are stale, and it is left standing
//! rather than half-corrected.** The catalog is not plugin-owned any more —
//! `types-registry` owns every declaration and this gear registers no
//! usage-type surface at all — and `ADR-0012` no longer names the decision
//! it once did: that number now resolves to
//! `cpt-cf-usage-collector-adr-backfill-isolation`. It is the live example
//! of why a shipped citation names an ADR by id and never by number.
//! Repointing the id alone would preserve a false claim under a correct
//! reference, which is worse than an obviously stale one; the sentence
//! needs rewriting by whoever finishes the usage-type-catalog doc debt.
//! The same pair sits in `lib.rs` twice and in `config_tests.rs`.

use serde::Deserialize;

/// Gear configuration for `[usage-collector]`.
///
/// Read once at `Gear::init` via `ctx.config_or_default()`; changing the
/// binding requires a gear restart.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UsageCollectorConfig {
    /// Vendor selector used to pick a storage-plugin implementation.
    ///
    /// The host queries types-registry for plugin instances matching this
    /// vendor and selects the one with the lowest priority number — but only
    /// lazily, on the first dispatch. No `types-registry` query happens at
    /// `init`.
    pub vendor: String,

    /// Operational-metrics configuration (`[usage_collector.metrics]`).
    ///
    /// Carries only the substitutable instrument-name prefix; the OTLP
    /// exporter, cardinality limit, and global `SdkMeterProvider` are all
    /// ToolKit-owned (`[opentelemetry]` block) per
    /// `cpt-cf-usage-collector-principle-otlp-push-emission`.
    pub metrics: MetricsConfig,

    /// How long a resolved GTS type declaration is served before the Type
    /// Resolver refreshes it, in seconds.
    ///
    /// Fold, unit and metadata surface are immutable for a type's life, so
    /// this is not a correctness window for them. It bounds how long a
    /// withdrawn declaration keeps resolving, and it is what keeps the cache
    /// honest once `types-registry` admits mutable major-only identifiers.
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
    /// [`DEFAULT_METADATA_SIZE_CAP_BYTES`](crate::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES)
    /// (`8192`) — the same constant `Service::new` hard-codes when it
    /// delegates to `Service::new_with_metrics` (which takes the cap as a
    /// plain mandatory `usize`, with no default of its own; every other
    /// caller, `module.rs` bootstrap included, passes this configured value
    /// or the constant explicitly) — so wiring this value through to
    /// `domain::validation::validate_submit_record_metadata` is a
    /// no-op for the default deployment rather than a silent tightening.
    pub metadata_size_cap_bytes: usize,

    /// How far into the future a covered period may end, in seconds.
    ///
    /// The live path rejects a period ending further ahead than this, and
    /// **so does the backfill route** — the route lifts the past bound
    /// only. The bound protects against a defective or clock-skewed emitter
    /// opening a period that does not yet exist
    /// (`cpt-cf-usage-collector-adr-backfill-isolation`).
    ///
    /// Defaults to `300` (5 minutes), the value DESIGN
    /// `cpt-cf-usage-collector-fr-live-future-time-bound` publishes.
    pub live_future_tolerance_secs: u64,

    /// How far into the past a covered period may end on the **live** path,
    /// in seconds.
    ///
    /// A period ending further back is rejected with a message naming the
    /// backfill route, which is where such an entry belongs. The default
    /// covers emitter outage and retry lag, which is what genuinely late
    /// live data is; anything older is history, and history belongs on the
    /// route that marks it.
    ///
    /// The bound belongs to the path, not to the entry kind, so it governs
    /// an invalidation over the period it copies exactly as it governs a
    /// measurement. A withdrawal of a closed month therefore travels the
    /// backfill route.
    ///
    /// Defaults to `172_800` (48 hours).
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
    /// storage profile guarantees for the target GTS type. The retention
    /// floor is this window plus one replay horizon
    /// (`cpt-cf-usage-collector-fr-billing-retention-floor`), 125 days at
    /// the launch defaults — a plugin-readiness condition surfaced at
    /// review, not a gear-side sweep.
    ///
    /// Defaults to `7_776_000` (90 days).
    pub backfill_window_secs: u64,
}

impl Default for UsageCollectorConfig {
    fn default() -> Self {
        Self {
            vendor: "cyberfabric".to_owned(),
            metrics: MetricsConfig::default(),
            type_cache_ttl_secs: 300,
            type_cache_capacity: 10_000,
            metadata_size_cap_bytes: crate::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES,
            live_future_tolerance_secs:
                crate::domain::covered_period::DEFAULT_LIVE_FUTURE_TOLERANCE_SECS,
            live_past_tolerance_secs:
                crate::domain::covered_period::DEFAULT_LIVE_PAST_TOLERANCE_SECS,
            backfill_window_secs: crate::domain::covered_period::DEFAULT_BACKFILL_WINDOW_SECS,
        }
    }
}

/// Operational-metrics configuration for `[usage_collector.metrics]`.
///
/// The only knob is the leading namespace segment of every instrument name
/// (`uc_` by default), which DESIGN §3.11.5 declares "substitutable at
/// adapter construction". Everything else about the metrics pipeline (OTLP
/// export, cardinality limit, backend selection) is owned by `ToolKit`'s
/// `[opentelemetry]` block, not by this gear.
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
    /// be a valid Prometheus/OpenTelemetry name prefix: a leading ASCII letter
    /// or underscore followed by ASCII letters, digits, or underscores
    /// (`[A-Za-z_][A-Za-z0-9_]*`). Surrounding whitespace is tolerated (it is
    /// trimmed by [`effective_prefix`]); interior spaces, dots, slashes, and
    /// other non-name characters are rejected. Failing here surfaces a
    /// misconfigured prefix at `Gear::init` instead of as silently broken or
    /// dropped telemetry at runtime.
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
    /// Projects the three configured covered-period bounds into the
    /// [`CoveredPeriodBounds`] the Ingestion Gateway enforces.
    ///
    /// Infallible, and deliberately so: [`Self::validate`] already
    /// guarantees at bootstrap that each of the three keys is non-zero and
    /// fits an `i64`, which is exactly what
    /// [`time::Duration::seconds`] needs. A fallible projection here would
    /// force every ingestion call site to handle an error that bootstrap has
    /// already made unreachable — so the `i64` conversions saturate rather
    /// than panic, and a configuration that could saturate them was refused
    /// at `Gear::init`.
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
    /// The three covered-period bounds are checked here too. Each must be
    /// non-zero and must fit an `i64`, because a bound is compared as a
    /// duration of `i64` seconds; a `u64::MAX` tolerance is a configuration
    /// mistake, not an infinite bound. Those two guarantees are what make
    /// [`Self::covered_period_bounds`] — the projection the Ingestion
    /// Gateway enforces — infallible, so this is the one place a bad bound
    /// can still be refused.
    ///
    /// `backfill_window_secs` must not be narrower than
    /// `live_past_tolerance_secs`. The live path's past-tolerance rejection
    /// tells an emitter to resubmit on the backfill route, so a narrower
    /// window would point an ordinary emitter at a route where that same
    /// period needs elevated authorization. The three bounds are otherwise
    /// deliberately asymmetric
    /// (`cpt-cf-usage-collector-adr-backfill-isolation`, "Why the three
    /// bounds are asymmetric"); this is the one ordering among them that has
    /// to hold.
    ///
    /// # Errors
    ///
    /// Returns an error if `vendor` is empty or whitespace-only, if the
    /// metrics prefix is not a valid instrument-name prefix (see
    /// [`MetricsConfig::validate`]), if `type_cache_ttl_secs` /
    /// `type_cache_capacity` is zero, if any of the three covered-period
    /// bounds is zero or does not fit an `i64`, or if
    /// `backfill_window_secs` is narrower than `live_past_tolerance_secs`.
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
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "config_tests.rs"]
mod config_tests;
