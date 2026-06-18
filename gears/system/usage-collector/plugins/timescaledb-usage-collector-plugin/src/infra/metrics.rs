//! `OpenTelemetry` metric inventory for the `TimescaleDB` storage plugin.
//!
//! Realizes design ID `cpt-cf-uc-plugin-design-metric-inventory` (`DESIGN.md`
//! §Observability): every backend-internal series the plugin owns under the
//! `uc.timescaledb.` sub-namespace. Instrument names use the `OTel`-native
//! dot-separated form; units are carried by [`with_unit`](opentelemetry::metrics::HistogramBuilder::with_unit)
//! and cumulative-counter semantics by instrument kind, not a name suffix.
//! Histogram bucket layouts bracket the NFR p95 budgets in `DESIGN.md` §1.2 and
//! are part of the contract.
//!
//! All labels are bounded to enumerated value sets (see the `label` module):
//! unbounded identifiers (`tenant_id`, `gts_id`, `uuid`, ...) MUST NOT appear as
//! metric dimensions — they belong in logs and traces.
//!
//! This module only constructs the instruments and exposes ergonomic recording
//! helpers; threading them through the stores and gear is a separate task.

use std::sync::Arc;
use std::time::Instant;

use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter, ObservableGauge};
use opentelemetry::{InstrumentationScope, KeyValue, global};
use sqlx::PgPool;

/// `OpenTelemetry` instrumentation scope (meter name) for every plugin series.
const SCOPE_NAME: &str = "uc.timescaledb";

/// Explicit histogram bucket boundaries (seconds) for backend operation
/// durations. The `OTel` SDK defaults are count-oriented and meaningless for a
/// seconds-valued duration; these brackets the §1.2 p95 budgets with finer
/// low-end resolution so client-side percentiles stay comparable.
const DURATION_BOUNDARIES_SECS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Explicit histogram bucket boundaries (row count) for the per-write batch
/// size. Integer-ish boundaries spanning a single row up to a large bulk write,
/// so write amortization is observable (`batch.rows`).
const BATCH_ROW_BOUNDARIES: &[f64] = &[1.0, 5.0, 10.0, 50.0, 100.0, 500.0, 1000.0];

/// Bounded metric label keys and values.
///
/// Centralizing the `&'static str` constants keeps every call site on the
/// enumerated value sets from `DESIGN.md` §Observability and prevents an
/// accidental high-cardinality label from leaking in.
pub mod label {
    /// Label key for the insert mode dimension.
    pub const MODE: &str = "mode";
    /// `mode` value: a single-row ingest.
    pub const MODE_SINGLE: &str = "single";
    /// `mode` value: a batch (multi-row) ingest.
    pub const MODE_BATCH: &str = "batch";

    /// Label key for the query-kind dimension.
    pub const QUERY_KIND: &str = "query_kind";
    /// `query_kind` value: a raw (keyset) record listing.
    pub const QUERY_KIND_RAW: &str = "raw";
    /// `query_kind` value: a server-side aggregated query.
    pub const QUERY_KIND_AGGREGATED: &str = "aggregated";

    /// Label key for the backend-error classification dimension.
    pub const CLASS: &str = "class";
    /// `class` value: a retryable transient backend failure.
    pub const CLASS_TRANSIENT: &str = "transient";
    /// `class` value: a non-retryable internal backend failure.
    pub const CLASS_INTERNAL: &str = "internal";
}

/// Insert-mode dimension behind the `mode` label of
/// `uc.timescaledb.insert.duration`. A closed enum so a call site cannot pass an
/// arbitrary string into the bounded label set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertMode {
    /// A single-row ingest (`mode = "single"`).
    Single,
    /// A batch (multi-row) ingest (`mode = "batch"`).
    Batch,
}

impl InsertMode {
    /// The bounded `mode` label value for this mode.
    const fn as_label(self) -> &'static str {
        match self {
            Self::Single => label::MODE_SINGLE,
            Self::Batch => label::MODE_BATCH,
        }
    }
}

/// Query-kind dimension behind the `query_kind` label of
/// `uc.timescaledb.query.duration` and `uc.timescaledb.query.requests`. A closed
/// enum so the bounded label set is enforced by the type, not by convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    /// A raw (keyset) record listing (`query_kind = "raw"`).
    Raw,
    /// A server-side aggregated query (`query_kind = "aggregated"`).
    Aggregated,
}

impl QueryKind {
    /// The bounded `query_kind` label value for this kind.
    const fn as_label(self) -> &'static str {
        match self {
            Self::Raw => label::QUERY_KIND_RAW,
            Self::Aggregated => label::QUERY_KIND_AGGREGATED,
        }
    }
}

/// Backend-error classification behind the `class` label of
/// `uc.timescaledb.backend.error`, mirroring the SPI transient-vs-internal
/// split. A closed enum so an out-of-set class is unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// A retryable transient backend failure (`class = "transient"`).
    Transient,
    /// A non-retryable internal backend failure (`class = "internal"`).
    Internal,
}

impl ErrorClass {
    /// The bounded `class` label value for this classification.
    const fn as_label(self) -> &'static str {
        match self {
            Self::Transient => label::CLASS_TRANSIENT,
            Self::Internal => label::CLASS_INTERNAL,
        }
    }
}

/// The full `OpenTelemetry` metric inventory for the plugin.
///
/// Built once via [`Metrics::new`] and shared through an `Arc<Metrics>`; the
/// `OTel` instrument handles are themselves cheap `Arc`-backed clones, so the
/// struct is intentionally not `Clone` (share the `Arc`, not the struct).
///
/// The two observable pool gauges store their [`ObservableGauge`] handles: the
/// registered callback is dropped — and thus unregistered — when the handle is
/// dropped, so the handles must outlive the meter provider.
#[derive(Debug)]
pub struct Metrics {
    // --- Histograms (seconds, unless noted) ---
    /// `uc.timescaledb.insert.duration` — labelled by `mode`.
    insert_duration: Histogram<f64>,
    /// `uc.timescaledb.query.duration` — labelled by `query_kind`.
    query_duration: Histogram<f64>,
    /// `uc.timescaledb.deactivate.duration`.
    deactivate_duration: Histogram<f64>,
    /// `uc.timescaledb.pool.acquire.duration`.
    pool_acquire_duration: Histogram<f64>,
    /// `uc.timescaledb.batch.rows` — row-count distribution per batch write.
    batch_rows: Histogram<f64>,

    // --- Counters ---
    /// `uc.timescaledb.dedup.absorbed`.
    dedup_absorbed: Counter<u64>,
    /// `uc.timescaledb.backend.error` — labelled by `class`.
    backend_error: Counter<u64>,
    /// `uc.timescaledb.idempotency.conflict`.
    idempotency_conflict: Counter<u64>,
    /// `uc.timescaledb.usage_type.referenced`.
    usage_type_referenced: Counter<u64>,
    /// `uc.timescaledb.migration.failure`.
    migration_failure: Counter<u64>,
    /// `uc.timescaledb.compensation`.
    compensation: Counter<u64>,
    /// `uc.timescaledb.dedup.stale`.
    dedup_stale: Counter<u64>,
    /// `uc.timescaledb.query.requests` — labelled by `query_kind`.
    query_requests: Counter<u64>,
    /// `uc.timescaledb.tls.handshake.failure`.
    tls_handshake_failure: Counter<u64>,

    // --- Synchronous gauges (set imperatively) ---
    /// `uc.timescaledb.usage_type_catalog.size`.
    usage_type_catalog_size: Gauge<u64>,
    /// `uc.timescaledb.ready` — plugin-local backend health (0/1).
    ready: Gauge<u64>,

    // --- Observable gauges (callback-read; handles kept to stay registered) ---
    /// `uc.timescaledb.pool.connections.active`.
    _pool_active: ObservableGauge<u64>,
    /// `uc.timescaledb.pool.connections.idle`.
    _pool_idle: ObservableGauge<u64>,
}

impl Metrics {
    /// Build the complete metric inventory against the global meter provider.
    /// Production entry point; resolves the meter from the process-global
    /// provider and delegates to [`Metrics::with_meter`].
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        let scope = InstrumentationScope::builder(SCOPE_NAME).build();
        let meter = global::meter_with_scope(scope);
        Self::with_meter(&meter, pool)
    }

    /// Build the inventory against an explicit [`Meter`] instead of the global
    /// provider.
    ///
    /// [`Metrics::new`] resolves the meter from the process-global provider;
    /// this seam lets a test install a local meter provider backed by an
    /// in-memory reader and assert the recorded series without mutating global
    /// state (so the assertions stay parallel-safe).
    ///
    /// The `pool` is cloned into the two observable-gauge callbacks, which read
    /// `pool.size()` / `pool.num_idle()` (synchronous, in-memory) on each
    /// collection cycle — no DB I/O happens in a callback.
    #[must_use]
    pub fn with_meter(meter: &Meter, pool: PgPool) -> Self {
        let insert_duration = meter
            .f64_histogram("uc.timescaledb.insert.duration")
            .with_description("Duration of usage-record inserts, by mode")
            .with_unit("s")
            .with_boundaries(DURATION_BOUNDARIES_SECS.to_vec())
            .build();
        let query_duration = meter
            .f64_histogram("uc.timescaledb.query.duration")
            .with_description("Duration of usage-record queries, by kind")
            .with_unit("s")
            .with_boundaries(DURATION_BOUNDARIES_SECS.to_vec())
            .build();
        let deactivate_duration = meter
            .f64_histogram("uc.timescaledb.deactivate.duration")
            .with_description("Duration of the event-deactivation cascade")
            .with_unit("s")
            .with_boundaries(DURATION_BOUNDARIES_SECS.to_vec())
            .build();
        let pool_acquire_duration = meter
            .f64_histogram("uc.timescaledb.pool.acquire.duration")
            .with_description("Time spent acquiring a connection from the pool")
            .with_unit("s")
            .with_boundaries(DURATION_BOUNDARIES_SECS.to_vec())
            .build();
        let batch_rows = meter
            .f64_histogram("uc.timescaledb.batch.rows")
            .with_description("Row count per batch write")
            .with_unit("{row}")
            .with_boundaries(BATCH_ROW_BOUNDARIES.to_vec())
            .build();

        let dedup_absorbed = meter
            .u64_counter("uc.timescaledb.dedup.absorbed")
            .with_description("Exact-equality retries silently absorbed on the dedup-key conflict")
            .build();
        let backend_error = meter
            .u64_counter("uc.timescaledb.backend.error")
            .with_description("Backend errors, by SPI transient/internal classification")
            .build();
        let idempotency_conflict = meter
            .u64_counter("uc.timescaledb.idempotency.conflict")
            .with_description("Canonical-field-mismatch idempotency conflicts")
            .build();
        let usage_type_referenced = meter
            .u64_counter("uc.timescaledb.usage_type.referenced")
            .with_description("FK ON DELETE RESTRICT rejections on usage-type delete")
            .build();
        let migration_failure = meter
            .u64_counter("uc.timescaledb.migration.failure")
            .with_description("Schema-migration failures at startup")
            .build();
        let compensation = meter
            .u64_counter("uc.timescaledb.compensation")
            .with_description("Inserts carrying a corrects_id (compensating records)")
            .build();
        let dedup_stale = meter
            .u64_counter("uc.timescaledb.dedup.stale")
            .with_description("Dedup hits whose stored record had aged out (retryable)")
            .build();
        let query_requests = meter
            .u64_counter("uc.timescaledb.query.requests")
            .with_description("Query requests, by kind (aggregated-vs-raw workload mix)")
            .build();
        let tls_handshake_failure = meter
            .u64_counter("uc.timescaledb.tls.handshake.failure")
            .with_description("TLS handshake failures against the backend DSN")
            .build();

        let usage_type_catalog_size = meter
            .u64_gauge("uc.timescaledb.usage_type_catalog.size")
            .with_description("Current usage-type catalog row count")
            .build();
        let ready = meter
            .u64_gauge("uc.timescaledb.ready")
            .with_description("Plugin-local backend readiness (1 = pool + migration ok)")
            .build();

        // Each observable gauge owns its own callback closure: 0.31 has no
        // batch-observe API, so the two pool gauges cannot share one callback.
        let active_pool = pool.clone();
        let pool_active = meter
            .u64_observable_gauge("uc.timescaledb.pool.connections.active")
            .with_description("Connections currently checked out of the pool")
            .with_callback(move |observer| {
                let active = u64::from(active_pool.size())
                    .saturating_sub(u64::try_from(active_pool.num_idle()).unwrap_or(0));
                observer.observe(active, &[]);
            })
            .build();
        let idle_pool = pool;
        let pool_idle = meter
            .u64_observable_gauge("uc.timescaledb.pool.connections.idle")
            .with_description("Connections currently idle in the pool")
            .with_callback(move |observer| {
                observer.observe(u64::try_from(idle_pool.num_idle()).unwrap_or(0), &[]);
            })
            .build();

        Self {
            insert_duration,
            query_duration,
            deactivate_duration,
            pool_acquire_duration,
            batch_rows,
            dedup_absorbed,
            backend_error,
            idempotency_conflict,
            usage_type_referenced,
            migration_failure,
            compensation,
            dedup_stale,
            query_requests,
            tls_handshake_failure,
            usage_type_catalog_size,
            ready,
            _pool_active: pool_active,
            _pool_idle: pool_idle,
        }
    }

    // --- Histogram recording helpers ---

    /// Record an insert duration (seconds) for the given [`InsertMode`].
    pub fn record_insert(&self, mode: InsertMode, secs: f64) {
        self.insert_duration
            .record(secs, &[KeyValue::new(label::MODE, mode.as_label())]);
    }

    /// Record a query duration (seconds) for the given [`QueryKind`].
    pub fn record_query(&self, kind: QueryKind, secs: f64) {
        self.query_duration
            .record(secs, &[KeyValue::new(label::QUERY_KIND, kind.as_label())]);
    }

    /// Record a deactivation-cascade duration (seconds).
    pub fn record_deactivate(&self, secs: f64) {
        self.deactivate_duration.record(secs, &[]);
    }

    /// Record a pool-acquire duration (seconds).
    pub fn record_pool_acquire(&self, secs: f64) {
        self.pool_acquire_duration.record(secs, &[]);
    }

    /// Record the row count `n` of a batch write.
    pub fn record_batch_rows(&self, n: f64) {
        self.batch_rows.record(n, &[]);
    }

    // --- Counter helpers ---

    /// Increment the silently-absorbed dedup retry counter.
    pub fn inc_dedup_absorbed(&self) {
        self.dedup_absorbed.add(1, &[]);
    }

    /// Increment the idempotency-conflict counter.
    pub fn inc_idempotency_conflict(&self) {
        self.idempotency_conflict.add(1, &[]);
    }

    /// Increment the usage-type-referenced (FK rejection) counter.
    pub fn inc_usage_type_referenced(&self) {
        self.usage_type_referenced.add(1, &[]);
    }

    /// Increment the compensation (`corrects_id` insert) counter.
    pub fn inc_compensation(&self) {
        self.compensation.add(1, &[]);
    }

    /// Increment the stale-dedup counter (dedup hit whose record had aged out).
    pub fn inc_dedup_stale(&self) {
        self.dedup_stale.add(1, &[]);
    }

    /// Increment the migration-failure counter.
    pub fn inc_migration_failure(&self) {
        self.migration_failure.add(1, &[]);
    }

    /// Increment the TLS-handshake-failure counter.
    pub fn inc_tls_handshake_failure(&self) {
        self.tls_handshake_failure.add(1, &[]);
    }

    /// Increment the backend-error counter for the given [`ErrorClass`].
    pub fn inc_backend_error(&self, class: ErrorClass) {
        self.backend_error
            .add(1, &[KeyValue::new(label::CLASS, class.as_label())]);
    }

    /// Increment the query-requests counter for the given [`QueryKind`].
    pub fn inc_query_request(&self, kind: QueryKind) {
        self.query_requests
            .add(1, &[KeyValue::new(label::QUERY_KIND, kind.as_label())]);
    }

    // --- Synchronous gauge setters ---

    /// Set the current usage-type catalog size.
    pub fn set_catalog_size(&self, n: u64) {
        self.usage_type_catalog_size.record(n, &[]);
    }

    /// Set the plugin-local readiness gauge (1 when `ready`, else 0).
    pub fn set_ready(&self, ready: bool) {
        self.ready.record(u64::from(ready), &[]);
    }
}

/// Which duration histogram an [`OpDurationGuard`] records on drop.
#[derive(Debug, Clone, Copy)]
pub enum TimedOp {
    /// `uc.timescaledb.query.duration`, labelled by the [`QueryKind`].
    Query(QueryKind),
    /// `uc.timescaledb.deactivate.duration`.
    Deactivate,
}

/// Records an operation-duration histogram on drop, so the duration is captured
/// on **every** return path — including the error arms that `?` out before a
/// success-only `record_*` call would run. Construct it at the top of an
/// operation and let it fall out of scope on return.
///
/// Holds an `Arc<Metrics>` (the inventory is shared via `Arc`, never deep
/// cloned); the target series is fixed at construction.
#[derive(Debug)]
pub struct OpDurationGuard {
    metrics: Arc<Metrics>,
    op: TimedOp,
    start: Instant,
}

impl OpDurationGuard {
    /// Start timing `op` against `metrics`; records on drop.
    #[must_use]
    pub fn start(metrics: Arc<Metrics>, op: TimedOp) -> Self {
        Self {
            metrics,
            op,
            start: Instant::now(),
        }
    }
}

impl Drop for OpDurationGuard {
    fn drop(&mut self) {
        let secs = self.start.elapsed().as_secs_f64();
        match self.op {
            TimedOp::Query(kind) => self.metrics.record_query(kind, secs),
            TimedOp::Deactivate => self.metrics.record_deactivate(secs),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "metrics_tests.rs"]
mod metrics_tests;
