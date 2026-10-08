//! Unit tests for the `[usage_collector]` configuration surface.
//!
//! Only the vendor binding, the covered-period bounds and the serde posture
//! (`#[serde(default, deny_unknown_fields)]`) are exercised here.
//! `types-registry` owns every usage-type declaration and this gear
//! registers no usage-type surface
//! (`cpt-cf-usage-collector-adr-registry-owned-typing`), so there is no
//! host-side declared-catalog surface left to test.

use super::*;

#[test]
fn serde_default_applies_default_vendor() {
    let cfg: UsageCollectorConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(
        cfg.vendor, "constructorfabric",
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
fn the_bounds_project_onto_the_field_that_carries_each_key() {
    // The projection is three same-typed `Duration`s built from three
    // same-typed `u64`s, so a transposed pair type-checks and reads
    // plausibly. Nothing else can catch it: `covered_period_bounds()` is
    // what every enforcement site consumes, and the unit tests over the
    // rule build `CoveredPeriodBounds` by hand and never see this mapping.
    //
    // The overridable values below are deliberately used rather than the
    // defaults — three DISTINCT non-default numbers, so any swap among the
    // three moves a value and fails, where a defaults-only assertion would
    // only be as discriminating as the defaults happen to be.
    let json = r#"{
        "live_future_tolerance_secs": 60,
        "live_past_tolerance_secs": 3600,
        "backfill_window_secs": 86400
    }"#;
    let cfg: UsageCollectorConfig = serde_json::from_str(json).expect("config parses");
    let bounds = cfg.covered_period_bounds();
    assert_eq!(bounds.future_tolerance, time::Duration::minutes(1));
    assert_eq!(bounds.live_past_tolerance, time::Duration::hours(1));
    assert_eq!(bounds.backfill_window, time::Duration::days(1));
}

#[test]
fn the_default_bounds_project_to_five_minutes_forty_eight_hours_and_ninety_days() {
    // The published numbers, read through the projection every ingestion
    // path actually consumes rather than off the `u64` keys.
    // `CoveredPeriodBounds::default()` — what `Service::new` uses — reads
    // the same three domain constants this config default reads, so
    // asserting the two agree is what keeps the pair from drifting apart.
    let projected = UsageCollectorConfig::default().covered_period_bounds();
    assert_eq!(projected.future_tolerance, time::Duration::minutes(5));
    assert_eq!(projected.live_past_tolerance, time::Duration::hours(48));
    assert_eq!(projected.backfill_window, time::Duration::days(90));
    assert_eq!(
        projected,
        crate::domain::covered_period::CoveredPeriodBounds::default(),
        "a Service built without a configured block MUST enforce the same \
         bounds a default deployment configures",
    );
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

// ── max_batch_records ──

#[test]
fn the_batch_cap_defaults_to_100() {
    let cfg: UsageCollectorConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(cfg.max_batch_records, 100);
}

#[test]
fn a_zero_batch_cap_is_rejected() {
    let cfg = UsageCollectorConfig {
        max_batch_records: 0,
        ..Default::default()
    };
    let err = cfg.validate().expect_err("zero batch cap must be rejected");
    assert!(err.to_string().contains("max_batch_records"), "got: {err}");
}

// ── ingestion_quota ──

#[test]
fn burst_entries_below_the_batch_cap_fails_startup_naming_the_key() {
    // DESIGN §3.8: burst_entries must be at least max_batch_records,
    // "otherwise a maximal batch could never be admitted and the entry cap
    // would name a limit no submission can satisfy."
    let cfg = UsageCollectorConfig {
        max_batch_records: 100,
        ingestion_quota: IngestionQuotaConfig {
            burst_entries: 99,
            ..Default::default()
        },
        ..Default::default()
    };

    let err = cfg
        .validate()
        .expect_err("burst below the cap must not start");
    let msg = err.to_string();
    assert!(msg.contains("burst_entries"), "names the key: {msg}");
    assert!(
        msg.contains("max_batch_records"),
        "names the other key: {msg}"
    );
}

#[test]
fn the_ingestion_quota_defaults_match_the_published_values() {
    // DESIGN §3.8's configuration table. This pins
    // `UsageCollectorConfig::default` and nothing more: with input `{}` the
    // `ingestion_quota` key is absent, so the container's own
    // `serde(default)` supplies the whole block and
    // `IngestionQuotaConfig::deserialize` is never called. The block's own
    // attributes are pinned by the two tests below.
    // Observed red under a mutation of those values, so the three assertions
    // pin what `UsageCollectorConfig::default` carries rather than restating
    // whatever it happens to hold. The values reach it from
    // `IngestionQuotaConfig::default` through the field initialiser at
    // `config.rs:188`, so a change on either side reds this.
    let cfg: UsageCollectorConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(cfg.ingestion_quota.sustained_entries_per_sec, 2_000);
    assert_eq!(cfg.ingestion_quota.burst_entries, 4_000);
    assert_eq!(cfg.ingestion_quota.idle_eviction_secs, 900);
}

#[test]
fn the_ingestion_quota_block_can_be_partially_overridden_via_serde() {
    // The job `#[serde(default)]` on `IngestionQuotaConfig` actually does.
    // Red against dropping it: a block naming one key fails to deserialize
    // with "missing field `sustained_entries_per_sec`", so a deployment
    // raising only its capacity would not boot.
    let json = r#"{"ingestion_quota": {"burst_entries": 8000}}"#;
    let cfg: UsageCollectorConfig = serde_json::from_str(json).unwrap();
    assert_eq!(cfg.ingestion_quota.burst_entries, 8_000);
    assert_eq!(
        cfg.ingestion_quota.sustained_entries_per_sec, 2_000,
        "the keys the block did not name keep their published defaults"
    );
    assert_eq!(cfg.ingestion_quota.idle_eviction_secs, 900);
}

#[test]
fn the_ingestion_quota_block_rejects_unknown_fields() {
    // Red against dropping `deny_unknown_fields` from the block: a
    // misspelled key would then be accepted and silently ignored, and the
    // allowance an operator thought they had set would be the default.
    let json = r#"{"ingestion_quota": {"burst_entries": 8000, "bogus": 1}}"#;
    assert!(serde_json::from_str::<UsageCollectorConfig>(json).is_err());
}

#[test]
fn a_zero_ingestion_quota_value_is_rejected_naming_the_key() {
    // Each of the three is a positive count (DESIGN §3.8). A zero rate
    // never refills, a zero capacity admits nothing, and a zero eviction
    // window evicts a bucket the instant it refills. The key name is
    // asserted per case because one shared message would leave an operator
    // reading three keys to find the one at fault.
    let cases = [
        (
            "sustained_entries_per_sec",
            IngestionQuotaConfig {
                sustained_entries_per_sec: 0,
                ..Default::default()
            },
        ),
        (
            "burst_entries",
            IngestionQuotaConfig {
                burst_entries: 0,
                ..Default::default()
            },
        ),
        (
            "idle_eviction_secs",
            IngestionQuotaConfig {
                idle_eviction_secs: 0,
                ..Default::default()
            },
        ),
    ];
    for (key, ingestion_quota) in cases {
        let cfg = UsageCollectorConfig {
            ingestion_quota,
            ..Default::default()
        };
        let err = cfg
            .validate()
            .expect_err("a zero quota value must be rejected at init");
        let msg = err.to_string();
        assert!(
            msg.contains(key),
            "the rejection must name {key}; got: {msg}"
        );
    }
}

// ── unavailable_retry_after_secs / target_not_converged_retry_after_secs ──
//
// DESIGN §3.8 publishes `1` for both, default `1` each. Step 1's own
// decision: zero is refused for both, mirroring `max_batch_records`' `==
// 0` idiom — a zero delay means "retry immediately", which for
// `unavailable_retry_after_secs` defeats the purpose DESIGN gives it ("The
// default stops a caller hot-looping without implying a precision the gear
// does not have"), and symmetrically for `target_not_converged_retry_after_secs`.

#[test]
fn the_retry_delay_defaults_are_the_ones_design_publishes() {
    let cfg: UsageCollectorConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(cfg.unavailable_retry_after_secs, 1);
    assert_eq!(cfg.target_not_converged_retry_after_secs, 1);
}

#[test]
fn the_retry_delays_are_overridable_via_serde() {
    let json = r#"{"unavailable_retry_after_secs": 5, "target_not_converged_retry_after_secs": 9}"#;
    let cfg: UsageCollectorConfig = serde_json::from_str(json).unwrap();
    assert_eq!(cfg.unavailable_retry_after_secs, 5);
    assert_eq!(cfg.target_not_converged_retry_after_secs, 9);
}

#[test]
fn a_zero_unavailable_retry_after_secs_is_rejected() {
    let cfg = UsageCollectorConfig {
        unavailable_retry_after_secs: 0,
        ..Default::default()
    };
    let err = cfg
        .validate()
        .expect_err("a zero unavailable_retry_after_secs must be rejected at init");
    assert!(
        err.to_string().contains("unavailable_retry_after_secs"),
        "got: {err}"
    );
}

#[test]
fn a_zero_target_not_converged_retry_after_secs_is_rejected() {
    let cfg = UsageCollectorConfig {
        target_not_converged_retry_after_secs: 0,
        ..Default::default()
    };
    let err = cfg
        .validate()
        .expect_err("a zero target_not_converged_retry_after_secs must be rejected at init");
    assert!(
        err.to_string()
            .contains("target_not_converged_retry_after_secs"),
        "got: {err}"
    );
}

// ── dod-slo-threshold-provenance : configuration-scan half ──
//
// Second half of `cpt-cf-usage-collector-dod-slo-threshold-provenance`'s
// Assertion: "A scan of the gear's typed configuration finds no key that
// sets or relaxes a latency, throughput, concurrency, or availability
// bound." Two things make a plain `grep` over `config.rs` insufficient as
// the whole test (ruling I60 of the Task 14 brief):
//
// 1. The six-token pattern the brief supplies --
//    `latency|throughput|concurren|availabil|p95|slo` -- is blind to a key
//    whose *name* does not spell one of its tokens. Task 8's
//    `unavailable_retry_after_secs` and
//    `target_not_converged_retry_after_secs` are exactly such keys: neither
//    name contains any of the six tokens, so the pattern cannot see them,
//    and a zero-hit `command grep -nE '...' usage-collector/src/config.rs`
//    proves only that no key was *spelled* inside scope, not that no key
//    was *judged*.
// 2. The pattern's own `availabil` token is one letter wider than the
//    substring that would catch "unavailable": tighten it to `availab` and
//    it matches `unavailable_retry_after_secs` by name -- a key whose
//    disposition this test records rather than leaving to a future
//    reader's grep.
//
// So this test does two things a textual `grep` cannot: (a) it derives the
// judgement table FROM the type via a macro-generated, compile-checked
// destructure, rather than checking the two against each other, and (b) it
// judges each field by its documented behaviour rather than by whether its
// name happens to spell a token.
//
// On (a) -- corrected, this comment previously overclaimed it: a plain
// `UsageCollectorConfig { field: _, .. }` destructure with every field
// bound to `_` *is* compile-checked against the type (a thirteenth field
// not named in the pattern is E0027), but it is **textually independent**
// of a separately hand-written judgement table -- the destructure and the
// table are two different lists that happen to agree on twelve today. A
// reviewer demonstrated the hole: add a thirteenth field documented as
// relaxing a bound, appease the destructure with `field: _,`, leave the
// table at twelve rows, and the full `--lib` lane stayed green (842 passed,
// 0 failed) because nothing ever re-counted the table against the type.
//
// `judged_fields!` below closes that gap by making the table and the
// destructure the SAME list: it takes one `field => (is_bound, reason)`
// entry per field, and from that single list generates both the
// destructure pattern (`UsageCollectorConfig { $($field),+ }`) and the
// returned table (`[(stringify!($field), $is_bound, $reason), ...]`).
// There is no second, independently-typed enumeration for the two to drift
// apart from. A field present on the struct but absent from this
// invocation fails to compile ("pattern does not mention field", E0027,
// same as before); a field named in this invocation that the struct does
// not have fails to compile too ("struct ... does not have this field",
// E0026). Critically, the macro's grammar has **no rule for a bare field
// name or a `_` placeholder** -- only `field => (bool, &str)` -- so naming
// a field at all *is* adding its row. The `field: _`-style minimal
// appeasement the reviewer used has nothing to attach to here: there is no
// freestanding destructure left to appease. See this file's fix-round
// report for the mutation that proves it (restoring the reviewer's exact
// scenario and showing the result).
// @cpt-dod:cpt-cf-usage-collector-dod-slo-threshold-provenance:p1
#[test]
fn no_configuration_key_sets_or_relaxes_a_latency_throughput_concurrency_or_availability_bound() {
    // (field name, is this an SLO bound-setting/relaxing key?, reasoning)
    macro_rules! judged_fields {
        ($($field:ident => ($is_bound:expr, $reason:expr)),+ $(,)?) => {{
            // The one destructure, generated from the list below rather
            // than written out separately. No `..` and no bare `_` member:
            // every field the type has must appear as a `field => (...)`
            // entry or this does not compile.
            let UsageCollectorConfig { $($field),+ } = UsageCollectorConfig::default();
            // Consumes every binding above in the same breath that
            // stringifies its name for the table below -- there is no
            // separate "unused variable" escape hatch to reason about.
            let _ = ($(&$field),+);
            [$((stringify!($field), $is_bound, $reason)),+]
        }};
    }

    let judged: [(&str, bool, &str); 12] = judged_fields![
        vendor => (
            false,
            "selects a storage-plugin implementation; carries no number"
        ),
        metrics => (
            false,
            "instrument-name prefix only; carries no bound"
        ),
        type_cache_ttl_secs => (
            false,
            "cache lifetime -- the DoD text's own named example of an \
             operational parameter that does not restate a bound"
        ),
        type_cache_capacity => (
            false,
            "cache entry ceiling; no latency, throughput, concurrency, or \
             availability figure"
        ),
        metadata_size_cap_bytes => (
            false,
            "payload-size cap enforced by validation; not a timing or \
             availability figure"
        ),
        live_future_tolerance_secs => (
            false,
            "covered-period data-eligibility window (how far ahead an \
             entry's period may end); decides whether an entry is admitted, \
             not how fast or how often the gear answers"
        ),
        live_past_tolerance_secs => (
            false,
            "covered-period data-eligibility window (how far behind); same \
             reasoning as the future tolerance"
        ),
        backfill_window_secs => (
            false,
            "selects which PDP action a backfill entry is authorized \
             against; not a timing or availability figure"
        ),
        max_batch_records => (
            false,
            "batch cap -- the DoD text's own named example of an \
             operational parameter"
        ),
        ingestion_quota => (
            false,
            "per-subject allowance -- the DoD text's own named example \
             (\"quota\")"
        ),
        unavailable_retry_after_secs => (
            false,
            "a client-directed retry-after hint attached to a response the \
             gear has already decided is unavailable; it tells a caller how \
             long to wait before retrying and sets or relaxes nothing about \
             the gear's own latency, throughput, concurrency, or the 99.95% \
             availability target -- availability here is accounted on \
             whether the request was accepted, not on the hint value \
             attached to a rejection"
        ),
        target_not_converged_retry_after_secs => (
            false,
            "a client-directed retry-after hint attached to a conflict \
             response; same reasoning as the key above -- it paces a \
             caller's retry, it does not set or relax a bound this feature \
             states"
        ),
    ];

    assert_eq!(
        judged.len(),
        12,
        "every pub field of UsageCollectorConfig must be judged exactly once"
    );
    for (name, is_bound, reason) in judged {
        assert!(
            !is_bound,
            "{name} judged as an SLO bound ({reason}); \
             dod-slo-threshold-provenance's configuration-scan half would fail"
        );
    }

    // Replay the brief's six-token pattern over the KEY NAMES (not the doc
    // prose, which legitimately discusses "latency" and "availability" in
    // English while describing *other* features' bounds). Zero matches
    // here is consistent with the brief's own `grep` result -- but per
    // ruling I60, this silence is not itself the judgement; the table
    // above is.
    let six_token_hits: Vec<&str> = judged
        .iter()
        .map(|(name, ..)| *name)
        .filter(|name| {
            let lower = name.to_lowercase();
            [
                "latency",
                "throughput",
                "concurren",
                "availabil",
                "p95",
                "slo",
            ]
            .iter()
            .any(|token| lower.contains(token))
        })
        .collect();
    assert!(
        six_token_hits.is_empty(),
        "the six-token pattern unexpectedly matched a key name: {six_token_hits:?}; \
         a positive hit here still requires the judgement table above, it does \
         not resolve it on its own"
    );

    // Ruling I60's near-miss, made concrete: tightening the availability
    // token by one letter (`availabil` -> `availab`) catches a key by name
    // -- `unavailable_retry_after_secs` -- which is exactly why this test
    // records a judgement for that key above instead of trusting the
    // six-token pattern's silence.
    let tightened_hits: Vec<&str> = judged
        .iter()
        .map(|(name, ..)| *name)
        .filter(|name| name.to_lowercase().contains("availab"))
        .collect();
    assert_eq!(
        tightened_hits,
        vec!["unavailable_retry_after_secs"],
        "the tightened pattern must catch exactly the one key ruling I60 names"
    );
}
