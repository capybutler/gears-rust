//! The `DESIGN.md` §3.11.5 Operational Metric Inventory, transcribed.
//!
//! **Transcribed by hand from the document, never derived from
//! [`crate::v2::infra::metrics`].** A pin re-sourced from the code it checks
//! asserts nothing; the point of this module is to disagree with the adapter
//! when the adapter is wrong.
//!
//! §3.11.5 fixes names, bucket layouts and label vocabularies as *"part of
//! the architectural contract"*, so a divergence here is a conformance defect
//! rather than a style question. Of those three, **this pin covers names and
//! label vocabularies only; bucket layouts are out of its scope** —
//! [`InstrumentSpec`] carries no bucket field, and the histogram layouts are
//! verified in `metrics_tests.rs` instead.
//!
//! [`InstrumentSpec::labels`] is read by the per-instrument vocabulary tests
//! for the instruments those name; every other instrument's label value set
//! stays unverified transcription until an assertion consumes it too.

/// The OpenTelemetry instrument shapes §3.11.5 uses.
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

/// DESIGN §3.11.5's instrument inventory.
///
/// Transcribed by hand from `docs/DESIGN.md` §3.11.5 (lines `:2232`-`:2271`
/// at transcription time); never derived from
/// [`crate::v2::infra::metrics::UcMetricsMeter`]. See the module doc above.
// @cpt-dod:cpt-cf-usage-collector-dod-metric-inventory-completeness:p2
pub const DESIGN_INSTRUMENTS: &[InstrumentSpec] = &[
    // ----- Counters (§3.11.5) -----
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
                // Widened (DESIGN.md:2239): `unknown_usage_type`,
                // `semantics_violation` and `metadata_size` are declared
                // rather than folded into `validation`, because folding
                // either would silently empty an existing dashboard while the
                // counter kept reporting. `unresolved_type` and `validation`
                // stay declared too but are currently unreachable, same as
                // DESIGN.md's row.
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
                // Matches DESIGN.md:2247. `undeclared_field` folds into
                // `query_budget`; `missing_time_range` has no producer on any
                // path, the range being a typed parameter rejected at the
                // edge. `order_mismatch` stays undeclared on purpose:
                // `QueryErrorCategory::OrderMismatch` is constructed nowhere
                // in non-test code. `record_not_found` is its own row: the
                // by-id point lookup's miss is an absent row, not the
                // unresolvable meter declaration `unknown_usage_type` names.
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
                // Six values, not five (DESIGN.md:2241) — a
                // `limit` / subscription-breadth argument rejection gained
                // `argument_rejected` (see
                // `FeedErrorCategory::ArgumentRejected`'s own doc), and the
                // PDP-outage fold stays on `plugin_error`.
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
            // Six values, not five (DESIGN.md:2242) — the gear
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
    // ----- Histograms (§3.11.5) -----
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
    // ----- Gauges (§3.11.5) -----
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
