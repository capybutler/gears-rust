use secrecy::SecretString;
use serde::Deserialize;
use toolkit::var_expand::{ExpandVars as ExpandVarsTrait, ExpandVarsError};

/// Wrapper around a config string whose final value is a secret.
///
/// Mirrors the `SecretFromEnv` wrapper in
/// `crates/gears/plugins/vp-idp-plugin/src/config.rs`. It holds a plain
/// `String` for `Deserialize` + `ExpandVars` compatibility (toolkit's
/// `#[expand_vars]` derive substitutes `${VAR}` placeholders on `String`
/// fields; `secrecy::SecretString` is not `ExpandVars`-aware), but suppresses
/// every accidental leak surface around it:
///
/// * No `Display` impl — `format!("{secret}")` won't compile.
/// * `Debug` emits `<redacted>`, so `tracing::debug!(?cfg)` / panic-formatter
///   dumps never print the resolved DSN (which embeds `${PGPASSWORD}`).
/// * No `Serialize`, no `PartialEq` — secret bytes never leak through a
///   config-snapshot path or an assertion message.
///
/// The only read accessors are [`Self::expose`] (deliberately verbose so every
/// read site is grep-able) and [`Self::clone_into_secret_string`], which moves
/// the value behind the `secrecy` opaque-debug/zeroize guarantee at the
/// connection boundary. `expand_vars` runs on the inner `String` before any
/// consumer sees the value.
#[derive(Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct SecretFromEnv(String);

impl SecretFromEnv {
    /// Read the resolved secret bytes. Use only at boundaries that consume the
    /// DSN (config validation, building the pool's `connect_options`); never log
    /// the returned value.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Clone the resolved secret into a [`SecretString`] so the connection
    /// boundary holds it behind `secrecy`'s opaque-debug + zeroize-on-drop
    /// guarantees. The intermediate `String` lives only on the caller's stack.
    #[must_use]
    pub fn clone_into_secret_string(&self) -> SecretString {
        SecretString::from(self.0.clone())
    }
}

impl std::fmt::Debug for SecretFromEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl ExpandVarsTrait for SecretFromEnv {
    fn expand_vars(&mut self) -> Result<(), ExpandVarsError> {
        self.0.expand_vars()
    }
}

/// Configuration for the `TimescaleDB` Usage Collector storage backend.
/// Durations are whole seconds (repo convention).
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct TimescaleDbPluginConfig {
    /// Postgres DSN; TLS required (use `sslmode=require`). The DSN embeds the
    /// Postgres password, so it is wrapped in [`SecretFromEnv`] (Debug-redacted,
    /// no Display / Serialize); `${VAR}` templating is still expanded via the
    /// `#[expand_vars]` derive.
    #[expand_vars]
    pub database_url: SecretFromEnv,
    /// Connection-pool lower bound.
    pub pool_size_min: u32,
    /// Connection-pool upper bound.
    pub pool_size_max: u32,
    /// Acquire timeout in seconds.
    pub connection_timeout_secs: u64,
    /// Per-statement timeout in seconds, applied as the Postgres
    /// `statement_timeout` GUC on every request-path pool connection. Bounds how
    /// long a single query (a hypertable list/aggregate scan, a single-row
    /// ingest, a multi-row batch write) may run so a wedged backend cannot pin
    /// a pool connection indefinitely and exhaust the pool. Must be `> 0`:
    /// Postgres treats `statement_timeout = 0` as *disabled*, which would
    /// reintroduce the unbounded-query footgun.
    pub statement_timeout_secs: u64,
    /// Time width of a new ledger chunk, in seconds. Applied at startup to
    /// chunks created afterwards; existing chunks keep their range.
    pub chunk_time_interval_secs: u64,
    /// How many consecutive type keys share one slice of the ledger's second
    /// partitioning dimension. `1` gives every type its own chunks, so each is
    /// dropped exactly at its own retention; `N` lets up to `N` types share a
    /// chunk, which is then held to the longest retention among them. Applied at
    /// startup to chunks created afterwards.
    pub type_key_slice_width: u32,
    /// Seconds between two retention sweeps.
    pub retention_sweep_interval_secs: u64,
    /// Vendor name for GTS instance registration.
    pub vendor: String,
    /// Plugin priority (lower = higher priority).
    pub priority: i16,
}

impl Default for TimescaleDbPluginConfig {
    fn default() -> Self {
        Self {
            database_url: SecretFromEnv::default(),
            pool_size_min: 2,
            pool_size_max: 16,
            connection_timeout_secs: 10,
            statement_timeout_secs: 30,
            chunk_time_interval_secs: 7 * 86_400,
            type_key_slice_width: 1,
            retention_sweep_interval_secs: 3_600,
            vendor: "cyberfabric".to_owned(),
            priority: 10,
        }
    }
}

/// Upper bound on every interval setting (100 years in seconds).
///
/// Postgres `make_interval(secs => ...)`, which applies the chunk interval,
/// overflows well below `u64::MAX`. A pathological value would otherwise surface
/// as a confusing failure *after* migrations have already run.
const MAX_INTERVAL_SECS: u64 = 100 * 365 * 86_400;

/// Upper bound on `type_key_slice_width`: `type_key` is an `int`.
const MAX_TYPE_KEY_SLICE_WIDTH: u32 = 2_147_483_647;

impl TimescaleDbPluginConfig {
    /// Validate invariants not expressible in the type.
    ///
    /// # Errors
    /// Returns an error string for an empty DSN, a pool `max` below 2 or
    /// `min > max`, a zero acquire timeout, a zero statement timeout, an interval
    /// outside `(0, MAX_INTERVAL_SECS]`, or a slice width outside
    /// `[1, MAX_TYPE_KEY_SLICE_WIDTH]`.
    pub fn validate(&self) -> Result<(), String> {
        if self.database_url.expose().trim().is_empty() {
            return Err("database_url must not be empty".to_owned());
        }
        // `max` must be >= 2, not just != 0: `apply_post_migration_setup` holds
        // one connection under a session advisory lock for the whole critical
        // section while `apply_partitioning` runs on a *second* on the same
        // pool. A `max` of 1 therefore self-deadlocks startup (`PoolTimedOut`).
        if self.pool_size_max < 2 || self.pool_size_min > self.pool_size_max {
            return Err(format!(
                "invalid pool bounds: min={} max={} (max must be >= 2: \
                 post-migration setup holds one connection while the partitioning \
                 statements run on a second — a max of 1 self-deadlocks startup)",
                self.pool_size_min, self.pool_size_max
            ));
        }
        if self.connection_timeout_secs == 0 {
            // A zero acquire timeout makes every pool checkout fail instantly.
            return Err("connection_timeout_secs must be > 0".to_owned());
        }
        if self.statement_timeout_secs == 0 {
            // Postgres treats `statement_timeout = 0` as *disabled*, so a zero here
            // would leave every request-path query unbounded — the exact footgun
            // this setting exists to close. Reject it rather than silently disable.
            return Err(
                "statement_timeout_secs must be > 0 (0 disables the Postgres \
                 statement_timeout, leaving request-path queries unbounded)"
                    .to_owned(),
            );
        }
        if self.chunk_time_interval_secs == 0 || self.chunk_time_interval_secs > MAX_INTERVAL_SECS {
            return Err(format!(
                "chunk_time_interval_secs must be in (0, {MAX_INTERVAL_SECS}] (100 years)"
            ));
        }
        if self.type_key_slice_width == 0 || self.type_key_slice_width > MAX_TYPE_KEY_SLICE_WIDTH {
            return Err(format!(
                "type_key_slice_width must be in [1, {MAX_TYPE_KEY_SLICE_WIDTH}]"
            ));
        }
        if self.retention_sweep_interval_secs == 0
            || self.retention_sweep_interval_secs > MAX_INTERVAL_SECS
        {
            return Err(format!(
                "retention_sweep_interval_secs must be in (0, {MAX_INTERVAL_SECS}] (100 years)"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "config_tests.rs"]
mod config_tests;
