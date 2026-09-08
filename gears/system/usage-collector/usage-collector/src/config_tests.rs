//! Unit tests for the `[usage_collector]` configuration surface.
//!
//! Only the vendor binding and serde posture (`#[serde(default,
//! deny_unknown_fields)]`) are exercised here; the metric catalog is plugin-
//! owned under ADR-0012, so there is no host-side declared-catalog surface
//! left to test.

use super::*;

#[test]
fn serde_default_applies_default_vendor() {
    let cfg: UsageCollectorConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(
        cfg.vendor, "cyberfabric",
        "serde(default) must use Default impl"
    );
}

#[test]
fn vendor_can_be_overridden_via_serde() {
    let json = r#"{"vendor": "acme"}"#;
    let cfg: UsageCollectorConfig = serde_json::from_str(json).unwrap();
    assert_eq!(cfg.vendor, "acme");
}

#[test]
fn rejects_unknown_fields() {
    let json = r#"{"vendor": "x", "unexpected": true}"#;
    assert!(serde_json::from_str::<UsageCollectorConfig>(json).is_err());
}

#[test]
fn validate_accepts_default_vendor() {
    assert!(UsageCollectorConfig::default().validate().is_ok());
}

#[test]
fn validate_rejects_empty_vendor() {
    let cfg = UsageCollectorConfig {
        vendor: String::new(),
        ..Default::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn validate_rejects_whitespace_only_vendor() {
    let cfg = UsageCollectorConfig {
        vendor: "   \t ".to_owned(),
        ..Default::default()
    };
    assert!(cfg.validate().is_err());
}

// ── MetricsConfig (operational-metrics prefix substitution) ──
//
// DESIGN §3.11.5: the leading `uc_` namespace segment is "substitutable at
// adapter construction". The prefix defaults to `uc` (NOT the snake_cased
// gear name) so the rendered Prometheus names match the inventory literally.

#[test]
fn metrics_config_defaults_to_uc_prefix() {
    assert_eq!(MetricsConfig::default().effective_prefix(), "uc");
}

#[test]
fn metrics_config_effective_prefix_uses_override() {
    let cfg = MetricsConfig {
        prefix: "acme".to_owned(),
    };
    assert_eq!(cfg.effective_prefix(), "acme");
}

#[test]
fn metrics_config_effective_prefix_falls_back_on_blank() {
    let cfg = MetricsConfig {
        prefix: "   ".to_owned(),
    };
    assert_eq!(
        cfg.effective_prefix(),
        "uc",
        "a blank/whitespace prefix must fall back to the `uc` default"
    );
}

#[test]
fn serde_default_applies_default_metrics_prefix() {
    let cfg: UsageCollectorConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(cfg.metrics.effective_prefix(), "uc");
}

#[test]
fn metrics_block_can_be_overridden_via_serde() {
    let json = r#"{"vendor": "acme", "metrics": {"prefix": "am"}}"#;
    let cfg: UsageCollectorConfig = serde_json::from_str(json).unwrap();
    assert_eq!(cfg.metrics.effective_prefix(), "am");
}

#[test]
fn metrics_block_rejects_unknown_fields() {
    let json = r#"{"metrics": {"prefix": "uc", "bogus": 1}}"#;
    assert!(serde_json::from_str::<UsageCollectorConfig>(json).is_err());
}

// ── Metrics-prefix validation ──
//
// The effective prefix is interpolated into every `uc_*` instrument name via
// `{prefix}_...`, so it must be a valid Prometheus/OTel name prefix
// (`[A-Za-z_][A-Za-z0-9_]*`). `validate()` rejects a malformed prefix at
// `Gear::init` instead of letting it surface as broken/dropped telemetry.

/// A valid config whose only variable is the metrics prefix.
fn cfg_with_prefix(prefix: &str) -> UsageCollectorConfig {
    UsageCollectorConfig {
        metrics: MetricsConfig {
            prefix: prefix.to_owned(),
        },
        ..Default::default()
    }
}

#[test]
fn validate_accepts_default_metrics_prefix() {
    // Blank prefix → effective "uc", a valid instrument namespace.
    assert!(cfg_with_prefix("").validate().is_ok());
}

#[test]
fn validate_accepts_valid_custom_prefix() {
    assert!(cfg_with_prefix("acme_uc").validate().is_ok());
    assert!(cfg_with_prefix("_private").validate().is_ok());
    assert!(cfg_with_prefix("uc2").validate().is_ok());
}

#[test]
fn validate_accepts_prefix_with_surrounding_whitespace() {
    // effective_prefix() trims, so surrounding whitespace is tolerated.
    let cfg = cfg_with_prefix("  uc  ");
    assert!(cfg.validate().is_ok());
    assert_eq!(cfg.metrics.effective_prefix(), "uc");
}

#[test]
fn validate_rejects_prefix_with_interior_space() {
    assert!(cfg_with_prefix("my prefix").validate().is_err());
}

#[test]
fn validate_rejects_prefix_with_dot() {
    assert!(cfg_with_prefix("uc.v2").validate().is_err());
}

#[test]
fn validate_rejects_prefix_with_slash() {
    assert!(cfg_with_prefix("uc/x").validate().is_err());
}

#[test]
fn validate_rejects_prefix_starting_with_digit() {
    assert!(cfg_with_prefix("2uc").validate().is_err());
}

#[test]
fn validate_rejects_prefix_with_hyphen() {
    // The gear name is `usage-collector`, but instrument names are `uc_*`;
    // a hyphen is not a legal Prometheus/OTel name character.
    assert!(cfg_with_prefix("usage-collector").validate().is_err());
}

// ── Type Resolver cache knobs (`type_cache_ttl_secs` / `type_cache_capacity` /
// `metadata_size_cap_bytes`) ──
//
// The cache is what keeps ingestion's NFRs independent of types-registry's
// own availability/latency (see `domain::type_resolver`); a zero TTL or
// capacity defeats that purpose, so `validate()` rejects both at
// `Gear::init` rather than silently degrading every dispatch into a
// registry round-trip.

#[test]
fn type_cache_defaults_are_applied_when_absent() {
    // `serde_json`, not `toml`: every other test in this file parses via
    // `serde_json::from_str`, and this crate does not depend on the `toml`
    // crate at all — adding it just for this assertion would be a new
    // dependency for a format-agnostic serde derive to exercise.
    let cfg: UsageCollectorConfig = serde_json::from_str("{}").expect("empty config parses");
    assert_eq!(cfg.type_cache_ttl_secs, 300);
    assert_eq!(cfg.type_cache_capacity, 10_000);
    // Matches `RECORD_METADATA_SIZE_CAP_BYTES` in `domain::validation`: the
    // incumbent hard-coded cap, not an invented placeholder, so wiring the
    // two together later is a no-op for the default deployment.
    assert_eq!(cfg.metadata_size_cap_bytes, 8192);
}

#[test]
fn type_cache_knobs_are_overridable() {
    let json = r#"{
        "type_cache_ttl_secs": 60,
        "type_cache_capacity": 500,
        "metadata_size_cap_bytes": 2048
    }"#;
    let cfg: UsageCollectorConfig = serde_json::from_str(json).expect("config parses");
    assert_eq!(cfg.type_cache_ttl_secs, 60);
    assert_eq!(cfg.type_cache_capacity, 500);
    assert_eq!(cfg.metadata_size_cap_bytes, 2048);
}

#[test]
fn a_zero_ttl_is_rejected() {
    // A zero TTL turns every ingestion into a registry round-trip, which is
    // the coupling the cache exists to remove.
    let cfg = UsageCollectorConfig {
        type_cache_ttl_secs: 0,
        ..Default::default()
    };
    let err = cfg.validate().expect_err("zero TTL must be rejected");
    assert!(
        err.to_string().contains("type_cache_ttl_secs"),
        "error must name the offending key, got: {err}"
    );
}

#[test]
fn a_zero_capacity_is_rejected() {
    // A zero capacity cannot hold even a single resolved declaration, which
    // would make the cache a no-op while still claiming to exist.
    let cfg = UsageCollectorConfig {
        type_cache_capacity: 0,
        ..Default::default()
    };
    let err = cfg.validate().expect_err("zero capacity must be rejected");
    assert!(
        err.to_string().contains("type_cache_capacity"),
        "error must name the offending key, got: {err}"
    );
}

// ── Covered-period bounds (`live_future_tolerance_secs` /
// `live_past_tolerance_secs` / `backfill_window_secs`) ──
//
// The three bounds are asymmetric by design
// (`cpt-cf-usage-collector-adr-backfill-isolation`, "Why the three bounds
// are asymmetric"): the future bound applies on both routes, the past
// tolerance governs the live route alone, and the backfill window selects
// which PDP action a backfilled entry is authorized against. `validate()`
// rejects a zero bound, a value that cannot be a duration of `i64` seconds,
// and the one ordering among the three that is load-bearing.

#[test]
fn the_covered_period_defaults_are_the_ones_design_publishes() {
    let cfg = UsageCollectorConfig::default();
    assert_eq!(cfg.live_future_tolerance_secs, 300, "5 minutes");
    assert_eq!(cfg.live_past_tolerance_secs, 172_800, "48 hours");
    assert_eq!(cfg.backfill_window_secs, 7_776_000, "90 days");
}

#[test]
fn the_covered_period_bounds_survive_an_absent_table() {
    // Distinct from the `Default::default()` assertion above: that reads the
    // `Default` impl, this reads the path a deployment actually takes when
    // the config file omits the keys. Container-level `#[serde(default)]`
    // makes the two the same code today, and this pins that they stay so —
    // a field-level `#[serde(default = "...")]` disagreeing with `Default`
    // would silently ship one set of bounds to an explicit config and
    // another to an absent one.
    let cfg: UsageCollectorConfig = serde_json::from_str("{}").expect("empty config parses");
    assert_eq!(cfg.live_future_tolerance_secs, 300);
    assert_eq!(cfg.live_past_tolerance_secs, 172_800);
    assert_eq!(cfg.backfill_window_secs, 7_776_000);
}

#[test]
fn the_covered_period_bounds_are_overridable() {
    let json = r#"{
        "live_future_tolerance_secs": 60,
        "live_past_tolerance_secs": 3600,
        "backfill_window_secs": 86400
    }"#;
    let cfg: UsageCollectorConfig = serde_json::from_str(json).expect("config parses");
    assert_eq!(cfg.live_future_tolerance_secs, 60);
    assert_eq!(cfg.live_past_tolerance_secs, 3_600);
    assert_eq!(cfg.backfill_window_secs, 86_400);
}

#[test]
fn a_zero_bound_is_rejected() {
    // A zero future tolerance refuses a period ending a microsecond from
    // now, and a zero past tolerance refuses everything that is not in the
    // future. Both are configuration mistakes that present at runtime as
    // total ingestion failure with a per-entry validation error; failing at
    // `Gear::init` names the key instead.
    let cases = [
        (
            "live_future_tolerance_secs",
            UsageCollectorConfig {
                live_future_tolerance_secs: 0,
                ..Default::default()
            },
        ),
        (
            "live_past_tolerance_secs",
            UsageCollectorConfig {
                live_past_tolerance_secs: 0,
                ..Default::default()
            },
        ),
        (
            "backfill_window_secs",
            UsageCollectorConfig {
                backfill_window_secs: 0,
                ..Default::default()
            },
        ),
    ];
    for (key, cfg) in cases {
        let err = cfg
            .validate()
            .expect_err("a zero bound must be rejected at init");
        assert!(
            err.to_string().contains(key),
            "the rejection must name the offending key; got: {err}"
        );
        // A zero `backfill_window_secs` is also narrower than the default
        // past tolerance, so asserting only on the key name would stay
        // green with the zero check deleted. The register pins which
        // rejection fired.
        assert!(
            err.to_string().contains("must be greater than 0"),
            "the zero rejection must fire, not the ordering one; got: {err}"
        );
    }
}

#[test]
fn a_bound_that_cannot_be_a_duration_is_rejected() {
    // Task 9 converts these to `time::Duration`, whose constructor takes
    // `i64` seconds. A `u64::MAX` tolerance is a configuration mistake, not
    // an infinite bound.
    let cases = [
        (
            "live_future_tolerance_secs",
            UsageCollectorConfig {
                live_future_tolerance_secs: u64::MAX,
                ..Default::default()
            },
        ),
        (
            "live_past_tolerance_secs",
            UsageCollectorConfig {
                live_past_tolerance_secs: u64::MAX,
                ..Default::default()
            },
        ),
        (
            "backfill_window_secs",
            UsageCollectorConfig {
                backfill_window_secs: u64::MAX,
                ..Default::default()
            },
        ),
    ];
    for (key, cfg) in cases {
        let err = cfg
            .validate()
            .expect_err("a bound beyond i64::MAX must be rejected at init");
        assert!(
            err.to_string().contains(key),
            "the rejection must name the offending key; got: {err}"
        );
        // A `u64::MAX` past tolerance also trips the ordering rule, so the
        // key name alone would stay green with the range check deleted.
        assert!(
            err.to_string().contains("i64"),
            "the range rejection must fire, not the ordering one; got: {err}"
        );
    }
}

#[test]
fn a_backfill_window_narrower_than_the_live_past_tolerance_is_rejected() {
    // The live rejection tells an emitter to resubmit on the backfill
    // route. If the window were the narrower of the two, entries the live
    // path refuses would need elevated authorization on the route it names
    // — so the message would be sending an ordinary emitter somewhere it
    // cannot succeed. This is the one ordering among the three bounds that
    // is load-bearing.
    let past = UsageCollectorConfig::default().live_past_tolerance_secs;
    let cfg = UsageCollectorConfig {
        backfill_window_secs: past - 1,
        ..Default::default()
    };
    let err = cfg
        .validate()
        .expect_err("a window narrower than the live past tolerance must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("backfill_window_secs") && msg.contains("live_past_tolerance_secs"),
        "the rejection must name both bounds it orders; got: {err}"
    );
}

#[test]
fn a_backfill_window_equal_to_the_live_past_tolerance_is_accepted() {
    // The ordering is `<`, not `<=`: a window exactly as wide as the live
    // past tolerance still admits every entry the live path turns away, so
    // the route the rejection names remains usable without escalation.
    let past = UsageCollectorConfig::default().live_past_tolerance_secs;
    let cfg = UsageCollectorConfig {
        backfill_window_secs: past,
        ..Default::default()
    };
    assert!(cfg.validate().is_ok(), "{:?}", cfg.validate());
}
