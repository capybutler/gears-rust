use super::*;

#[test]
fn config_defaults_are_applied() {
    let cfg: TimescaleDbPluginConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 10);
    assert_eq!(cfg.pool_size_min, 2);
    assert_eq!(cfg.pool_size_max, 16);
    assert_eq!(cfg.connection_timeout_secs, 10);
    assert_eq!(cfg.statement_timeout_secs, 30);
    assert_eq!(cfg.transaction_timeout_secs, 60);
    assert_eq!(cfg.chunk_time_interval_secs, 604_800);
    assert_eq!(cfg.type_key_slice_width, 1);
    assert_eq!(cfg.retention_sweep_interval_secs, 3_600);
    assert_eq!(cfg.feed_acceptance_slack_secs, 120);
    assert!(cfg.database_url.expose().is_empty());
    // `feed_replay_horizon_secs` carries no working default, so it reads as the
    // zero value the validator refuses, which is how "required" is spelled here.
    assert_eq!(cfg.feed_replay_horizon_secs, 0);
}

#[test]
fn validate_rejects_empty_database_url() {
    let cfg: TimescaleDbPluginConfig = serde_json::from_str("{}").unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn validate_rejects_min_gt_max_pool() {
    let json = r#"{ "database_url": "postgres://x", "pool_size_min": 20, "pool_size_max": 4 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn validate_rejects_pool_max_of_one() {
    // A max of 1 self-deadlocks startup: post-migration setup holds the single
    // connection under an advisory lock while the partitioning statements run
    // on a second, so the pool must allow at least 2.
    let json = r#"{ "database_url": "postgres://x", "pool_size_min": 1, "pool_size_max": 1 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(
        cfg.validate().is_err(),
        "pool_size_max of 1 must be rejected: it self-deadlocks post-migration setup"
    );
}

#[test]
fn validate_rejects_zero_connection_timeout() {
    let json = r#"{ "database_url": "postgres://x", "connection_timeout_secs": 0 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(
        cfg.validate().is_err(),
        "a zero acquire timeout yields a pool that times out immediately"
    );
}

#[test]
fn validate_rejects_zero_statement_timeout() {
    // Postgres treats `statement_timeout = 0` as *disabled* (no bound), which
    // would reintroduce the unbounded-query footgun this setting exists to close,
    // so a zero must be rejected rather than silently disabling the timeout.
    let json = r#"{ "database_url": "postgres://x", "statement_timeout_secs": 0 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(
        cfg.validate().is_err(),
        "a zero statement_timeout disables the bound, leaving request-path queries unbounded"
    );
}

#[test]
fn validate_accepts_nonzero_statement_timeout() {
    let json = r#"{ "database_url": "postgres://x", "feed_replay_horizon_secs": 3600,
                   "statement_timeout_secs": 45 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.validate().is_ok());
}

#[test]
fn a_transaction_timeout_at_or_below_the_statement_timeout_is_rejected() {
    // `docs/DESIGN.md` §3.5: transaction_timeout_secs "MUST be greater than
    // `statement_timeout_secs`; config load rejects a value that is not". It
    // bounds a whole transaction where the statement timeout bounds one
    // statement, so an equal value makes the outer bound unreachable.
    let mut cfg = valid_config();
    cfg.statement_timeout_secs = 30;
    cfg.transaction_timeout_secs = 30;
    let err = cfg.validate().expect_err("equal timeouts must be rejected");
    assert!(
        err.contains("transaction_timeout_secs") && err.contains("statement_timeout_secs"),
        "the failure must name both fields, got: {err}"
    );
}

#[test]
fn a_transaction_timeout_above_the_statement_timeout_is_accepted() {
    let mut cfg = valid_config();
    cfg.statement_timeout_secs = 30;
    cfg.transaction_timeout_secs = 31;
    cfg.validate()
        .expect("a strictly greater transaction timeout is valid");
}

#[test]
fn an_absent_replay_horizon_is_rejected_naming_the_field() {
    // The SPI does not carry the replay horizon, so configuration is its only
    // source (`docs/features/registration-schema-provisioning.md`,
    // `inst-cfg-require-horizon`). Absence and zero are indistinguishable under
    // `#[serde(default)]`, so the validator refuses zero — the same mechanism
    // `database_url` already uses for the other required field.
    let mut cfg = valid_config();
    cfg.feed_replay_horizon_secs = 0;
    let err = cfg
        .validate()
        .expect_err("an absent horizon must be rejected");
    assert!(
        err.contains("feed_replay_horizon_secs"),
        "the failure must name the field, got: {err}"
    );
}

#[test]
fn a_zero_acceptance_slack_is_rejected() {
    // Zero does NOT mean "guard disabled". A slack of zero would refuse every
    // entry whose `accepted_at` is not the INSERT's own `statement_timestamp()`
    // to the microsecond, which is every entry. Refused here so that reading
    // cannot be reintroduced by accident.
    let mut cfg = valid_config();
    cfg.feed_acceptance_slack_secs = 0;
    let err = cfg
        .validate()
        .expect_err("a zero acceptance slack must be rejected");
    assert!(
        err.contains("feed_acceptance_slack_secs"),
        "the failure must name the field, got: {err}"
    );
}

#[test]
fn validate_rejects_zero_chunk_time_interval() {
    let json = r#"{ "database_url": "postgres://x", "feed_replay_horizon_secs": 3600,
                   "chunk_time_interval_secs": 0 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(
        cfg.validate().is_err(),
        "a zero-width chunk cannot hold a row"
    );
}

#[test]
fn validate_rejects_a_chunk_time_interval_beyond_the_interval_bound() {
    let json = format!(
        r#"{{ "database_url": "postgres://x", "feed_replay_horizon_secs": 3600,
               "chunk_time_interval_secs": {} }}"#,
        u64::MAX
    );
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(&json).unwrap();
    assert!(
        cfg.validate().is_err(),
        "an interval make_interval cannot hold must fail before it reaches the database"
    );
}

#[test]
fn validate_rejects_zero_type_key_slice_width() {
    let json = r#"{ "database_url": "postgres://x", "feed_replay_horizon_secs": 3600,
                   "type_key_slice_width": 0 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(
        cfg.validate().is_err(),
        "a slice must hold at least one type key"
    );
}

#[test]
fn validate_rejects_a_type_key_slice_width_wider_than_the_key_type() {
    let json = r#"{ "database_url": "postgres://x", "feed_replay_horizon_secs": 3600,
                   "type_key_slice_width": 2147483648 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(
        cfg.validate().is_err(),
        "type_key is an int; a wider slice is meaningless"
    );
}

#[test]
fn validate_rejects_zero_retention_sweep_interval() {
    let json = r#"{ "database_url": "postgres://x", "feed_replay_horizon_secs": 3600,
                   "retention_sweep_interval_secs": 0 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(
        cfg.validate().is_err(),
        "a zero interval would sweep in a hot loop"
    );
}

#[test]
fn validate_rejects_a_retention_sweep_interval_beyond_the_interval_bound() {
    let json = format!(
        r#"{{ "database_url": "postgres://x", "feed_replay_horizon_secs": 3600,
               "retention_sweep_interval_secs": {} }}"#,
        u64::MAX
    );
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(&json).unwrap();
    assert!(
        cfg.validate().is_err(),
        "an interval make_interval cannot hold must fail before it reaches the database"
    );
}

#[test]
fn config_rejects_the_retired_table_wide_retention_key() {
    // Retention is per type now, read from types-registry. A config still
    // carrying the table-wide window must fail loudly rather than be ignored.
    let json = r#"{ "database_url": "postgres://x", "retention_period_secs": 31536000 }"#;
    assert!(serde_json::from_str::<TimescaleDbPluginConfig>(json).is_err());
}

#[test]
fn validate_accepts_well_formed_config() {
    let json = r#"{ "database_url": "postgres://u:p@h/db?sslmode=require",
                   "feed_replay_horizon_secs": 3600 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.validate().is_ok());
}

#[test]
fn config_rejects_unknown_fields() {
    let json = r#"{ "database_url": "postgres://x", "nope": true }"#;
    assert!(serde_json::from_str::<TimescaleDbPluginConfig>(json).is_err());
}

#[test]
fn expand_vars_expands_database_url_placeholders() {
    use toolkit::var_expand::ExpandVars;
    let json = r#"{ "database_url": "postgres://u:p@h:${UC_TS_DSN_PORT_CANARY_9f3a:-5432}/db?sslmode=require" }"#;
    let mut cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    cfg.expand_vars()
        .expect("expand_vars should resolve placeholders");
    assert_eq!(
        cfg.database_url.expose(),
        "postgres://u:p@h:5432/db?sslmode=require"
    );
}

#[test]
fn debug_does_not_leak_database_url_password() {
    // The DSN embeds the Postgres password; any `Debug` of the config (a stray
    // `tracing::debug!(?cfg)`, a panic formatter) must not print it.
    let json =
        r#"{ "database_url": "postgres://pguser:sup3r-s3cret@db:5432/app?sslmode=require" }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    let dump = format!("{cfg:?}");
    assert!(
        !dump.contains("sup3r-s3cret"),
        "Debug of the config must not leak the DSN password; got: {dump}"
    );
}

#[test]
fn rollup_defaults_are_applied() {
    let cfg: TimescaleDbPluginConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(cfg.rollup_materialization_lag_secs, 7_200);
    assert_eq!(cfg.rollup_live_window_secs, 259_200);
    assert_eq!(cfg.rollup_refresh_interval_secs, 120);
    assert_eq!(cfg.rollup_history_refresh_interval_secs, 3_600);
}

/// A config that passes [`TimescaleDbPluginConfig::validate`], for a test that
/// varies one field and asserts on the outcome.
///
/// Both fields it sets are the ones with no working default, so a config
/// without them fails validation for that reason whatever else a test varied.
/// The helper validates itself, so a test that then gets an `Err` knows the
/// mutation it made is what produced it.
fn valid_config() -> TimescaleDbPluginConfig {
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(
        r#"{ "database_url": "postgres://u:p@h/db?sslmode=require",
              "feed_replay_horizon_secs": 3600 }"#,
    )
    .unwrap();
    cfg.validate()
        .expect("the helper must hand back a config that validates");
    cfg
}

/// Parse `{ "database_url": "postgres://x", <extra> }` and validate it.
///
/// Carries `feed_replay_horizon_secs` for the same reason [`valid_config`]
/// does: without it every call would return `Err` naming the horizon, and a
/// test asserting `is_err()` would pass whatever it varied.
fn validate_with(extra: &str) -> Result<(), String> {
    let json = format!(
        r#"{{ "database_url": "postgres://x", "feed_replay_horizon_secs": 3600, {extra} }}"#
    );
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(&json).unwrap();
    cfg.validate()
}

#[test]
fn validate_rejects_a_chunk_interval_that_is_not_whole_hours() {
    // Every hourly rollup bucket must lie inside one ledger chunk, or the
    // retention cut cannot delete a bucket without under-counting a neighbour.
    assert!(validate_with(r#""chunk_time_interval_secs": 5400"#).is_err());
    assert!(validate_with(r#""chunk_time_interval_secs": 7200"#).is_ok());
}

#[test]
fn validate_rejects_a_materialization_lag_that_is_not_whole_hours_or_under_one_hour() {
    assert!(validate_with(r#""rollup_materialization_lag_secs": 5400"#).is_err());
    assert!(validate_with(r#""rollup_materialization_lag_secs": 0"#).is_err());
    assert!(validate_with(r#""rollup_materialization_lag_secs": 3600"#).is_ok());
}

#[test]
fn validate_rejects_a_materialization_lag_not_below_the_live_window() {
    assert!(
        validate_with(
            r#""rollup_materialization_lag_secs": 259200, "rollup_live_window_secs": 259200"#
        )
        .is_err()
    );
}

#[test]
fn validate_rejects_a_live_window_that_is_not_whole_hours_or_beyond_the_bound() {
    assert!(
        validate_with(
            r#""rollup_live_window_secs": 262800, "rollup_materialization_lag_secs": 3600"#
        )
        .is_ok()
    );
    assert!(validate_with(r#""rollup_live_window_secs": 260000"#).is_err());
    assert!(
        validate_with(&format!(
            r#""rollup_live_window_secs": {}"#,
            u64::MAX - (u64::MAX % 3600)
        ))
        .is_err()
    );
}

#[test]
fn validate_rejects_zero_or_unbounded_rollup_refresh_intervals() {
    for key in [
        "rollup_refresh_interval_secs",
        "rollup_history_refresh_interval_secs",
    ] {
        assert!(
            validate_with(&format!(r#""{key}": 0"#)).is_err(),
            "{key} = 0"
        );
        assert!(
            validate_with(&format!(r#""{key}": {}"#, u64::MAX)).is_err(),
            "{key} = MAX"
        );
    }
}
