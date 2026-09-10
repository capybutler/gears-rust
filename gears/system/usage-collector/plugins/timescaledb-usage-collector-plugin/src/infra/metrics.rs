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
//! guide** — no such document exists under that name here, and
//! `docs/DESIGN.md` §4 is the closest thing this crate has to one (its own
//! traceability row at `docs/DESIGN.md:90` claims the role). That table cannot
//! currently be read as one: it predates the slice-4 record model and still
//! lists instruments this crate deleted with the usage-type catalog. **The code
//! below is what the plugin actually emits, and the gap is a documentation debt
//! rather than a second opinion.**
//!
//! Instrument names are the **full literal** Prometheus names (snake_case,
//! `_total` on counters, `_seconds` on duration histograms) with **no**
//! `.with_unit(...)` hint, so the rendered series name is identical whether the
//! downstream collector runs with `add_metric_suffixes` on or off — matching
//! the parent gateway (`usage-collector/src/infra/metrics.rs`) and the wider
//! application-gear convention. `metrics_tests` asserts that shape over the
//! whole exported inventory, off each instrument's **kind** rather than off its
//! spelling, so this paragraph is a description of a mechanism and not a
//! promise on its own.
//!
//! Histogram bucket layouts bracket the p95 budget of
//! `cpt-cf-usage-collector-nfr-query-latency` and the write envelope of
//! `cpt-cf-usage-collector-nfr-throughput` — a rate NFR, which has no p95 —
//! against the gear's `DESIGN.md` §3.11.2 Latency Budgets, and are part of the
//! contract. Cited by NFR id rather than through this crate's own
//! `docs/DESIGN.md` §1.2 driver table, whose surrounding rows still describe
//! the retired record model.
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
/// budgets with finer low-end resolution so client-side percentiles stay
/// comparable.
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
    /// `uc_timescaledb_invalidation_rejected_rows_total` — the in-batch
    /// pre-reject, one increment per refused row.
    invalidation_rejected_rows: Counter<u64>,
    /// `uc_timescaledb_invalidation_rejected_statements_total` — the index's
    /// cross-call refusal, one increment per refused statement.
    invalidation_rejected_statements: Counter<u64>,
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
        let invalidation_rejected_rows = meter
            .u64_counter("uc_timescaledb_invalidation_rejected_rows_total")
            .with_description(
                "Withdrawal rows refused by the at-most-one rule before the batch INSERT is \
                 built, one per refused row",
            )
            .build();
        let invalidation_rejected_statements = meter
            .u64_counter("uc_timescaledb_invalidation_rejected_statements_total")
            .with_description(
                "Write statements refused by the at-most-one-invalidation index, one per \
                 refused statement however many withdrawals it carried; a retried batch \
                 counts once per attempt, so read it beside \
                 uc_timescaledb_batch_retries_total",
            )
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
            invalidation_rejected_rows,
            invalidation_rejected_statements,
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

    /// Increment the in-batch refused-withdrawal counter, once per refused
    /// **row**: `plan_batch` found an earlier entry of this same `create_batch`
    /// already withdrawing the target, and pre-rejected this one before the
    /// multi-row `INSERT` was built.
    ///
    /// Sibling of [`Self::inc_invalidation_rejected_statement`]. The two are
    /// separate instruments rather than two values of one label **because their
    /// units differ**: a label asserts that its arms are one measurement
    /// partitioned, which is what makes `sum by (...)` meaningful, and rows and
    /// statements do not sum. Splitting puts the unit in the series name, where
    /// no dashboard can lose it.
    pub fn inc_invalidation_rejected_row(&self) {
        self.invalidation_rejected_rows.add(1, &[]);
    }

    /// Increment the cross-call refused-withdrawal counter, once per refused
    /// **statement**: `usage_records_one_invalidation_uniq` refused the write
    /// because the target was already withdrawn by an entry from an earlier
    /// call.
    ///
    /// Sibling of [`Self::inc_invalidation_rejected_row`]; see there for why
    /// these are two instruments. Two caveats an operator reading the rate
    /// needs, both carried in the instrument's own description because a
    /// Prometheus series carries no rustdoc:
    ///
    /// * **It counts statements, not withdrawals.** One aborted multi-row
    ///   `INSERT` withdrawing two *different* already-withdrawn targets is a
    ///   single increment — `plan_batch` pre-rejects same-target duplicates
    ///   only, and nothing splits the statement per row.
    /// * **A retried batch counts once per attempt.** The refusal can surface
    ///   as a retryable `Transient` (the retention race in `map_insert_error`),
    ///   so one request can increment this up to `MAX_BATCH_ATTEMPTS` times.
    ///   `uc_timescaledb_batch_retries_total` moves alongside, which is how the
    ///   two are told apart.
    pub fn inc_invalidation_rejected_statement(&self) {
        self.invalidation_rejected_statements.add(1, &[]);
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

    /// Every instrument name this inventory declares.
    ///
    /// The destructure below has **no `..`**, on purpose — but be exact about
    /// what that buys. It makes adding a field to [`Metrics`] a compile error
    /// (`E0027`) **until the field is accounted for in the destructure**; it
    /// does not force the instrument's name into the `vec!` beside it. Adding
    /// `foo: _` to silence the compiler and forgetting the string is still
    /// possible, and if the instrument is also never driven in the test, the
    /// counts match and the run is green.
    ///
    /// What it does do is make it impossible to *reach* this function without
    /// being shown the new field, at the one place whose whole job is to list
    /// them — which is strictly more than a hand-kept array elsewhere in the
    /// tree can offer, and the reason `metrics_tests` can assert the exported
    /// set **equals** this one rather than merely containing some of it.
    ///
    /// Renaming an instrument in [`Self::with_meter`] without renaming it here
    /// fails that assertion, from the other side.
    /// Gated on `any(test, feature = "postgres")` rather than `test` alone, for
    /// the same reason [`crate::infra::storage::migration_probe`] is: the
    /// `tests/*.rs` integration crates are external to this one, and an
    /// inventory they cannot reach is an inventory they will hand-copy.
    /// `records_ingest_integration_pg` checks the counter names it asserts on
    /// against this list, because its `counter_sum` reads a renamed instrument
    /// as a legitimate zero. `postgres` is a test-only feature, so nothing
    /// ships with this compiled in.
    #[cfg(any(test, feature = "postgres"))]
    #[must_use]
    pub fn declared_instrument_names(&self) -> Vec<&'static str> {
        let Self {
            insert_duration: _,
            query_duration: _,
            pool_acquire_duration: _,
            batch_rows: _,
            dedup_absorbed: _,
            backend_error: _,
            idempotency_conflict: _,
            migration_failure: _,
            invalidation: _,
            invalidation_rejected_rows: _,
            invalidation_rejected_statements: _,
            dedup_stale: _,
            batch_retry: _,
            query_requests: _,
            tls_handshake_failure: _,
            ready: _,
            _pool_active: _,
            _pool_idle: _,
        } = self;
        vec![
            "uc_timescaledb_insert_duration_seconds",
            "uc_timescaledb_query_duration_seconds",
            "uc_timescaledb_pool_acquire_duration_seconds",
            "uc_timescaledb_batch_rows",
            "uc_timescaledb_dedup_absorbed_total",
            "uc_timescaledb_backend_errors_total",
            "uc_timescaledb_idempotency_conflicts_total",
            "uc_timescaledb_migration_failures_total",
            "uc_timescaledb_invalidations_total",
            "uc_timescaledb_invalidation_rejected_rows_total",
            "uc_timescaledb_invalidation_rejected_statements_total",
            "uc_timescaledb_dedup_stale_total",
            "uc_timescaledb_batch_retries_total",
            "uc_timescaledb_query_requests_total",
            "uc_timescaledb_tls_handshake_failures_total",
            "uc_timescaledb_ready",
            "uc_timescaledb_pool_connections_active",
            "uc_timescaledb_pool_connections_idle",
        ]
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
