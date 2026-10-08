//! The `DESIGN.md` §3.11.5 Operational Metric Inventory, transcribed.
//!
//! **Transcribed by hand from the document, never derived from
//! [`crate::infra::metrics`].** A pin re-sourced from the code it checks is
//! logically implied by that code and therefore vacuous — the defect ruling
//! H51 recorded when a re-sourced contract test would have asserted nothing.
//! The point of this module is to disagree with the adapter when the adapter
//! is wrong.
//!
//! §3.11.5 fixes names, bucket layouts and label vocabularies as *"part of
//! the architectural contract"*, so a divergence here is a conformance
//! defect rather than a style question. Of those three, **this pin covers
//! names and label vocabularies only; bucket layouts are out of its
//! scope** — [`InstrumentSpec`] carries no bucket field. Controller ruling
//! I18 routes the bucket contract to T16, which widens from verifying three
//! histograms' layouts to all ten.
//!
//! **[`InstrumentSpec::labels`] has its first consumer now.** T2's
//! `the_type_resolution_result_vocabulary_matches_design` reads the
//! `uc_type_resolution_total` row below; every other instrument's label
//! value set — roughly 250 of this file's 419 lines — stays unverified
//! transcription until T6's per-instrument vocabulary assertions consume
//! them too. The field's mere *presence* on [`InstrumentSpec`] is not
//! coverage; only a passing consumer is.
//!
//! ## Three tests, two files (controller ruling I13)
//!
//! Spec §5.3 describes one pin covering *"the expected instrument set and
//! each label's declared value set"*. In the shipped shape that is **three**
//! different tests split across **two** files, because one test asserting
//! every instrument's full label cross-product would have to drive every
//! label value of every instrument from one fixture, which no single
//! operation class can do — it would be vacuous over the labels it could not
//! reach while its name claimed otherwise (spec §10 item 1's failure mode).
//! Conflating the three into one claim is the mistake ruling H47 recorded
//! when a reader read two different sets as one, so the split is
//! written down here:
//!
//! - `metrics_inventory_tests.rs`'s
//!   `the_gear_emits_exactly_the_design_inventory` — instrument **names**,
//!   both directions. Catches an absent instrument and an undocumented one.
//! - `metrics_inventory_tests.rs`'s
//!   `no_emitted_instrument_carries_an_unbounded_identifier_as_a_label`
//!   (emission side), plus **two** source-side scans in
//!   `service_metrics_tests.rs`:
//!   `no_label_key_constant_in_ports_metrics_declares_an_unbounded_identifier`,
//!   the load-bearing one — it reads the `key` module's constant *values*,
//!   so it catches a forbidden identifier regardless of what the constant
//!   is named — and `no_metrics_recorder_method_is_passed_an_unbounded_identifier_as_a_label`,
//!   which corroborates it with a per-method token scan a renamed constant
//!   would defeat. Together these are the **cardinality rule**,
//!   cross-instrument, derived from the declarations. Both source-side
//!   scans live in `service_metrics_tests.rs` rather than here because
//!   their shared `production_sources()` oracle is private to that module
//!   and moving it would be a refactor outside this task.
//! - Per-instrument **label value sets** — T2's
//!   `the_type_resolution_result_vocabulary_matches_design` (one row) and
//!   T6's label assertions (the rest), which consume
//!   [`InstrumentSpec::labels`].

/// The three OpenTelemetry instrument shapes §3.11.5 uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstrumentKind {
    Counter,
    Histogram,
    Gauge,
}

/// One §3.11.5 row: the instrument's full literal name under the
/// substitutable `uc_` prefix, its kind, and each label's declared closed
/// value set.
// @cpt-dod:cpt-cf-usage-collector-dod-metric-naming-contract:p2
#[derive(Debug, Clone, Copy)]
pub struct InstrumentSpec {
    pub name: &'static str,
    pub kind: InstrumentKind,
    /// `(label_key, declared_values)`. An unlabelled instrument carries an
    /// empty slice — §3.11.5 writes those rows with a `—` label column. A
    /// labelled instrument whose column names the label but states no
    /// closed set (`operation` is "SPI method name" rather than an
    /// enumeration, on `uc_plugin_accept_errors_total` and
    /// `uc_plugin_call_duration_seconds`) also carries an empty value slice:
    /// the document leaves that vocabulary open, so there is nothing closed
    /// to transcribe.
    pub labels: &'static [(&'static str, &'static [&'static str])],
}

/// DESIGN §3.11.5's 26 instruments: 10 counters, 10 histograms, 6 gauges.
///
/// Transcribed by hand from `docs/DESIGN.md` §3.11.5 (lines `:2232`-`:2271`
/// at transcription time); never derived from
/// [`crate::infra::metrics::UcMetricsMeter`]. See the module doc on H51.
// @cpt-dod:cpt-cf-usage-collector-dod-metric-inventory-completeness:p2
pub const DESIGN_INSTRUMENTS: &[InstrumentSpec] = &[
    // ----- Counters (§3.11.5, 10 rows) -----
    InstrumentSpec {
        name: "uc_ingestion_requests_total",
        kind: InstrumentKind::Counter,
        labels: &[
            ("outcome", &["accepted", "partial", "rejected"]),
            (
                "error_category",
                &[
                    "none",
                    "missing_security_context",
                    "authz",
                    "unresolved_type",
                    "validation",
                    "metadata_size",
                    "quota",
                    "plugin_error",
                ],
            ),
        ],
    },
    InstrumentSpec {
        name: "uc_ingestion_records_total",
        kind: InstrumentKind::Counter,
        labels: &[
            ("outcome", &["accepted", "duplicate", "rejected"]),
            ("entry_type", &["record", "invalidation"]),
            ("origin", &["live", "backfill"]),
            (
                "error_category",
                // Widened this slice (T3, DESIGN.md:2239): `unknown_usage_type`,
                // `semantics_violation` and `metadata_size` are declared —
                // all three are already emitted
                // (`RecordErrorCategory::as_str`), and the declare-or-fold
                // test says declare rather than fold `semantics_violation`
                // / `metadata_size` into `validation`, because folding
                // either would silently empty an existing dashboard while
                // the counter kept reporting. `unresolved_type` and
                // `validation` stay declared too but are currently
                // unreachable (no producer emits either spelling on this
                // counter) — open and unassigned, same as DESIGN.md's row.
                &[
                    "none",
                    "authz",
                    "unknown_usage_type",
                    "unresolved_type",
                    "validation",
                    "semantics_violation",
                    "metadata_size",
                    "idempotency_conflict",
                    "invalidation_rule",
                    "plugin_error",
                ],
            ),
        ],
    },
    InstrumentSpec {
        name: "uc_query_requests_total",
        kind: InstrumentKind::Counter,
        labels: &[
            (
                "query_kind",
                &["aggregated", "raw", "point", "reconciliation"],
            ),
            ("outcome", &["success", "denied", "error"]),
            (
                "error_category",
                // Corrected against DESIGN.md:2247: `unresolved_type` was a
                // stale spelling of the emitted `unknown_usage_type`,
                // `undeclared_field` already folds into `query_budget` by
                // `QueryErrorCategory::QueryBudget`'s own doc, and
                // `missing_time_range` had no producer on any path — the
                // range is a typed parameter rejected at the edge, before
                // this counter is reached. `filter_mismatch` is emitted by
                // `classify_query_result` and was undeclared.
                //
                // `order_mismatch` stays undeclared on purpose:
                // `QueryErrorCategory::OrderMismatch` is constructed nowhere
                // in non-test code, so declaring it would replace two dead
                // labels with a third. It is the one thing that still blocks
                // a `the_query_error_category_vocabulary_matches_design`
                // pin here, since that test shape walks every variant.
                //
                // `record_not_found` joined this row because the by-id
                // point lookup's miss is an
                // absent row, not the unresolvable meter declaration
                // `unknown_usage_type` names, and the two used to share
                // that one series.
                &[
                    "none",
                    "missing_security_context",
                    "authz",
                    "unknown_usage_type",
                    "record_not_found",
                    "cursor_decode",
                    "filter_mismatch",
                    "query_budget",
                    "plugin_error",
                ],
            ),
        ],
    },
    InstrumentSpec {
        name: "uc_feed_requests_total",
        kind: InstrumentKind::Counter,
        labels: &[
            ("outcome", &["success", "denied", "error"]),
            (
                "error_category",
                // Ruling E15 + I6 (DESIGN.md:2241): six values, not five — a
                // `limit` / subscription-breadth argument rejection gained
                // `argument_rejected` (a name of its own, not the query
                // counter's `query_budget` — see
                // `FeedErrorCategory::ArgumentRejected`'s own doc for why),
                // and the PDP-outage fold stays on `plugin_error` (ruled,
                // not widened).
                &[
                    "none",
                    "authz",
                    "cursor_decode",
                    "cursor_beyond_retention",
                    "argument_rejected",
                    "plugin_error",
                ],
            ),
        ],
    },
    InstrumentSpec {
        name: "uc_type_resolution_total",
        kind: InstrumentKind::Counter,
        labels: &[(
            "result",
            // Ruling I5 (DESIGN.md:2242): six values, not five — the gear
            // had been emitting `served_stale` undeclared.
            &[
                "cache_hit",
                "cache_miss",
                "served_stale",
                "restored",
                "unresolved",
                "registry_error",
            ],
        )],
    },
    // An unlabelled row — §3.11.5 writes its label column as `—`.
    InstrumentSpec {
        name: "uc_ingestion_quota_rejections_total",
        kind: InstrumentKind::Counter,
        labels: &[],
    },
    InstrumentSpec {
        name: "uc_declaration_mirror_write_failures_total",
        kind: InstrumentKind::Counter,
        labels: &[],
    },
    InstrumentSpec {
        name: "uc_pdp_failures_total",
        kind: InstrumentKind::Counter,
        labels: &[
            (
                "operation",
                &[
                    "ingest",
                    "backfill",
                    "query_raw",
                    "query_aggregated",
                    "get_record",
                    "read_feed",
                    "reconciliation",
                ],
            ),
            ("cause", &["unreachable", "timeout"]),
        ],
    },
    InstrumentSpec {
        name: "uc_authz_decisions_total",
        kind: InstrumentKind::Counter,
        labels: &[
            (
                "operation",
                &[
                    "ingest",
                    "backfill",
                    "query_raw",
                    "query_aggregated",
                    "get_record",
                    "read_feed",
                    "reconciliation",
                ],
            ),
            ("decision", &["permit", "deny"]),
        ],
    },
    InstrumentSpec {
        name: "uc_plugin_accept_errors_total",
        kind: InstrumentKind::Counter,
        // `operation` names the SPI method dispatched; §3.11.5 states it as
        // "(SPI method name)" rather than an enumerated set, so there is no
        // closed vocabulary to transcribe — see the `labels` field doc.
        labels: &[
            ("operation", &[]),
            ("error_category", &["unready", "backend_error", "timeout"]),
        ],
    },
    // ----- Histograms (§3.11.5, 10 rows) -----
    InstrumentSpec {
        name: "uc_ingestion_duration_seconds",
        kind: InstrumentKind::Histogram,
        labels: &[("origin", &["live", "backfill"])],
    },
    InstrumentSpec {
        name: "uc_query_duration_seconds",
        kind: InstrumentKind::Histogram,
        labels: &[("query_kind", &["aggregated", "raw", "point"])],
    },
    InstrumentSpec {
        name: "uc_feed_page_duration_seconds",
        kind: InstrumentKind::Histogram,
        labels: &[],
    },
    InstrumentSpec {
        name: "uc_plugin_call_duration_seconds",
        kind: InstrumentKind::Histogram,
        // Open vocabulary — see `uc_plugin_accept_errors_total` above.
        labels: &[("operation", &[])],
    },
    InstrumentSpec {
        name: "uc_pdp_duration_seconds",
        kind: InstrumentKind::Histogram,
        labels: &[(
            "operation",
            &[
                "ingest",
                "backfill",
                "query_raw",
                "query_aggregated",
                "get_record",
                "read_feed",
                "reconciliation",
            ],
        )],
    },
    InstrumentSpec {
        name: "uc_type_resolution_duration_seconds",
        kind: InstrumentKind::Histogram,
        labels: &[("result", &["cache_hit", "cache_miss"])],
    },
    InstrumentSpec {
        name: "uc_ingestion_batch_size",
        kind: InstrumentKind::Histogram,
        labels: &[],
    },
    InstrumentSpec {
        name: "uc_record_metadata_bytes",
        kind: InstrumentKind::Histogram,
        labels: &[],
    },
    InstrumentSpec {
        name: "uc_query_result_rows",
        kind: InstrumentKind::Histogram,
        labels: &[("query_kind", &["aggregated", "raw"])],
    },
    InstrumentSpec {
        name: "uc_feed_page_entries",
        kind: InstrumentKind::Histogram,
        labels: &[],
    },
    // ----- Gauges (§3.11.5, 6 rows) -----
    InstrumentSpec {
        name: "uc_plugin_ready",
        kind: InstrumentKind::Gauge,
        labels: &[],
    },
    InstrumentSpec {
        name: "uc_pdp_ready",
        kind: InstrumentKind::Gauge,
        labels: &[],
    },
    InstrumentSpec {
        name: "uc_query_inflight",
        kind: InstrumentKind::Gauge,
        labels: &[("query_kind", &["aggregated", "raw"])],
    },
    InstrumentSpec {
        name: "uc_resolved_types",
        kind: InstrumentKind::Gauge,
        labels: &[],
    },
    InstrumentSpec {
        name: "uc_declaration_cache_age_seconds",
        kind: InstrumentKind::Gauge,
        labels: &[],
    },
    InstrumentSpec {
        name: "uc_ingestion_quota_buckets_active",
        kind: InstrumentKind::Gauge,
        labels: &[],
    },
];

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "metrics_inventory_tests.rs"]
mod metrics_inventory_tests;
