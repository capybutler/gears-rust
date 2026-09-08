//! Configuration for the usage-collector gear.
//!
//! Carries only the vendor selector used to bind a storage-plugin
//! implementation. Read once at `Gear::init` via `ctx.config_or_default()`;
//! changing the binding requires a gear restart. The usage-type catalog is
//! plugin-owned (ADR-0012 / foundation.md 0.2.0), so no usage-type
//! declarations are accepted here.

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
}

impl Default for UsageCollectorConfig {
    fn default() -> Self {
        Self {
            vendor: "cyberfabric".to_owned(),
            metrics: MetricsConfig::default(),
            type_cache_ttl_secs: 300,
            type_cache_capacity: 10_000,
            metadata_size_cap_bytes: crate::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES,
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
    /// Validates the configuration at bootstrap.
    ///
    /// Rejects an empty or whitespace-only `vendor` selector so the failure
    /// surfaces at `Gear::init` rather than lazily on the first dispatch when
    /// plugin selection finds no match. Also rejects a zero type-cache TTL
    /// or capacity: a zero TTL turns every ingestion into a types-registry
    /// round-trip, which is exactly the coupling the cache exists to remove,
    /// and a zero capacity cannot hold even a single resolved declaration.
    ///
    /// # Errors
    ///
    /// Returns an error if `vendor` is empty or whitespace-only, if the
    /// metrics prefix is not a valid instrument-name prefix (see
    /// [`MetricsConfig::validate`]), or if `type_cache_ttl_secs` /
    /// `type_cache_capacity` is zero.
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
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "config_tests.rs"]
mod config_tests;
