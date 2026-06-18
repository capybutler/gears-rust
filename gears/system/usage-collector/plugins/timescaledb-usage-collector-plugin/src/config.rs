use serde::Deserialize;

/// Configuration for the `TimescaleDB` Usage Collector storage backend.
/// Durations are whole seconds (repo convention).
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct TimescaleDbPluginConfig {
    /// Postgres DSN; TLS required (use `sslmode=require`).
    #[expand_vars]
    pub database_url: String,
    /// Connection-pool lower bound.
    pub pool_size_min: u32,
    /// Connection-pool upper bound.
    pub pool_size_max: u32,
    /// Acquire timeout in seconds.
    pub connection_timeout_secs: u64,
    /// `usage_records` retention window in seconds; chunks wholly older are dropped.
    pub retention_period_secs: u64,
    /// Vendor name for GTS instance registration.
    pub vendor: String,
    /// Plugin priority (lower = higher priority).
    pub priority: i16,
}

impl Default for TimescaleDbPluginConfig {
    fn default() -> Self {
        Self {
            database_url: String::new(),
            pool_size_min: 2,
            pool_size_max: 16,
            connection_timeout_secs: 10,
            retention_period_secs: 365 * 86_400, // 365 days
            vendor: "cyberfabric".to_owned(),
            priority: 10,
        }
    }
}

/// Upper bound on `retention_period_secs` (100 years in seconds).
///
/// Postgres `make_interval(secs => ...)` — used to register the retention
/// policy and the dedup-cleanup job (see `pool::apply_retention_policy`) —
/// overflows well below `u64::MAX`. A pathological retention would otherwise
/// surface as a confusing failure *after* migrations have already run. 100
/// years is far beyond any realistic usage-data retention while staying safely
/// inside `make_interval`'s range.
const MAX_RETENTION_SECS: u64 = 100 * 365 * 86_400;

impl TimescaleDbPluginConfig {
    /// Validate invariants not expressible in the type.
    ///
    /// # Errors
    /// Returns an error string for an empty DSN, inconsistent pool bounds, a
    /// zero acquire timeout, or a retention window outside `(0,
    /// MAX_RETENTION_SECS]`.
    pub fn validate(&self) -> Result<(), String> {
        if self.database_url.trim().is_empty() {
            return Err("database_url must not be empty".to_owned());
        }
        if self.pool_size_max == 0 || self.pool_size_min > self.pool_size_max {
            return Err(format!(
                "invalid pool bounds: min={} max={}",
                self.pool_size_min, self.pool_size_max
            ));
        }
        if self.connection_timeout_secs == 0 {
            // A zero acquire timeout makes every pool checkout fail instantly.
            return Err("connection_timeout_secs must be > 0".to_owned());
        }
        if self.retention_period_secs == 0 {
            return Err("retention_period_secs must be > 0".to_owned());
        }
        if self.retention_period_secs > MAX_RETENTION_SECS {
            return Err(format!(
                "retention_period_secs must be <= {MAX_RETENTION_SECS} (100 years); \
                 a larger window overflows the backend interval type"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "config_tests.rs"]
mod config_tests;
