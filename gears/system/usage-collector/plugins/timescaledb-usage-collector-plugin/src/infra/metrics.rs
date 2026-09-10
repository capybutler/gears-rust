//! `OpenTelemetry` metric inventory for the `TimescaleDB` storage plugin.
//!
//! Realizes design ID `cpt-cf-uc-plugin-design-metric-inventory`: every
//! backend-internal series the plugin owns under the `uc_timescaledb_`
//! sub-namespace. The gear's `DESIGN.md` §3.11.5 owns the request-path `uc_`
//! inventory and delegates the rest, verbatim at `DESIGN.md:1821-1822`:
//!
//! > Plugins may expose backend-internal metrics under their own prefix. Those
//! > series are owned by the plugin's deployment guide.
//!
//! **§3.11.5 names no instrument for a storage plugin** — every "Emitting
//! component" cell in its three tables is a gateway, the type-resolver, the
//! plugin-host or a PDP enforcer — so the two rules it binds this crate by are
//! the naming convention below and the bounded-label rule further down, and
//! nothing in it obliges a particular series here.
//!
//! Note what the clause delegates ownership *to*: the plugin's **deployment
//! guide**, which is `docs/DESIGN.md` §4 in this crate. That table cannot
//! currently be read as one — it predates the slice-4 record model and still
//! lists instruments this crate deleted with the usage-type catalog — so **the
//! code below is what the plugin actually emits, and the gap is a documentation
//! debt rather than a second opinion**. Instrument names are the **full literal**
//! Prometheus names (snake_case, `_total` on counters, `_seconds` on duration
//! histograms) with **no** `.with_unit(...)` hint, so the rendered series name is
//! identical whether the downstream collector runs with `add_metric_suffixes` on
//! or off — matching the parent gateway (`usage-collector/src/infra/metrics.rs`)
//! and the wider application-gear convention. Histogram bucket layouts bracket
//! the p95 budgets of `cpt-cf-usage-collector-nfr-query-latency` and
//! `cpt-cf-usage-collector-nfr-throughput` (the gear's `DESIGN.md` §3.11.2
//! Latency Budgets) and are part of the contract — cited by NFR id rather than
//! through this crate's own `docs/DESIGN.md` §1.2 driver table, whose
//! surrounding rows still describe the retired record model.
//!
//! All labels are bounded to enumerated value sets (see the `label` module):
//! unbounded identifiers (`tenant_id`, `gts_id`, `id`, ...) MUST NOT appear as
//! metric dimensions — they belong in logs and traces.

// Vendored TimescaleDB raw-SQL backend: `sqlx` is required infra (hypertable
// time-series, `time_bucket` aggregation, keyset pagination — see DESIGN.md). Tenant
// isolation is enforced by hand via parameterized `tenant_id` predicates and an
// allowlisted-identifier query builder (DESIGN.md §Injection-Safe Query Translation),
// not SecureConn/AccessScope.
#![allow(unknown_lints, de0706_no_direct_sqlx)]

use std::sync::Arc;
use std::time::Instant;

use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter, ObservableGauge};
use opentelemetry::{InstrumentationScope, KeyValue, global};
use sqlx::PgPool;

/// `OpenTelemetry` instrumentation scope (meter name) for every plugin series.
const SCOPE_NAME: &str = "uc.timescaledb";

/// Explicit histogram bucket boundaries (seconds) for backend operation
/// durations. The `OTel` SDK defaults are count-oriented and meaningless for a
/// seconds-valued duration; these bracket the gear `DESIGN.md` §3.11.2 p95
/// budgets with finer
/// low-end resolution so client-side percentiles stay comparable.
const DURATION_BOUNDARIES_SECS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Explicit histogram bucket boundaries (row count) for the per-write batch
/// size. Integer-ish boundaries spanning a single row up to a large bulk write,
/// so write amortization is observable (`uc_timescaledb_batch_rows`).
const BATCH_ROW_BOUNDARIES: &[f64] = &[1.0, 5.0, 10.0, 50.0, 100.0, 500.0, 1000.0];

/// Bounded metric label keys and values.
///
/// Centralizing the `&'static str` constants keeps every call site on the
/// enumerated sets declared in this module — the gear `DESIGN.md` §3.11.5
/// "Label cardinality" rule, which bounds every label and bars unbounded
/// identifiers outright — and prevents an accidental high-cardinality label
/// from leaking in.
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
    pub const ERROR_CATEGORY: &str = "error_category";
    /// `error_category` value: a retryable transient backend failure.
    pub const ERROR_CATEGORY_TRANSIENT: &str = "transient";
    /// `error_category` value: a non-retryable internal backend failure.
    pub const ERROR_CATEGORY_INTERNAL: &str = "internal";

    /// Label key for the invalidation-rejection scope dimension.
    pub const SCOPE: &str = "scope";
    /// `scope` value: the withdrawal was refused against an earlier entry of
    /// the same `create_batch` call.
    pub const SCOPE_IN_BATCH: &str = "in_batch";
    /// `scope` value: the withdrawal was refused against an entry already in
    /// the ledger from an earlier call.
    pub const SCOPE_CROSS_CALL: &str = "cross_call";
}

/// Insert-mode dimension behind the `mode` label of
/// `uc_timescaledb_insert_duration_seconds`. A closed enum so a call site cannot pass an
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
/// `uc_timescaledb_query_duration_seconds` and `uc_timescaledb_query_requests_total`. A closed
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

/// Backend-error classification behind the `error_category` label of
/// `uc_timescaledb_backend_errors_total`, mirroring the SPI transient-vs-internal
/// split. A closed enum so an out-of-set class is unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// A retryable transient backend failure (`error_category = "transient"`).
    Transient,
    /// A non-retryable internal backend failure (`error_category = "internal"`).
    Internal,
}

impl ErrorClass {
    /// The bounded `error_category` label value for this classification.
    const fn as_label(self) -> &'static str {
        match self {
            Self::Transient => label::ERROR_CATEGORY_TRANSIENT,
            Self::Internal => label::ERROR_CATEGORY_INTERNAL,
        }
    }
}

/// Which of the two at-most-one-invalidation rejection paths refused a
/// withdrawal, behind the `scope` label of
/// `uc_timescaledb_invalidation_rejections_total`.
///
/// The SPI names at-most-one invalidation as the store's single admission-time
/// obligation, and the plugin refuses a second withdrawal along two paths that
/// are asymmetric in every other respect: `plan_batch` pre-rejects a duplicate
/// inside one `create_batch` call before the multi-row `INSERT` is built, while
/// the partial unique index refuses one that arrives in a later call. Splitting
/// the counter on that boundary is what makes the two legible apart, and it is
/// the refusal rate that shows an operator the obligation being exercised at
/// all. A closed enum so the label set is enforced by the type.
///
/// **The two arms are not addable.** `in_batch` increments once per refused
/// **row**; `cross_call` increments once per refused **statement**, because the
/// index aborts a multi-row `INSERT` whole rather than per row. Read each series
/// on its own; a sum of the two counts two different things.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidationRejection {
    /// Refused against an earlier entry of the same batch (`scope = "in_batch"`).
    InBatch,
    /// Refused against an entry already in the ledger (`scope = "cross_call"`).
    CrossCall,
}

impl InvalidationRejection {
    /// The bounded `scope` label value for this rejection path.
    const fn as_label(self) -> &'static str {
        match self {
            Self::InBatch => label::SCOPE_IN_BATCH,
            Self::CrossCall => label::SCOPE_CROSS_CALL,
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
    /// `uc_timescaledb_insert_duration_seconds` — labelled by `mode`.
    insert_duration: Histogram<f64>,
    /// `uc_timescaledb_query_duration_seconds` — labelled by `query_kind`.
    query_duration: Histogram<f64>,
    /// `uc_timescaledb_pool_acquire_duration_seconds`.
    pool_acquire_duration: Histogram<f64>,
    /// `uc_timescaledb_batch_rows` — row-count distribution per batch write.
    batch_rows: Histogram<f64>,

    // --- Counters ---
    /// `uc_timescaledb_dedup_absorbed_total`.
    dedup_absorbed: Counter<u64>,
    /// `uc_timescaledb_backend_errors_total` — labelled by `error_category`.
    backend_error: Counter<u64>,
    /// `uc_timescaledb_idempotency_conflicts_total`.
    idempotency_conflict: Counter<u64>,
    /// `uc_timescaledb_migration_failures_total`.
    migration_failure: Counter<u64>,
    /// `uc_timescaledb_invalidations_total`.
    invalidation: Counter<u64>,
    /// `uc_timescaledb_invalidation_rejections_total` — labelled by `scope`.
    invalidation_rejection: Counter<u64>,
    /// `uc_timescaledb_dedup_stale_total`.
    dedup_stale: Counter<u64>,
    /// `uc_timescaledb_batch_retries_total` — bounded in-process `create_batch`
    /// retries after a transient backend error (deadlock victim self-heal).
    batch_retry: Counter<u64>,
    /// `uc_timescaledb_query_requests_total` — labelled by `query_kind`.
    query_requests: Counter<u64>,
    /// `uc_timescaledb_tls_handshake_failures_total`.
    tls_handshake_failure: Counter<u64>,

    // --- Synchronous gauges (set imperatively) ---
    /// `uc_timescaledb_ready` — plugin-local backend health (0/1).
    ready: Gauge<u64>,

    // --- Observable gauges (callback-read; handles kept to stay registered) ---
    /// `uc_timescaledb_pool_connections_active`.
    _pool_active: ObservableGauge<u64>,
    /// `uc_timescaledb_pool_connections_idle`.
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
            .f64_histogram("uc_timescaledb_insert_duration_seconds")
            .with_description("Duration of usage-record inserts, by mode")
            .with_boundaries(DURATION_BOUNDARIES_SECS.to_vec())
            .build();
        let query_duration = meter
            .f64_histogram("uc_timescaledb_query_duration_seconds")
            .with_description("Duration of usage-record queries, by kind")
            .with_boundaries(DURATION_BOUNDARIES_SECS.to_vec())
            .build();
        let pool_acquire_duration = meter
            .f64_histogram("uc_timescaledb_pool_acquire_duration_seconds")
            .with_description("Time spent acquiring a connection from the pool")
            .with_boundaries(DURATION_BOUNDARIES_SECS.to_vec())
            .build();
        let batch_rows = meter
            .f64_histogram("uc_timescaledb_batch_rows")
            .with_description("Row count per batch write")
            .with_boundaries(BATCH_ROW_BOUNDARIES.to_vec())
            .build();

        let dedup_absorbed = meter
            .u64_counter("uc_timescaledb_dedup_absorbed_total")
            .with_description("Exact-equality retries silently absorbed on the dedup-key conflict")
            .build();
        let backend_error = meter
            .u64_counter("uc_timescaledb_backend_errors_total")
            .with_description("Backend errors, by SPI transient/internal classification")
            .build();
        let idempotency_conflict = meter
            .u64_counter("uc_timescaledb_idempotency_conflicts_total")
            .with_description("Canonical-field-mismatch idempotency conflicts")
            .build();
        let migration_failure = meter
            .u64_counter("uc_timescaledb_migration_failures_total")
            .with_description("Schema-migration failures at startup")
            .build();
        let invalidation = meter
            .u64_counter("uc_timescaledb_invalidations_total")
            .with_description("Accepted invalidation entries (append-only withdrawals)")
            .build();
        let invalidation_rejection = meter
            .u64_counter("uc_timescaledb_invalidation_rejections_total")
            .with_description("Withdrawals refused by the at-most-one rule, by scope")
            .build();
        let dedup_stale = meter
            .u64_counter("uc_timescaledb_dedup_stale_total")
            .with_description("Dedup hits whose stored record had aged out (retryable)")
            .build();
        let batch_retry = meter
            .u64_counter("uc_timescaledb_batch_retries_total")
            .with_description("Bounded create_batch retries after a transient backend error")
            .build();
        let query_requests = meter
            .u64_counter("uc_timescaledb_query_requests_total")
            .with_description("Query requests, by kind (aggregated-vs-raw workload mix)")
            .build();
        let tls_handshake_failure = meter
            .u64_counter("uc_timescaledb_tls_handshake_failures_total")
            .with_description("TLS handshake failures against the backend DSN")
            .build();

        let ready = meter
            .u64_gauge("uc_timescaledb_ready")
            .with_description("Plugin-local backend readiness (1 = pool + migration ok)")
            .build();

        // Each observable gauge owns its own callback closure: 0.31 has no
        // batch-observe API, so the two pool gauges cannot share one callback.
        let active_pool = pool.clone();
        let pool_active = meter
            .u64_observable_gauge("uc_timescaledb_pool_connections_active")
            .with_description("Connections currently checked out of the pool")
            .with_callback(move |observer| {
                let active = u64::from(active_pool.size())
                    .saturating_sub(u64::try_from(active_pool.num_idle()).unwrap_or(0));
                observer.observe(active, &[]);
            })
            .build();
        let idle_pool = pool;
        let pool_idle = meter
            .u64_observable_gauge("uc_timescaledb_pool_connections_idle")
            .with_description("Connections currently idle in the pool")
            .with_callback(move |observer| {
                observer.observe(u64::try_from(idle_pool.num_idle()).unwrap_or(0), &[]);
            })
            .build();

        Self {
            insert_duration,
            query_duration,
            pool_acquire_duration,
            batch_rows,
            dedup_absorbed,
            backend_error,
            idempotency_conflict,
            migration_failure,
            invalidation,
            invalidation_rejection,
            dedup_stale,
            batch_retry,
            query_requests,
            tls_handshake_failure,
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

    /// Increment the accepted-invalidation counter (one per admitted entry
    /// carrying an [`Invalidation`](usage_collector_sdk::Invalidation)).
    pub fn inc_invalidation(&self) {
        self.invalidation.add(1, &[]);
    }

    /// Increment the refused-withdrawal counter for the given
    /// [`InvalidationRejection`] path.
    pub fn inc_invalidation_rejection(&self, scope: InvalidationRejection) {
        self.invalidation_rejection
            .add(1, &[KeyValue::new(label::SCOPE, scope.as_label())]);
    }

    /// Increment the stale-dedup counter (dedup hit whose record had aged out).
    pub fn inc_dedup_stale(&self) {
        self.dedup_stale.add(1, &[]);
    }

    /// Increment the `create_batch` bounded-retry counter (one per retry of a
    /// transient backend error).
    pub fn inc_batch_retry(&self) {
        self.batch_retry.add(1, &[]);
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
            .add(1, &[KeyValue::new(label::ERROR_CATEGORY, class.as_label())]);
    }

    /// Increment the query-requests counter for the given [`QueryKind`].
    pub fn inc_query_request(&self, kind: QueryKind) {
        self.query_requests
            .add(1, &[KeyValue::new(label::QUERY_KIND, kind.as_label())]);
    }

    // --- Synchronous gauge setters ---

    /// Set the plugin-local readiness gauge (1 when `ready`, else 0).
    pub fn set_ready(&self, ready: bool) {
        self.ready.record(u64::from(ready), &[]);
    }
}

/// Which duration histogram an [`OpDurationGuard`] records on drop.
#[derive(Debug, Clone, Copy)]
pub enum TimedOp {
    /// `uc_timescaledb_query_duration_seconds`, labelled by the [`QueryKind`].
    Query(QueryKind),
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
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "metrics_tests.rs"]
mod metrics_tests;
