//! OpenTelemetry-backed implementation of
//! [`UsageCollectorMetrics`](crate::domain::ports::metrics::UsageCollectorMetrics).
//!
//! Instruments are declared on a scoped `Meter` from `ToolKit`'s **global**
//! `SdkMeterProvider` at gear bootstrap. The gear constructs no exporter and
//! exposes no `/metrics` scrape endpoint — telemetry is OTLP-pushed by `ToolKit`
//! (`cpt-cf-usage-collector-principle-otlp-push-emission`) — and the platform
//! `http.server.*` instruments are NOT redeclared here
//! (`cpt-cf-usage-collector-principle-gateway-http-server-instrument-reuse`).
//!
//! Names are the **full literal** Prometheus names from DESIGN §3.11.5
//! (`_total` on counters, `_seconds` on duration histograms) with **no**
//! `.with_unit(...)` hint, so the rendered series name is identical whether
//! the downstream collector runs with `add_metric_suffixes` on or off.

use std::sync::Arc;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};

use crate::domain::ports::metrics::{
    AuthzDecision, FeedErrorCategory, IngestRequestErrorCategory, IngestRequestOutcome,
    PdpFailureCause, PdpOp, PluginErrorCategory, PluginOp, QueryErrorCategory, QueryKind,
    RecordErrorCategory, RecordOutcome, RequestOutcome, TypeResolutionOutcome,
    UsageCollectorMetrics, key,
};
use usage_collector_sdk::{EntryType, RecordOrigin};

// Second marker site for this DoD. `InstrumentSpec`
// (`infra/metrics_inventory.rs`) carries the names/kind/label-vocabulary part
// but deliberately not the histogram-bucket-layout part ("this pin covers names
// and label vocabularies only; bucket layouts are out of its scope"). The
// bucket-layout clause ("Histogram buckets MUST bracket the published latency
// budgets... narrowing a bucket layout MUST be treated as a breaking change") is
// realized here instead, across the bucket-boundary constants below and
// `ingestion_batch_size_buckets`' ladder, each of whose docs states the DESIGN
// §3.11.5 budget or cap it brackets. An identifier realized in more than one
// place carries a marker in each; `UcMetricsMeter::new` carries the third, for
// the prefix-substitution mechanism.
// @cpt-dod:cpt-cf-usage-collector-dod-metric-naming-contract:p2
/// Bucket boundaries (seconds) for `uc_pdp_duration_seconds` — brackets the
/// PDP share of the 200 ms ingestion p95 budget (DESIGN §3.11.5).
const PDP_DURATION_BUCKETS_SECONDS: [f64; 9] =
    [0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5];

/// Bucket boundaries (seconds) for `uc_plugin_call_duration_seconds` —
/// separates plugin-owned time from gear overhead (DESIGN §3.11.5).
const PLUGIN_CALL_DURATION_BUCKETS_SECONDS: [f64; 10] =
    [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0];

/// Buckets (seconds) for `uc_ingestion_duration_seconds` — bracket the
/// 200 ms ingestion p95 budget (DESIGN §3.11.5).
const INGESTION_DURATION_BUCKETS_SECONDS: [f64; 9] =
    [0.01, 0.025, 0.05, 0.1, 0.15, 0.2, 0.3, 0.5, 1.0];

/// Buckets (seconds) for `uc_query_duration_seconds` — bracket the 500 ms
/// aggregated-query p95 budget (DESIGN §3.11.5).
const QUERY_DURATION_BUCKETS_SECONDS: [f64; 8] = [0.05, 0.1, 0.25, 0.5, 0.75, 1.0, 2.0, 5.0];

/// The fixed ladder `uc_ingestion_batch_size` buckets are cut from.
const INGESTION_BATCH_SIZE_LADDER: [f64; 7] = [1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0];

/// Buckets for `uc_ingestion_batch_size` (records per request): the ladder
/// entries below the configured cap, then the cap itself, so the upper bucket
/// is always the cap (DESIGN §3.11.5).
#[must_use]
pub fn ingestion_batch_size_buckets(cap: usize) -> Vec<f64> {
    // A cap is a count of entries, far below 2^52, so the cast is exact.
    #[allow(clippy::cast_precision_loss)]
    let cap = cap as f64;
    INGESTION_BATCH_SIZE_LADDER
        .iter()
        .copied()
        .filter(|bound| *bound < cap)
        .chain(std::iter::once(cap))
        .collect()
}

/// Buckets (bytes) for `uc_record_metadata_bytes` — upper bucket equals the
/// 8 KiB metadata cap (DESIGN §3.11.5).
const RECORD_METADATA_BYTES_BUCKETS: [f64; 6] = [256.0, 512.0, 1024.0, 2048.0, 4096.0, 8192.0];

/// Buckets for `uc_query_result_rows` (rows per response) — DESIGN §3.11.5.
const QUERY_RESULT_ROWS_BUCKETS: [f64; 8] =
    [1.0, 10.0, 50.0, 100.0, 500.0, 1000.0, 10000.0, 100_000.0];

/// Buckets (seconds) for `uc_feed_page_duration_seconds` (DESIGN §3.11.5).
const FEED_DURATION_BUCKETS_SECONDS: [f64; 8] = [0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0];

/// Buckets (entries per page) for `uc_feed_page_entries` (DESIGN §3.11.5).
/// The top boundary sits above `crate::domain::feed`'s `MAX_FEED_LIMIT`, so
/// the overflow bucket stays empty for any page a conforming gateway serves.
const FEED_PAGE_ENTRIES_BUCKETS: [f64; 7] = [1.0, 10.0, 50.0, 100.0, 500.0, 1000.0, 5000.0];

/// Buckets (seconds) for `uc_type_resolution_duration_seconds`
/// (DESIGN §3.11.5). Sub-millisecond at the low end because a cache hit is
/// an in-memory map read, not I/O.
const TYPE_RESOLUTION_DURATION_BOUNDARIES_SECS: [f64; 7] =
    [0.0001, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5];

/// The full OpenTelemetry instrument set: the foundation-owned plugin-host +
/// PDP-helper instruments (Phase 1) plus the per-component gateway
/// instruments for ingestion, query and the feed (Phase 2), plus the
/// Type Resolver instrument that replaced the deleted usage-type catalog
/// counters.
// @cpt-flow:cpt-cf-usage-collector-flow-diagnose-from-telemetry:p2
pub struct UcMetricsMeter {
    // ── Plugin-host (owned by foundation §2.1) ──
    plugin_ready: Gauge<i64>,
    plugin_accept_errors: Counter<u64>,
    plugin_call_duration_seconds: Histogram<f64>,

    // ── PDP helper (owned by foundation §2.1) ──
    pdp_ready: Gauge<i64>,
    pdp_failures: Counter<u64>,
    pdp_duration_seconds: Histogram<f64>,
    authz_decisions: Counter<u64>,

    // ── Ingestion gateway (§2.3 usage-emission) ──
    ingestion_requests: Counter<u64>,
    ingestion_records: Counter<u64>,
    ingestion_duration_seconds: Histogram<f64>,
    ingestion_batch_size: Histogram<f64>,
    record_metadata_bytes: Histogram<f64>,
    ingestion_quota_rejections: Counter<u64>,
    ingestion_quota_buckets_active: Gauge<i64>,

    // ── Query gateway (§2.4 usage-query) ──
    query_requests: Counter<u64>,
    query_duration_seconds: Histogram<f64>,
    query_inflight: opentelemetry::metrics::UpDownCounter<i64>,
    query_result_rows: Histogram<f64>,

    // ── Feed gateway (§2.5 billing-usage-feed) ──
    feed_requests: Counter<u64>,
    feed_page_duration_seconds: Histogram<f64>,
    feed_page_entries: Histogram<f64>,

    // ── Type Resolver (§2.2 usage-type-lifecycle successor) ──
    type_resolution: Counter<u64>,
    resolved_types: Gauge<i64>,
    declaration_cache_age_seconds: Gauge<i64>,
    type_resolution_duration_seconds: Histogram<f64>,
    declaration_mirror_write_failures: Counter<u64>,
}

impl UcMetricsMeter {
    /// Build every foundation-owned instrument on `meter`. `prefix` is the
    /// substitutable leading namespace segment (`uc` by default) — the
    /// rendered names are `{prefix}_...`.
    ///
    // Third marker site for `dod-metric-naming-contract`: its "Names MUST share
    // one substitutable prefix" clause names this function by its own mechanism
    // — "the contract is checked where the instruments are constructed, which is
    // the bootstrap path every component shares" — and every field below is
    // built from `format!("{prefix}_...")`. The other sites realize the
    // bucket-layout and name/label-vocabulary parts.
    // @cpt-dod:cpt-cf-usage-collector-dod-metric-naming-contract:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-observability-plugin-host-instruments:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-observability-pdp-helper-instruments:p1
    // @cpt-dod:cpt-cf-usage-collector-principle-otlp-push-emission:p2
    // @cpt-dod:cpt-cf-usage-collector-nfr-operational-visibility:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-metric-label-cardinality-bound:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-observability-alert-integration:p2
    // @cpt-begin:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-binding-meter-bootstrap
    #[must_use]
    pub fn new(meter: &Meter, prefix: &str, max_batch_records: usize) -> Self {
        Self {
            plugin_ready: meter
                .i64_gauge(format!("{prefix}_plugin_ready"))
                .with_description(
                    "1 iff the active storage-plugin binding is structurally resolved",
                )
                .build(),
            plugin_accept_errors: meter
                .u64_counter(format!("{prefix}_plugin_accept_errors_total"))
                .with_description(
                    "Plugin SPI dispatch failures by operation and error_category \
                     (unready / backend_error / timeout)",
                )
                .build(),
            plugin_call_duration_seconds: meter
                .f64_histogram(format!("{prefix}_plugin_call_duration_seconds"))
                .with_description("Plugin SPI dispatch wall-clock by operation")
                .with_boundaries(PLUGIN_CALL_DURATION_BUCKETS_SECONDS.to_vec())
                .build(),
            pdp_ready: meter
                .i64_gauge(format!("{prefix}_pdp_ready"))
                .with_description(
                    "1 while the authz-resolver client is bound in the PolicyEnforcer",
                )
                .build(),
            pdp_failures: meter
                .u64_counter(format!("{prefix}_pdp_failures_total"))
                .with_description("PDP authorization transport/evaluation failures by operation")
                .build(),
            pdp_duration_seconds: meter
                .f64_histogram(format!("{prefix}_pdp_duration_seconds"))
                .with_description("PDP access_scope_with round-trip by operation")
                .with_boundaries(PDP_DURATION_BUCKETS_SECONDS.to_vec())
                .build(),
            authz_decisions: meter
                .u64_counter(format!("{prefix}_authz_decisions_total"))
                .with_description("Completed PDP decisions by operation and decision (permit/deny)")
                .build(),

            // ── Ingestion gateway ──
            ingestion_requests: meter
                .u64_counter(format!("{prefix}_ingestion_requests_total"))
                .with_description(
                    "Completed ingestion batch requests by outcome and error_category",
                )
                .build(),
            ingestion_records: meter
                .u64_counter(format!("{prefix}_ingestion_records_total"))
                .with_description(
                    "Per-record ingestion acknowledgements by outcome, entry_type, origin, \
                     error_category",
                )
                .build(),
            ingestion_duration_seconds: meter
                .f64_histogram(format!("{prefix}_ingestion_duration_seconds"))
                .with_description("Ingestion request wall-clock by origin")
                .with_boundaries(INGESTION_DURATION_BUCKETS_SECONDS.to_vec())
                .build(),
            ingestion_batch_size: meter
                .f64_histogram(format!("{prefix}_ingestion_batch_size"))
                .with_description("Records per received batch submission")
                .with_boundaries(ingestion_batch_size_buckets(max_batch_records))
                .build(),
            record_metadata_bytes: meter
                .f64_histogram(format!("{prefix}_record_metadata_bytes"))
                .with_description("Serialized RecordMetadata size in bytes per submitted record")
                .with_boundaries(RECORD_METADATA_BYTES_BUCKETS.to_vec())
                .build(),
            // Both quota instruments are declared with NO label at all: DESIGN
            // §3.11.5 gives `uc_ingestion_quota_rejections_total` and
            // `uc_ingestion_quota_buckets_active` a label column of `—`, and
            // its cardinality paragraph says the quota "is unlabelled — there is
            // deliberately no per-tenant tier to carry even in aggregate
            // (§3.2)". That is a back-reference to §3.2, not a dimension: no
            // configuration key produces a tier. The rows are the inventory and
            // they say `—`.
            ingestion_quota_rejections: meter
                .u64_counter(format!("{prefix}_ingestion_quota_rejections_total"))
                .with_description(
                    "Entries refused by the per-subject ingestion quota, summed over the \
                     submitted entry count of each refused submission",
                )
                .build(),
            ingestion_quota_buckets_active: meter
                .i64_gauge(format!("{prefix}_ingestion_quota_buckets_active"))
                .with_description(
                    "Resident per-subject ingestion quota buckets on this replica \
                     (per-instance: aggregate with max or last, never sum)",
                )
                .build(),

            // ── Query gateway ──
            // @cpt-dod:cpt-cf-usage-collector-nfr-operational-visibility:p2
            query_requests: meter
                .u64_counter(format!("{prefix}_query_requests_total"))
                .with_description("Completed query attempts by query_kind, outcome, error_category")
                .build(),
            query_duration_seconds: meter
                .f64_histogram(format!("{prefix}_query_duration_seconds"))
                .with_description("Query wall-clock by query_kind")
                .with_boundaries(QUERY_DURATION_BUCKETS_SECONDS.to_vec())
                .build(),
            query_inflight: meter
                .i64_up_down_counter(format!("{prefix}_query_inflight"))
                .with_description("Currently in-flight queries by query_kind")
                .build(),
            query_result_rows: meter
                .f64_histogram(format!("{prefix}_query_result_rows"))
                .with_description("Rows/groups returned per successful query by query_kind")
                .with_boundaries(QUERY_RESULT_ROWS_BUCKETS.to_vec())
                .build(),

            // ── Feed gateway ──
            feed_requests: meter
                .u64_counter(format!("{prefix}_feed_requests_total"))
                .with_description("Feed page requests by outcome and error category")
                .build(),
            feed_page_duration_seconds: meter
                .f64_histogram(format!("{prefix}_feed_page_duration_seconds"))
                .with_description("Feed page request wall-clock")
                .with_boundaries(FEED_DURATION_BUCKETS_SECONDS.to_vec())
                .build(),
            feed_page_entries: meter
                .f64_histogram(format!("{prefix}_feed_page_entries"))
                .with_description("Entries served on one feed page")
                .with_boundaries(FEED_PAGE_ENTRIES_BUCKETS.to_vec())
                .build(),

            // ── Type Resolver ──
            type_resolution: meter
                .u64_counter(format!("{prefix}_type_resolution_total"))
                .with_description(
                    "Completed Type Resolver calls by result (cache_hit / cache_miss / \
                     served_stale / restored / unresolved / registry_error)",
                )
                .build(),
            resolved_types: meter
                .i64_gauge(format!("{prefix}_resolved_types"))
                .with_description(
                    "Current entry count of the resolved-declaration cache \
                     (per-instance: aggregate with max or last, never sum)",
                )
                .build(),
            declaration_cache_age_seconds: meter
                .i64_gauge(format!("{prefix}_declaration_cache_age_seconds"))
                .with_description(
                    "Age in seconds of the oldest cached declaration since its last \
                     successful refresh; unset while the cache is empty",
                )
                .build(),
            type_resolution_duration_seconds: meter
                .f64_histogram(format!("{prefix}_type_resolution_duration_seconds"))
                .with_description(
                    "Type Resolver call wall-clock by result (cache_hit / cache_miss)",
                )
                .with_boundaries(TYPE_RESOLUTION_DURATION_BOUNDARIES_SECS.to_vec())
                .build(),
            declaration_mirror_write_failures: meter
                .u64_counter(format!("{prefix}_declaration_mirror_write_failures_total"))
                .with_description(
                    "Declarations resolved but not mirrored, so a later registry restart \
                     will not restore them (ingestion is unaffected)",
                )
                .build(),
        }
    }
    // @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-binding-meter-bootstrap
}

impl UsageCollectorMetrics for UcMetricsMeter {
    fn set_pdp_ready(&self, ready: bool) {
        self.pdp_ready.record(i64::from(ready), &[]);
    }

    fn record_pdp_decision(&self, op: PdpOp, decision: AuthzDecision, seconds: f64) {
        self.pdp_duration_seconds
            .record(seconds, &[KeyValue::new(key::OPERATION, op.as_str())]);
        self.authz_decisions.add(
            1,
            &[
                KeyValue::new(key::OPERATION, op.as_str()),
                KeyValue::new(key::DECISION, decision.as_str()),
            ],
        );
    }

    fn record_pdp_failure(&self, op: PdpOp, cause: PdpFailureCause, seconds: f64) {
        self.pdp_duration_seconds
            .record(seconds, &[KeyValue::new(key::OPERATION, op.as_str())]);
        self.pdp_failures.add(
            1,
            &[
                KeyValue::new(key::OPERATION, op.as_str()),
                KeyValue::new(key::CAUSE, cause.as_str()),
            ],
        );
    }

    fn set_plugin_ready(&self, ready: bool) {
        self.plugin_ready.record(i64::from(ready), &[]);
    }

    fn record_plugin_call(&self, op: PluginOp, seconds: f64) {
        self.plugin_call_duration_seconds
            .record(seconds, &[KeyValue::new(key::OPERATION, op.as_str())]);
    }

    fn record_plugin_accept_error(&self, op: PluginOp, category: PluginErrorCategory) {
        self.plugin_accept_errors.add(
            1,
            &[
                KeyValue::new(key::OPERATION, op.as_str()),
                KeyValue::new(key::ERROR_CATEGORY, category.as_str()),
            ],
        );
    }

    // ── Ingestion gateway ──

    fn observe_ingestion_batch_size(&self, size: u64) {
        // f64 histogram; batch sizes (1..=cap) are exactly representable.
        #[allow(clippy::cast_precision_loss)]
        self.ingestion_batch_size.record(size as f64, &[]);
    }

    // @cpt-dod:cpt-cf-usage-collector-dod-ingestion-telemetry:p1
    //
    // The REALIZING EMITTER for this DoD's "ingestion duration histogram
    // labelled by origin" sentence — one `record(seconds, ..)` per completed
    // request, with `RecordOrigin::as_str` as the label value. Its twin marker on
    // the port carries the label-mandate half.
    fn observe_ingestion_duration(&self, seconds: f64, origin: RecordOrigin) {
        self.ingestion_duration_seconds
            .record(seconds, &[KeyValue::new(key::ORIGIN, origin.as_str())]);
    }

    fn observe_record_metadata_bytes(&self, bytes: u64) {
        #[allow(clippy::cast_precision_loss)]
        self.record_metadata_bytes.record(bytes as f64, &[]);
    }

    // @cpt-dod:cpt-cf-usage-collector-dod-ingestion-telemetry:p1
    //
    // The REALIZING EMITTER for this DoD's "one counter observation per entry,
    // labelled with the outcome as accepted, duplicate or rejected, with the
    // entry type, with the origin marker, and with an error category on a
    // rejection" sentence — one `add(1, ..)` per call, every label value read off
    // the typed arguments. Its twin marker on the port carries the
    // label-mandate half.
    fn record_ingestion_record(
        &self,
        outcome: RecordOutcome,
        entry_type: EntryType,
        origin: RecordOrigin,
        error_category: RecordErrorCategory,
    ) {
        self.ingestion_records.add(
            1,
            &[
                KeyValue::new(key::OUTCOME, outcome.as_str()),
                KeyValue::new(key::ENTRY_TYPE, entry_type.as_str()),
                KeyValue::new(key::ORIGIN, origin.as_str()),
                KeyValue::new(key::ERROR_CATEGORY, error_category.as_str()),
            ],
        );
    }

    // @cpt-dod:cpt-cf-usage-collector-dod-ingestion-telemetry:p1
    //
    // The REALIZING EMITTER for this DoD's "one counter observation per
    // submission request, labelled with a request-wide outcome and a
    // request-wide error category" sentence — one `add(1, ..)` per completed
    // submission, both label values read off the typed arguments. Its twin
    // marker on the port carries the label-mandate half.
    fn record_ingestion_request(
        &self,
        outcome: IngestRequestOutcome,
        error_category: IngestRequestErrorCategory,
    ) {
        self.ingestion_requests.add(
            1,
            &[
                KeyValue::new(key::OUTCOME, outcome.as_str()),
                KeyValue::new(key::ERROR_CATEGORY, error_category.as_str()),
            ],
        );
    }

    fn record_quota_rejection(&self, entries: u64) {
        // `entries`, never `1`: the counter carries throttled volume
        // (DESIGN §3.11.5, `:2243`). Empty attribute slice — this instrument
        // has no labels at all.
        self.ingestion_quota_rejections.add(entries, &[]);
    }

    fn set_quota_buckets_active(&self, count: u64) {
        // An `i64` gauge like every other in this adapter; the port takes the
        // `u64` the bucket map's length naturally is and the narrowing happens
        // here, the division of labour `observe_ingestion_batch_size`'s
        // `u64` → `f64` already uses. A count reaching `i64::MAX` would mean the
        // map had outgrown addressable memory, so the saturation is a lint
        // obligation rather than a reachable case.
        self.ingestion_quota_buckets_active
            .record(i64::try_from(count).unwrap_or(i64::MAX), &[]);
    }

    // ── Query gateway ──

    fn query_inflight_inc(&self, kind: QueryKind) {
        // DESIGN §3.11.5's `uc_query_inflight` row admits `aggregated`, `raw`
        // alone. See `QueryKind::admits_inflight_gauge`.
        if kind.admits_inflight_gauge() {
            self.query_inflight
                .add(1, &[KeyValue::new(key::QUERY_KIND, kind.as_str())]);
        }
    }

    fn query_inflight_dec(&self, kind: QueryKind) {
        // Mirrors the increment's gate exactly, so a `kind` the gauge was
        // never bumped for is never drained for either.
        if kind.admits_inflight_gauge() {
            self.query_inflight
                .add(-1, &[KeyValue::new(key::QUERY_KIND, kind.as_str())]);
        }
    }

    fn observe_query_result_rows(&self, kind: QueryKind, rows: u64) {
        if kind.admits_result_rows_sample() {
            #[allow(clippy::cast_precision_loss)]
            self.query_result_rows.record(
                rows as f64,
                &[KeyValue::new(key::QUERY_KIND, kind.as_str())],
            );
        }
    }

    fn record_query_request(
        &self,
        kind: QueryKind,
        outcome: RequestOutcome,
        error_category: QueryErrorCategory,
        seconds: f64,
    ) {
        // DESIGN §3.11.5's `uc_query_duration_seconds` row admits
        // `aggregated`, `raw`, `point` — not `reconciliation`, which carries
        // no latency budget (§3.11.2). See `QueryKind::admits_duration_sample`.
        if kind.admits_duration_sample() {
            self.query_duration_seconds
                .record(seconds, &[KeyValue::new(key::QUERY_KIND, kind.as_str())]);
        }
        self.query_requests.add(
            1,
            &[
                KeyValue::new(key::QUERY_KIND, kind.as_str()),
                KeyValue::new(key::OUTCOME, outcome.as_str()),
                KeyValue::new(key::ERROR_CATEGORY, error_category.as_str()),
            ],
        );
    }

    // ── Feed gateway ──

    fn record_feed_request(
        &self,
        outcome: RequestOutcome,
        error_category: FeedErrorCategory,
        seconds: f64,
    ) {
        self.feed_page_duration_seconds.record(seconds, &[]);
        self.feed_requests.add(
            1,
            &[
                KeyValue::new(key::OUTCOME, outcome.as_str()),
                KeyValue::new(key::ERROR_CATEGORY, error_category.as_str()),
            ],
        );
    }

    fn observe_feed_page_entries(&self, entries: u64) {
        #[allow(clippy::cast_precision_loss)]
        self.feed_page_entries.record(entries as f64, &[]);
    }

    // ── Type Resolver ──

    // @cpt-dod:cpt-cf-usage-collector-dod-resolution-telemetry:p1
    //
    // The REALIZING EMITTER for this DoD's first MUST clause — one counter
    // observation per resolution, labelled with the declared `result` values.
    // One `add(1, ..)` per call, the label value being
    // `TypeResolutionOutcome::as_str`, so every spelling reaches the wire from
    // here and nowhere else. Twin markers on the port: the enum carries the
    // vocabulary half, the trait method the one-per-resolution half.
    fn record_type_resolution(&self, outcome: TypeResolutionOutcome) {
        self.type_resolution
            .add(1, &[KeyValue::new(key::RESULT, outcome.as_str())]);
    }

    fn set_resolved_types(&self, count: u64) {
        // An `i64` gauge like every other gauge in this adapter; see
        // `set_quota_buckets_active`'s comment for why the saturating
        // narrowing is a lint obligation rather than a reachable case.
        self.resolved_types
            .record(i64::try_from(count).unwrap_or(i64::MAX), &[]);
    }

    // @cpt-dod:cpt-cf-usage-collector-dod-resolution-telemetry:p1
    //
    // The REALIZING EMITTER for this DoD's "MUST publish the age of the oldest
    // served declaration" clause. The `None` arm below is why the clause is met
    // rather than merely attempted: an empty cache publishes NOTHING, where a
    // `0` would read as perfectly fresh and satisfy §3.11.6's staleness alert
    // forever. Twin marker on the port method, which carries the freeze property
    // this emitter cannot state.
    fn set_declaration_cache_age_seconds(&self, age: Option<u64>) {
        // `None` means the cache is empty. §3.11.5 gives this row no sentinel,
        // so nothing is recorded -- NOT a `0`, which would read as perfectly
        // fresh and satisfy the staleness alert (§3.11.6) forever. Widening this
        // to `.unwrap_or(0)` would reintroduce that false-fresh reading.
        if let Some(age) = age {
            self.declaration_cache_age_seconds
                .record(i64::try_from(age).unwrap_or(i64::MAX), &[]);
        }
    }

    // @cpt-dod:cpt-cf-usage-collector-dod-resolution-telemetry:p1
    //
    // The REALIZING EMITTER for this DoD's "MUST emit a duration histogram for
    // the hit and miss paths" clause. The match below is that in code: the hit
    // and miss arms record, every other outcome is explicitly enumerated as
    // recording nothing, so "those paths and no other" is structural rather than
    // a convention. Twin marker on the port method.
    fn observe_type_resolution_duration(&self, outcome: TypeResolutionOutcome, seconds: f64) {
        // §3.11.5 declares `result` on this histogram with only two values.
        // Recording a third would ship a label value the row does not
        // admit -- see the port method's own doc for why a failure must
        // never be folded onto `cache_miss` to "cover" it instead.
        match outcome {
            TypeResolutionOutcome::CacheHit | TypeResolutionOutcome::CacheMiss => {
                self.type_resolution_duration_seconds
                    .record(seconds, &[KeyValue::new(key::RESULT, outcome.as_str())]);
            }
            TypeResolutionOutcome::ServedStale
            | TypeResolutionOutcome::Restored
            | TypeResolutionOutcome::Unresolved
            | TypeResolutionOutcome::RegistryError => {}
        }
    }

    // @cpt-dod:cpt-cf-usage-collector-dod-resolution-telemetry:p1
    //
    // The REALIZING EMITTER for this DoD's "MUST count a failed mirror write
    // separately" clause. *Separately* is this being its own instrument rather
    // than a `result` value on `uc_type_resolution_total`, which the distinct
    // fields here and in `record_type_resolution` make structural. Twin marker
    // on the port method, on why a failed mirror READ is not counted here.
    fn record_declaration_mirror_write_failure(&self) {
        // Unlabelled: DESIGN §3.11.5's row gives this counter no labels, and
        // the failing meter's identity rides the `warn!` the resolver emits
        // beside this increment.
        self.declaration_mirror_write_failures.add(1, &[]);
    }
}

/// Convenience constructor used at gear bootstrap: build an
/// `Arc<UcMetricsMeter>` against the process-global OpenTelemetry meter
/// provider, scoped to the `usage-collector` instrumentation library.
///
/// The instruments bind to whatever global provider exists at construction time;
/// `ToolKit` installs the real `SdkMeterProvider` before `Gear::init` runs, so
/// in production this binds to the OTLP-push pipeline.
// @cpt-dod:cpt-cf-usage-collector-dod-otlp-push-emission:p2
// @cpt-begin:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-binding-meter-bootstrap
#[must_use]
pub fn build_default_adapter(prefix: &str, max_batch_records: usize) -> Arc<UcMetricsMeter> {
    let scope = opentelemetry::InstrumentationScope::builder("usage-collector").build();
    let meter = opentelemetry::global::meter_with_scope(scope);
    Arc::new(UcMetricsMeter::new(&meter, prefix, max_batch_records))
}
// @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-binding-meter-bootstrap

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "metrics_tests.rs"]
mod metrics_tests;
