//! Postgres-backed [`RecordStore`] over the `usage_records` hypertable.
//!
//! All operations — `create_batch` / `get` / `feed_page` / `list` /
//! `aggregate` / `reconciliation` — are real `sqlx`.
//!
//! **This module is the orchestration and nothing else.** Each SPI method here
//! does the same four things and no more: build the statement, acquire a
//! connection, run it, decode what comes back — with the metrics and the error
//! classification that belong to a method boundary. Every statement it sends is
//! built by a module that cannot reach a connection:
//!
//! - the guarded insert and its conflict read-back — [`super::query::write`]
//! - the point lookup and the keyset page — [`super::query::list`]
//! - the feed page and the retention-mark probe — [`super::query::feed`]
//! - the exact-scan fold — [`super::query::aggregate`]
//! - the rollup fold — [`super::query::rollup`]
//! - reconciliation's S1∧S2 and S3 — [`super::query::reconciliation`]
//!
//! That split is what lets every statement be unit-tested without a database,
//! and it is why a statement is always built **before** a connection is
//! acquired: a query that cannot be rendered never reaches the pool and never
//! reads a row. Several of the tests below exist only to pin that ordering.
//!
//! The two orchestrations long enough to read on their own are children rather
//! than inlined here — [`write`] for the batch transaction and [`feed`] for the
//! six-step page protocol. Both are `impl PgRecordStore` blocks, so they reach
//! this type's private fields and the shared helpers below without widening
//! anything.

// Vendored TimescaleDB raw-SQL backend: `sqlx` is required infra (hypertable
// time-series, `time_bucket` aggregation, keyset pagination — see DESIGN.md). Tenant
// isolation is enforced by hand via parameterized `tenant_id` predicates and an
// allowlisted-identifier query builder (DESIGN.md §Injection-Safe Query Translation),
// not SecureConn/AccessScope.
#![allow(unknown_lints, de0706_no_direct_sqlx)]

mod feed;
mod write;

use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use bigdecimal::BigDecimal;
use rust_decimal::Decimal;
use sqlx::pool::PoolConnection;
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, Connection as _, PgPool, Postgres, Row};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit_odata::{ODataQuery, ast};
use uuid::Uuid;

use usage_collector_sdk::v2::{
    AggregationBucket, AggregationDimension, AggregationFold, AggregationResult, Keyset,
    MetadataFilter, MeterRef, MeteredRecord, ObservedQuantity, QuantitySummary,
    ReconciliationMetadata, RecordPage, StoredUsageRecord, TimeRange, UsageCollectorPluginError,
    UsageQuantity,
};

use crate::domain::ports::{FeedPageRows, RecordStore};
use crate::infra::metrics::{ErrorClass, Metrics, OpDurationGuard, QueryKind, TimedOp};
use crate::infra::storage::entity::UsageRecordRow;
use crate::infra::storage::error::{
    acquire_error_clears_readiness, is_ledger_pk_violation, map_sqlx_err,
};
use crate::infra::storage::mapper::record_row_to_model;
use crate::infra::storage::query::aggregate::build_aggregate_sql;
use crate::infra::storage::query::effective_page_size;
use crate::infra::storage::query::feed::build_feed_page_sql;
use crate::infra::storage::query::list::{
    DEFAULT_PAGE_SIZE, build_get_sql, build_list_page, build_list_sql,
};
use crate::infra::storage::query::reconciliation::{
    build_reconciliation_summary_sql, build_reconciliation_watermarks_sql,
};
use crate::infra::storage::query::rollup::{build_rollup_aggregate_sql, rollup_eligible};
use crate::infra::storage::query::translate::{bind_one, bind_one_query};
use crate::infra::storage::retry::{
    MAX_BATCH_ATTEMPTS, batch_retry_backoff, is_retryable_batch_error, with_retry,
};
use crate::infra::storage::type_key::TypeKeyCache;

use feed::dedup_subscription_types;

/// `sqlx`-backed implementation of [`RecordStore`] over the `usage_records`
/// hypertable.
///
/// Every operation acquires its connection through `Self::timed_acquire`, so
/// `uc_timescaledb_pool_acquire_duration_seconds` is recorded per acquire and
/// `uc_timescaledb_tls_handshake_failures_total` is incremented when a fresh
/// physical connection fails its TLS handshake (via
/// `Self::record_backend_error`). Both are named in plain backticks rather than
/// linked: they are private, so an intra-doc link from this public item would
/// resolve only under `--document-private-items`.
#[derive(Debug, Clone)]
pub struct PgRecordStore {
    pool: PgPool,
    metrics: Arc<Metrics>,
    cancel: CancellationToken,
    type_keys: Arc<TypeKeyCache>,
    /// Whether `aggregate` may serve an eligible query from `usage_rollup_1h`.
    /// Always `true` outside tests; `Self::without_rollup` turns it off so
    /// the integration suite can compare both reads over one database. Not
    /// linked, because that constructor is `#[cfg(any(test, feature =
    /// "postgres"))]` and so does not exist in a default-feature doc build.
    rollup_enabled: bool,
    /// `feed_acceptance_slack_secs`, bound into every write statement's
    /// admission guard (see
    /// [`admitted_expr`](crate::infra::storage::query::write::admitted_expr)).
    /// Held as the `i64` it binds as. Config validation caps it far below
    /// `i64::MAX`, so [`Self::new`]'s saturation is unreachable.
    acceptance_slack_secs: i64,
}

impl PgRecordStore {
    /// Build a store over an existing connection pool. `cancel` is the gear's
    /// cancellation token; the request path stops re-arming the `ready` gauge
    /// once it fires so a drain-time acquire cannot flip readiness back on after
    /// the shutdown watcher has cleared it.
    ///
    /// `acceptance_slack_secs` is the deployment's `feed_acceptance_slack_secs`
    /// (`crate::config`), and it reaches the database as a bind on every write
    /// rather than as anything this type decides.
    #[must_use]
    pub fn new(
        pool: PgPool,
        metrics: Arc<Metrics>,
        cancel: CancellationToken,
        acceptance_slack_secs: u64,
    ) -> Self {
        Self {
            pool,
            metrics,
            cancel,
            type_keys: Arc::new(TypeKeyCache::default()),
            rollup_enabled: true,
            acceptance_slack_secs: i64::try_from(acceptance_slack_secs).unwrap_or(i64::MAX),
        }
    }

    /// The same store with the rollup read switched off, so every aggregate
    /// takes the exact scan. Test-only: the equivalence suite compares the two.
    #[cfg(any(test, feature = "postgres"))]
    #[must_use]
    pub fn without_rollup(mut self) -> Self {
        self.rollup_enabled = false;
        self
    }

    /// Map a `sqlx` error via [`map_sqlx_err`] and, as a side effect, increment
    /// the backend-error counter under the matching [`ErrorClass`]
    /// ([`ErrorClass::Transient`] for a [`UsageCollectorPluginError::Transient`]
    /// mapping, otherwise [`ErrorClass::Internal`]). Returns the mapped error so
    /// it slots into the existing `.map_err(...)` call sites unchanged.
    fn record_backend_error(&self, err: &sqlx::Error) -> UsageCollectorPluginError {
        // A TLS handshake failure is the plugin's one metered transport-security
        // signal (`uc_timescaledb_tls_handshake_failures_total`); count it
        // before the generic mapping.
        if matches!(err, sqlx::Error::Tls(_)) {
            self.metrics.inc_tls_handshake_failure();
        }
        let mapped = map_sqlx_err(err);
        let class = if matches!(mapped, UsageCollectorPluginError::Transient { .. }) {
            ErrorClass::Transient
        } else {
            ErrorClass::Internal
        };
        self.metrics.inc_backend_error(class);
        mapped
    }

    /// [`Self::record_backend_error`], plus the one insert-path error that is
    /// retryable without being a connectivity or serialization fault: a PRIMARY
    /// KEY collision on one of this write's own dedup identities
    /// ([`is_ledger_pk_violation`]). The case it is **sized for** is a
    /// concurrent writer of that identity;
    /// [`entry_identity`](write::entry_identity) sets out a second route to the
    /// same raise, which a mis-derived `id` reaches with no concurrency at all.
    ///
    /// **It is lifted here rather than in
    /// [`crate::infra::storage::error::classify_db`]**, which still maps a
    /// primary-key `23505` to a non-retryable `Internal` — right for a collision
    /// nothing intercepted. Only a write path knows it is in the race, so only a
    /// write path may reclassify it.
    ///
    /// **Re-running resolves the race, and it must be a fresh transaction**: the
    /// winner is committed by the time this error is raised, so the re-run's
    /// `ON CONFLICT` pre-check sees its row and the batch resolves it as an
    /// ordinary dedup hit. `create_batch`'s bounded jittered retry
    /// ([`with_retry`]) is exactly that.
    ///
    /// **It does not resolve the other route**: on a mis-derived `id` the re-run
    /// finds nothing, meets the same primary-key entry again, and
    /// [`MAX_BATCH_ATTEMPTS`] is spent before the host is told. The detail string
    /// names the race deliberately — re-wording it would buy a caller nothing it
    /// can act on differently.
    fn record_insert_error(&self, err: &sqlx::Error) -> UsageCollectorPluginError {
        if is_ledger_pk_violation(err) {
            self.metrics.inc_backend_error(ErrorClass::Transient);
            return UsageCollectorPluginError::transient(
                "a concurrent write of the same usage-entry identity committed first",
            );
        }
        self.record_backend_error(err)
    }

    /// Acquire a pooled connection, recording
    /// `uc_timescaledb_pool_acquire_duration_seconds`. Errors map
    /// through [`Self::record_backend_error`] (which also catches a TLS-handshake
    /// failure on a fresh physical connection). Every operation acquires through
    /// this path so the acquire-latency histogram is representative.
    // @cpt-algo:cpt-cf-uc-plugin-algo-backend-readiness-gauge:p2
    // @cpt-dod:cpt-cf-uc-plugin-dod-readiness-gauge-semantics:p2
    // @cpt-state:cpt-cf-uc-plugin-state-backend-readiness:p2
    async fn timed_acquire(&self) -> Result<PoolConnection<Postgres>, UsageCollectorPluginError> {
        let t = Instant::now();
        match self.pool.acquire().await {
            Ok(conn) => {
                self.metrics.record_pool_acquire(t.elapsed().as_secs_f64());
                // A successful acquire re-arms `uc_timescaledb_ready`, but only
                // while not shutting down: once `cancel` fires the shutdown
                // watcher owns the gauge. This gate narrows but does not close
                // the check-then-set race against the watcher; the residual is a
                // sub-tick blip on a best-effort gauge during one-way shutdown.
                if !self.cancel.is_cancelled() {
                    self.metrics.set_ready(true);
                }
                Ok(conn)
            }
            Err(e) => {
                // Clear readiness only on a connectivity-class failure so the
                // `uc_timescaledb_ready == 0` alert fires on a live outage but
                // not on a healthy-but-saturated pool (`PoolTimedOut` while the
                // pool still holds connections), which would otherwise flap the
                // gauge under load.
                if acquire_error_clears_readiness(&e, self.pool.size()) {
                    self.metrics.set_ready(false);
                }
                Err(self.record_backend_error(&e))
            }
        }
    }
}

/// Read one aggregate result row into a bucket: `dim_count` dimension columns
/// as the key, then the folded value.
///
/// `dim_count` is
/// [`AggregateStatement`](crate::infra::storage::query::aggregate::AggregateStatement)'s, so it is the count the builder
/// numbered its own `GROUP BY` ordinals with rather than a second reading of
/// `group_by` here — the two could otherwise drift with nothing to notice, no
/// unit test executing a statement.
///
/// Split out so the caller is a 1:1 `map` over the fetched rows — the shape the
/// no-grouping case needs: with no `GROUP BY` the statement is a bare aggregate
/// and `PostgreSQL` answers exactly one row, which becomes one bucket with an
/// empty key. That, not an empty bucket list, is what a conforming plugin owes.
///
/// A `NULL` dimension reads as the empty string. With the presence guards
/// [`dimension_presence_guard`](crate::infra::storage::query::aggregate::dimension_presence_guard)
/// emits, the nullable dimensions cannot produce one; the fallback stands for
/// the columns that never can.
///
/// # Errors
///
/// Returns [`UsageCollectorPluginError::Internal`] when a column cannot be
/// decoded at its expected type — `TEXT` for a dimension, `numeric` for the
/// fold.
// @cpt-algo:cpt-cf-uc-plugin-algo-bucket-key-rendering:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-bucket-key-rendering:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-empty-selection-values:p1
fn aggregate_bucket(
    row: &PgRow,
    dim_count: usize,
) -> Result<AggregationBucket, UsageCollectorPluginError> {
    let mut key = Vec::with_capacity(dim_count);
    for i in 0..dim_count {
        let dim = row.try_get::<Option<String>, _>(i).map_err(|e| {
            UsageCollectorPluginError::internal(format!(
                "aggregate dimension column {i} read failed: {e}"
            ))
        })?;
        key.push(dim.unwrap_or_default());
    }
    let value = row
        .try_get::<Option<BigDecimal>, _>(dim_count)
        .map_err(|e| {
            UsageCollectorPluginError::internal(format!(
                "aggregate value column {dim_count} read failed: {e}"
            ))
        })?;
    Ok(AggregationBucket { key, value })
}

#[async_trait]
impl RecordStore for PgRecordStore {
    // @cpt-flow:cpt-cf-uc-plugin-seq-ingest-batch:p2
    // @cpt-flow:cpt-cf-uc-plugin-flow-persist-entry-batch:p1
    // @cpt-flow:cpt-cf-uc-plugin-flow-persist-withdrawal:p1
    // @cpt-state:cpt-cf-uc-plugin-state-entry-withdrawal:p2
    // @cpt-dod:cpt-cf-uc-plugin-dod-record-and-withdrawal-coexist:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-withdrawal-as-appended-entry:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-no-mutation-of-target:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-linearizable-dedup-level:p1
    async fn create_batch(
        &self,
        records: Vec<MeteredRecord>,
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        if records.is_empty() {
            tracing::warn!(
                "create_usage_records called with an empty batch (host-contract breach)"
            );
            return Err(UsageCollectorPluginError::internal(
                "create_usage_records called with an empty batch (host-contract breach)",
            ));
        }

        // Bounded retry around the whole call: on an outer `Transient`, re-run
        // up to `MAX_BATCH_ATTEMPTS` times. `is_retryable_batch_error` lists the
        // causes that reach here. Each attempt acquires a fresh connection and
        // opens a fresh transaction, so a rolled-back attempt leaves no state
        // behind; the transaction is atomic and the identities make it
        // idempotent, so a re-run cannot double-write. `Ok(vec)` is never
        // retried — per-row `Transient` outcomes inside it are the host's to
        // handle, and retrying them would be a correctness bug.
        let n = records.len();
        // Timed once including any retries, regardless of outcome.
        let t = Instant::now();
        let result = with_retry(
            MAX_BATCH_ATTEMPTS,
            batch_retry_backoff,
            is_retryable_batch_error,
            |attempt, err| {
                // A distinct warn + counter, so a self-healed transient can be
                // told apart from a returned one (which never moves this
                // counter).
                tracing::warn!(
                    attempt,
                    max_attempts = MAX_BATCH_ATTEMPTS,
                    error = %err,
                    "retrying usage-record batch write after transient backend error"
                );
                self.metrics.inc_batch_retry();
            },
            || self.create_batch_inner(&records),
        )
        .await;
        self.metrics.record_insert(t.elapsed().as_secs_f64());
        if result.is_ok() {
            // `n` is a row count: convert via `try_from` (no `as` cast),
            // saturating an implausibly huge batch to `u32::MAX` before f64.
            self.metrics
                .record_batch_rows(f64::from(u32::try_from(n).unwrap_or(u32::MAX)));
        }
        result
    }

    /// Point lookup by `id`, intersected with the caller's compiled PDP scope.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorPluginError::UsageRecordNotFound`] when no row
    /// satisfies both the `id` and the scope — the two cases are one answer, on
    /// purpose — [`UsageCollectorPluginError::Internal`] when the scope cannot
    /// be translated or a stored row cannot be mapped, and the mapped backend
    /// error when the query itself fails.
    // @cpt-algo:cpt-cf-uc-plugin-algo-scoped-point-lookup:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-scoped-point-lookup:p1
    // @cpt-algo:cpt-cf-uc-plugin-algo-consistency-profile-publication:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-consistency-profile-publication:p1
    async fn get(
        &self,
        id: Uuid,
        scope: &ast::Expr,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        // Lookup by the public `id`. This relies on a one-record-per-`id`
        // contract, which the hypertable schema cannot enforce on its own — a
        // `UNIQUE` there must include every partition column, so only the
        // composite PK `(id, window_end, type_key)` is enforced.
        // `fetch_optional` therefore returns the first matching row.
        //
        // `id` is the derived dedup identity (`entry_identity`), so each stored
        // row carries a distinct `id` and `WHERE id = $1` matches at most one
        // row. A withdrawn record and the withdrawal naming it are two `id`s,
        // differing in the entry-kind input. The derivation keys on the
        // *identifier* while `usage_records_dedup_uniq` spans `gts_type_uuid`;
        // the two partition rows into identical identity classes under the
        // managed identifier profile types-registry ADR-0001 admits, and into
        // coarser ones where an external source serves two identifiers embedding
        // one explicit UUID tail — never finer.
        //
        // **No `invalidates` predicate belongs in this query, and none ever
        // will.** The asymmetry with the fold is deliberate: `aggregate`
        // excludes a withdrawn pair because a total that counts one is wrong,
        // and that is a derived view; this is the ledger itself. The SPI states
        // it outright — a withdrawn pair MUST be returned as persisted here and
        // from `list_usage_records` — because hiding either half destroys the
        // audit trail the append-only model exists to keep. A consumer wanting
        // the netted view has what it needs: an invalidation names its target
        // (`UsageRecord::invalidation`), so the fold happens on the reader's
        // side.
        //
        // The scope is translated before a connection is acquired, so a scope
        // that cannot be rendered never reaches the pool and never reads a row.
        let (sql, binds) = build_get_sql(scope).map_err(UsageCollectorPluginError::internal)?;
        let mut q = sqlx::query_as::<_, UsageRecordRow>(AssertSqlSafe(sql)).bind(id);
        for b in &binds {
            q = bind_one(q, b);
        }
        let mut conn = self.timed_acquire().await?;
        let row = q
            .fetch_optional(&mut *conn)
            .await
            .map_err(|err| self.record_backend_error(&err))?;

        match row {
            Some(row) => record_row_to_model(row),
            // One arm for both "no such entry" and "exists, but outside your
            // scope": the scope is part of the `WHERE`, so a withheld row is
            // already indistinguishable from an absent one by the time we get
            // here. Nothing may be logged, counted or timed that would tell
            // them apart — that distinction *is* the existence oracle.
            None => Err(UsageCollectorPluginError::UsageRecordNotFound { id }),
        }
    }

    /// One feed page, run as the six-step protocol this plugin's
    /// `docs/DESIGN.md` §3.6 `cpt-cf-uc-plugin-seq-feed-page` sets out.
    ///
    /// Steps 1-5 run on one connection, inside a real [`sqlx::Transaction`]
    /// opened with [`sqlx::Connection::begin_with`] rather than a raw `BEGIN`.
    /// That is not cosmetic: `sqlx` tracks an open `Transaction`, so if this
    /// future is dropped before [`sqlx::Transaction::commit`] runs (a host-side
    /// timeout or a client disconnect) the drop queues a `ROLLBACK`. Without
    /// that tracking the connection would return to the pool still holding an
    /// open `REPEATABLE READ READ ONLY` transaction, and the next caller to
    /// acquire it would run inside an abandoned snapshot.
    ///
    /// The transaction borrows `conn` only until [`sqlx::Transaction::commit`]
    /// consumes it, so step 6 — the authoritative autocommit mark re-check —
    /// runs on the same pooled connection afterward, and both it and step 3's
    /// fast-path check reach one [`Self::mark_stands_above`].
    ///
    /// Steps 2 to 4 are [`Self::feed_page_in_transaction`], in [`feed`].
    ///
    /// # Errors
    ///
    /// See [`RecordStore::feed_page`].
    // @cpt-flow:cpt-cf-uc-plugin-seq-feed-page:p2
    // @cpt-flow:cpt-cf-uc-plugin-flow-read-feed-page:p1
    // @cpt-algo:cpt-cf-uc-plugin-algo-feed-page-protocol:p1
    // @cpt-state:cpt-cf-uc-plugin-state-feed-position:p2
    // @cpt-dod:cpt-cf-uc-plugin-dod-settled-page-protocol:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-feed-completeness:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-named-start:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-gateway-owned-position:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-feed-freshness-bound:p1
    async fn feed_page(
        &self,
        subscription: &[Uuid],
        scope: &ast::Expr,
        after: Option<(u64, Uuid)>,
        until: Option<(u64, Uuid)>,
        limit: u64,
    ) -> Result<FeedPageRows, UsageCollectorPluginError> {
        // The drop-timer records `uc_timescaledb_feed_page_duration_seconds` on
        // every return, including the validation and refusal error arms below.
        let _timer = OpDurationGuard::start(Arc::clone(&self.metrics), TimedOp::FeedPage);

        // Translated before a connection is acquired, so a scope that cannot
        // be rendered never opens a transaction — the discipline `get`
        // already keeps.
        let (sql, binds) = build_feed_page_sql(after, until, scope, limit)
            .map_err(UsageCollectorPluginError::internal)?;
        // Deduplicated, order-preserving: `unnest($1::uuid[])` iterates per
        // array *element*, so a repeated type would deliver its rows twice.
        // `FeedSubscription::new` already sorts and dedups, but that invariant
        // lives two crates away, so this statement keeps its own correctness
        // local. `sort_unstable` + `dedup` would also dedup but erase the
        // caller's order, which `unnest` walks the array in.
        let types = dedup_subscription_types(subscription);

        let mut conn = self.timed_acquire().await?;

        // Step 1. READ ONLY because nothing here writes, and REPEATABLE READ
        // because the horizon read must fix the snapshot the page statement
        // then runs under. See this method's own doc for why `begin_with`
        // rather than a raw `BEGIN` statement.
        let mut tx = conn
            .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        let page = self
            .feed_page_in_transaction(&mut tx, &sql, &binds, &types, after, until, limit)
            .await;

        // Step 5, on the success path only. `?` propagates a steps-2-4 error
        // before `tx` is committed; dropping an uncommitted `Transaction` queues
        // its `ROLLBACK`, so an error is rolled back rather than left for an
        // unconditional `COMMIT` to paper over.
        let page = page?;
        tx.commit()
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        // Step 6. Authoritative, in autocommit, after the COMMIT: step 3 runs
        // under the page's snapshot and can miss a drop that commits after
        // it. Short-circuiting on `page.marked` is what keeps this the
        // "opt page_after present, not marked" branch of DESIGN §3.6's
        // diagram: a mark the fast path already found is not re-read here.
        //
        // One shared return for both refusal reasons (step 3's fast-path find
        // or step 6's live re-check), so one counter call here covers both:
        // the counter counts refusals, not re-checks, and this method never
        // returns `CursorBeyondRetention` from anywhere else.
        if let Some(position) = after
            && (page.marked || self.mark_stands_above(&mut conn, &types, position).await?)
        {
            self.metrics.inc_feed_cursor_refusal();
            return Err(UsageCollectorPluginError::CursorBeyondRetention);
        }

        Ok(FeedPageRows {
            entries: page.entries,
            next: page.next,
        })
    }

    /// Keyset-paginated ledger read over one meter and one covered-period
    /// range, with the caller's composed `$filter`, the metadata side channel
    /// and an optional keyset.
    ///
    /// The statement is
    /// [`build_list_sql`](crate::infra::storage::query::list::build_list_sql)'s
    /// and the page is
    /// [`build_list_page`](crate::infra::storage::query::list::build_list_page)'s;
    /// what is left here is the round trip between them. Both halves are pure,
    /// so both are tested without a database.
    ///
    /// Selection reads the covered-period end alone, `from <= window_end <
    /// to`, and entries are returned as persisted, withdrawn pairs included.
    /// Each of the two is stated where it is enforced rather than only here.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorPluginError::Internal`] when the statement
    /// cannot be built (an untranslatable `$filter`, an order or keyset field
    /// off the allowlist) or the page cannot be assembled (an order field
    /// that is not a keyset key, a keyset that is empty or too large, a
    /// stored row that cannot be mapped), and the mapped backend error when
    /// the query itself fails.
    // @cpt-flow:cpt-cf-uc-plugin-seq-list-keyset:p2
    // @cpt-flow:cpt-cf-uc-plugin-flow-read-raw-page:p1
    // @cpt-algo:cpt-cf-uc-plugin-algo-consistency-profile-publication:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-consistency-profile-publication:p1
    async fn list(
        &self,
        gts_type_uuid: Uuid,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        keyset: Option<&Keyset>,
    ) -> Result<RecordPage, UsageCollectorPluginError> {
        // The drop-timer records the histogram on every return, including the
        // validation error arms below, not just on success.
        let _timer =
            OpDurationGuard::start(Arc::clone(&self.metrics), TimedOp::Query(QueryKind::Raw));
        self.metrics.inc_query_request(QueryKind::Raw);

        // Defense-in-depth: clamp the caller's `$top` to `MAX_PAGE_SIZE` so a
        // value that slipped past the core gateway's `$top` cap can never drive
        // an unbounded `LIMIT n+1 ... fetch_all`.
        let limit = effective_page_size(query.limit, DEFAULT_PAGE_SIZE);

        // Built before a connection is acquired, so a query that cannot be
        // rendered never reaches the pool and never reads a row.
        let (sql, binds) = build_list_sql(
            gts_type_uuid,
            time_range,
            query,
            metadata_filter,
            keyset,
            limit,
        )
        .map_err(UsageCollectorPluginError::internal)?;

        let mut q = sqlx::query_as::<_, UsageRecordRow>(AssertSqlSafe(sql));
        for b in &binds {
            q = bind_one(q, b);
        }
        let mut conn = self.timed_acquire().await?;
        let rows = q
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        build_list_page(rows, query, limit)
    }

    /// Pushed-down fold over one meter's entries in one covered-period range,
    /// optionally grouped.
    ///
    /// The statement is
    /// [`build_aggregate_sql`](crate::infra::storage::query::aggregate::build_aggregate_sql)'s;
    /// the rules it encodes are documented there. Two properties are this
    /// method's rather than the builder's:
    ///
    /// - **The statement is built before a connection is acquired**, so a query
    ///   that cannot be rendered never reaches the pool and never reads a row.
    /// - **Each fetched row becomes exactly one bucket**
    ///   ([`aggregate_bucket`]), so an empty `group_by` — a bare aggregate with
    ///   no `GROUP BY`, which `PostgreSQL` answers with exactly one row — yields
    ///   the single empty-keyed bucket the SPI asks for rather than an empty
    ///   bucket list. That bucket carries `Some(0)` under `COUNT` **and** under
    ///   `SUM`, both of which DESIGN §3.3 defines over an empty selection;
    ///   `MAX`, `MIN` and `LATEST` are not defined over one and keep the `None`
    ///   ([`usage_collector_sdk::v2::AggregationBucket::value`]). The two zeroes
    ///   arrive differently: `COUNT` is `SELECT COUNT(*)`'s own answer, while
    ///   `SUM` over zero rows is `NULL` in `PostgreSQL` and is made `0` by a
    ///   `COALESCE` in the statement —
    ///   [`fold_select_expr`](crate::infra::storage::query::aggregate::fold_select_expr)
    ///   on the scan path, [`build_rollup_aggregate_sql`] on the rollup one.
    ///
    /// Both are held by a pair of tests against a lazy pool at a dead DSN, where
    /// reaching the pool answers `Transient` and stopping before it answers
    /// `Internal`: `a_fold_that_cannot_be_built_never_reaches_the_pool` requires
    /// the `Internal`, `the_ungrouped_fold_still_reaches_the_pool` the
    /// `Transient`. Either alone would leave the ordering unpinned.
    ///
    /// The dimension columns read positionally as `Option<String>` and the fold
    /// at index `k` as `Option<BigDecimal>` — arbitrary precision, so a wide
    /// `SUM` cannot overflow on decode.
    ///
    /// **`LATEST` materializes before it picks, so its peak memory is O(largest
    /// group).** The expression is `(ARRAY_AGG(r.quantity ORDER BY …))[1]`, so
    /// `PostgreSQL` builds a group's values into an array before taking the
    /// head. An aggregate carrying its own `ORDER BY` takes the grouped node off
    /// the hash path, so the plan is `Sort → GroupAggregate` and exactly one
    /// array is live at a time. Peak memory therefore grows with the row count
    /// of the largest group, not with the number of groups, and the array does
    /// not spill. (The `Sort` beneath is `work_mem`-bounded and needed by every
    /// candidate formulation, so it is not a cost of this one.)
    ///
    /// **Nothing bounds the rows within one group except the caller's covered
    /// period, which is a request parameter.**
    /// [`aggregate_limit_clause`](crate::infra::storage::query::aggregate::aggregate_limit_clause)
    /// bounds the number of groups and never the rows inside one, so a `LATEST`
    /// read with a wide largest group is a server-side allocation sized by
    /// caller input. `SUM`, `COUNT`, `MAX` and `MIN` accumulate fixed state per
    /// group.
    ///
    /// **Two alternatives are O(1) per group and both are kept out.**
    /// `DISTINCT ON` and `ROW_NUMBER() OVER (PARTITION BY …) = 1` avoid the
    /// array, but neither is a `SELECT`-list expression a caller can drop in
    /// beside `SUM`, and neither expresses the ungrouped fold — `DISTINCT ON ()`
    /// is a syntax error, and `PARTITION BY` nothing answers **zero** rows over
    /// an empty selection where this method owes exactly one empty-keyed bucket.
    ///
    /// An eligible `SUM` or `COUNT` is served from `usage_rollup_1h` (see
    /// [`crate::infra::storage::query::rollup`]); the result is numerically
    /// identical to the scan's, up to the rollup's refresh watermark (spec §8.1)
    /// and given spec §5.2's invariants. Both routes return one
    /// [`AggregateStatement`](crate::infra::storage::query::aggregate::AggregateStatement),
    /// so the choice of route is the only thing this
    /// method branches on — the running and decoding below is shared.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorPluginError::Internal`] when the composed filter
    /// cannot be translated, the query fails, or a result column cannot be
    /// decoded at its expected type; a pool-acquisition failure surfaces as
    /// whatever `timed_acquire` classifies it as.
    // @cpt-flow:cpt-cf-uc-plugin-seq-query-aggregated:p2
    // @cpt-flow:cpt-cf-uc-plugin-flow-run-aggregated-query:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-rollup-eligibility-and-path-reporting:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-latest-memory-bound-publication:p1
    async fn aggregate(
        &self,
        gts_type_uuid: Uuid,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        // The drop-timer records the histogram on every return, not just
        // success.
        let _timer = OpDurationGuard::start(
            Arc::clone(&self.metrics),
            TimedOp::Query(QueryKind::Aggregated),
        );
        self.metrics.inc_query_request(QueryKind::Aggregated);

        // The rollup answers an eligible SUM/COUNT exactly (spec §6); every
        // other query takes the scan it always has. The eligibility test reads
        // the same composed filter the scan would translate.
        let routed = if self.rollup_enabled {
            Some(rollup_eligible(
                fold,
                query.filter(),
                metadata_filter,
                group_by,
                time_range,
            ))
        } else {
            None
        };
        let statement = match routed {
            Some(Ok(split)) => {
                let st = build_rollup_aggregate_sql(
                    gts_type_uuid,
                    split,
                    fold,
                    query.filter(),
                    group_by,
                )
                .map_err(UsageCollectorPluginError::internal)?;
                self.metrics.record_aggregate_path(None);
                st
            }
            other => {
                if let Some(Err(reason)) = other {
                    self.metrics.record_aggregate_path(Some(reason));
                }
                build_aggregate_sql(
                    gts_type_uuid,
                    time_range,
                    fold,
                    query,
                    metadata_filter,
                    group_by,
                )
                .map_err(UsageCollectorPluginError::internal)?
            }
        };

        let mut q = sqlx::query(AssertSqlSafe(statement.sql));
        for b in &statement.binds {
            q = bind_one_query(q, b);
        }
        let mut conn = self.timed_acquire().await?;
        let rows = q
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        // One bucket per row, in the order `PostgreSQL` emitted them. The `map`
        // keeps the no-grouping case honest: no branch on `group_by.is_empty()`
        // exists to answer an empty bucket list with. The count comes from the
        // statement that was built, not a second reading of `group_by`, so the
        // decoder cannot read a different number of key columns than the SELECT
        // list emits.
        let buckets = rows
            .iter()
            .map(|row| aggregate_bucket(row, statement.dim_count))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(AggregationResult { buckets })
    }

    /// Spec §9.3's three selections, run as
    /// [`build_reconciliation_summary_sql`]'s S1∧S2 statement followed by
    /// [`build_reconciliation_watermarks_sql`]'s S3, on the same pooled
    /// connection.
    ///
    /// **[`OpDurationGuard`] with [`TimedOp::Reconciliation`], not a third
    /// [`QueryKind`] variant.** This plugin's `QueryKind` is fixed at
    /// `{Aggregated, Raw}` by `docs/DESIGN.md`, so a third value would emit a
    /// label that document does not declare.
    /// `uc_timescaledb_reconciliation_duration_seconds` (§4.3) is unlabelled, as
    /// `uc_timescaledb_feed_page_duration_seconds` is, so a dedicated label-free
    /// `TimedOp` variant records it.
    ///
    /// Two statements rather than one: see
    /// [`build_reconciliation_watermarks_sql`] for why S3 cannot join S1∧S2's
    /// ranged scan without forfeiting the leading-edge index read. Both run
    /// against the one connection [`Self::timed_acquire`] returns, but are not
    /// wrapped in one transaction: `READ COMMITTED` still lets a commit land
    /// between them, which the DESIGN sequence does not rule out either.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorPluginError::Internal`] when `scope` cannot be
    /// translated, when either statement fails, or when a result column
    /// cannot be decoded at its expected type; a pool-acquisition failure
    /// surfaces as whatever [`Self::timed_acquire`] classifies it as.
    // @cpt-flow:cpt-cf-uc-plugin-seq-reconciliation:p2
    // @cpt-flow:cpt-cf-uc-plugin-flow-read-reconciliation-figures:p2
    // @cpt-algo:cpt-cf-uc-plugin-algo-accepted-count:p2
    // @cpt-algo:cpt-cf-uc-plugin-algo-fold-appropriate-summary:p2
    // @cpt-algo:cpt-cf-uc-plugin-algo-watermark-reads:p2
    // @cpt-dod:cpt-cf-uc-plugin-dod-accepted-count-includes-withdrawals:p2
    // @cpt-dod:cpt-cf-uc-plugin-dod-unbounded-watermarks:p2
    // @cpt-dod:cpt-cf-uc-plugin-dod-scope-applied-first:p2
    // @cpt-dod:cpt-cf-uc-plugin-dod-empty-selection-figures:p2
    // @cpt-dod:cpt-cf-uc-plugin-dod-no-paging-no-rollup:p2
    async fn reconciliation(
        &self,
        tenant_id: Uuid,
        meter: &MeterRef,
        time_range: TimeRange,
        fold: AggregationFold,
        scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        let gts_type_uuid = meter.uuid;

        // The drop-timer records
        // `uc_timescaledb_reconciliation_duration_seconds` on every return,
        // including the validation and decode error arms below.
        let _timer = OpDurationGuard::start(Arc::clone(&self.metrics), TimedOp::Reconciliation);

        // Both statements are built before a connection is acquired, so a
        // scope that cannot be rendered never reaches the pool and never
        // reads a row.
        let summary_stmt =
            build_reconciliation_summary_sql(gts_type_uuid, time_range, tenant_id, fold, scope)
                .map_err(UsageCollectorPluginError::internal)?;
        let watermarks_stmt = build_reconciliation_watermarks_sql(gts_type_uuid, tenant_id, scope)
            .map_err(UsageCollectorPluginError::internal)?;

        let mut conn = self.timed_acquire().await?;

        let mut q = sqlx::query(AssertSqlSafe(summary_stmt.sql));
        for b in &summary_stmt.binds {
            q = bind_one_query(q, b);
        }
        let summary_row = q
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        // Column 0 is always `accepted_count`, the bare unfiltered `COUNT(*)`
        // — never absent, never negative.
        let accepted_count: i64 = summary_row.try_get(0).map_err(|e| {
            UsageCollectorPluginError::internal(format!(
                "reconciliation accepted_count column read failed: {e}"
            ))
        })?;

        // Columns 1.. are `build_reconciliation_summary_sql`'s two shapes, read
        // by the same `QuantitySummary::accrues` branch that chose which one the
        // statement built, so the decoder cannot read a shape the SELECT list
        // disagrees with.
        let quantity_summary = if QuantitySummary::accrues(fold) {
            let total: BigDecimal = summary_row.try_get(1).map_err(|e| {
                UsageCollectorPluginError::internal(format!(
                    "reconciliation accrued summary column read failed: {e}"
                ))
            })?;
            QuantitySummary::Accrued(total)
        } else {
            let count: i64 = summary_row.try_get(1).map_err(|e| {
                UsageCollectorPluginError::internal(format!(
                    "reconciliation observation count column read failed: {e}"
                ))
            })?;
            let latest: Option<Decimal> = summary_row.try_get(2).map_err(|e| {
                UsageCollectorPluginError::internal(format!(
                    "reconciliation latest observation column read failed: {e}"
                ))
            })?;
            let latest = latest
                .map(UsageQuantity::try_from)
                .transpose()
                .map_err(|e| {
                    UsageCollectorPluginError::internal(format!(
                        "reconciliation latest observation quantity invalid: {e}"
                    ))
                })?;
            // `count` (column 1, `COUNT(*)`) and `latest` (column 2, a separate
            // "pick the latest row" subquery) are two independently computed SQL
            // projections; nothing but the query text keeps them in agreement. A
            // disagreement here means those two projections drifted apart, which
            // `ObservedQuantity` cannot represent, so it is reported rather than
            // silently reshaped into one branch or the other.
            let count = NonZeroU64::new(u64::try_from(count).unwrap_or(u64::MAX));
            let observed = match (count, latest) {
                (None, None) => None,
                (Some(count), Some(latest)) => Some(ObservedQuantity { count, latest }),
                (count, latest) => {
                    return Err(UsageCollectorPluginError::internal(format!(
                        "reconciliation observation count ({count:?}) and latest observation \
                         ({latest:?}) disagree on whether the range selected anything"
                    )));
                }
            };
            QuantitySummary::Observations(observed)
        };

        let mut q = sqlx::query(AssertSqlSafe(watermarks_stmt.sql));
        for b in &watermarks_stmt.binds {
            q = bind_one_query(q, b);
        }
        let watermarks_row = q
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| self.record_backend_error(&e))?;
        let max_accepted_at: Option<OffsetDateTime> = watermarks_row.try_get(0).map_err(|e| {
            UsageCollectorPluginError::internal(format!(
                "reconciliation max_accepted_at column read failed: {e}"
            ))
        })?;
        let max_window_end: Option<OffsetDateTime> = watermarks_row.try_get(1).map_err(|e| {
            UsageCollectorPluginError::internal(format!(
                "reconciliation max_window_end column read failed: {e}"
            ))
        })?;

        Ok(ReconciliationMetadata {
            accepted_count: u64::try_from(accepted_count).unwrap_or(u64::MAX),
            quantity_summary,
            max_accepted_at,
            max_window_end,
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "mod_tests.rs"]
mod mod_tests;
