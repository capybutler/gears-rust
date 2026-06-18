# TimescaleDB Usage Collector Plugin — Code-Review Fixes (Important #1–5) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the five Important findings from the code review of `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin`: align metric names to the (corrected) observability contract, make the `ready` gauge reflect pool-acquire failures, add MIN/MAX/AVG aggregation tests + casts, cover the subject dimension / `subject_ref` round-trip, and support `eq null` / `ne null` filters.

**Architecture:** Pure-persistence storage plugin (TimescaleDB/Postgres) implementing the `UsageCollectorPluginV1` SPI via a `StorageAdapter`. Changes are small and localized to `infra/metrics.rs`, the two stores' `timed_acquire`, `infra/storage/query/{aggregate,translate}.rs`, and the test suites. No schema migration, no SPI change, no unrelated refactoring.

**Tech Stack:** Rust, `sqlx` (Postgres), `opentelemetry` + `opentelemetry_sdk` (metrics), `toolkit_odata` (filter AST), `testcontainers` (`timescale/timescaledb` image, gated behind the `postgres` feature), `rust_decimal`.

**Conventions:**
- Crate package name: `cf-gears-timescaledb-usage-collector-plugin`.
- Unit tests (no Docker): `cargo test -p cf-gears-timescaledb-usage-collector-plugin`
- Integration tests (Docker required): add `--features postgres`
- All paths below are relative to repo root unless noted. The plugin root is:
  `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/` (abbreviated **PLUGIN/** below).
- Work on the current branch `usage-collector/timescaledb-plugin`.
- End every commit message with:
  `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`

**Decision locked during brainstorming:** keep the code's existing `uc.timescaledb.*` metric prefix (do **not** rename to `usage_collector.timescaledb.*`); instead bring DESIGN.md to the code on the prefix, and **drop `.count` from all counters** (OTel-native: instrument kind conveys cumulative semantics). DESIGN.md is otherwise authoritative.

---

## File Map

| File | Change |
| --- | --- |
| `PLUGIN/src/infra/metrics.rs` | Drop `.count` from the 9 counter instrument names + their doc comments. Keep `uc.timescaledb.` prefix. |
| `PLUGIN/src/infra/metrics_tests.rs` | Update the 2 counter-name assertions to the no-`.count` names. |
| `gears/system/usage-collector/docs/timescaledb-usage-collector-storage-plugin/DESIGN.md` | §Observability: prefix `usage_collector.timescaledb.` → `uc.timescaledb.`; drop `.count` from the 8 suffixed counters + the alert source; soften the parent-convention prose. |
| `PLUGIN/src/infra/storage/record_store.rs` | `timed_acquire`: `set_ready(true)` on success, `set_ready(false)` on acquire failure. |
| `PLUGIN/src/infra/storage/catalog_store.rs` | Same `timed_acquire` change. |
| `PLUGIN/src/infra/storage/record_store_tests.rs` | New unit test: acquire failure clears the `ready` gauge. |
| `PLUGIN/src/infra/storage/query/aggregate.rs` | Cast MIN/MAX/AVG to `::numeric`; doc note on AVG precision; wire a new test module. |
| `PLUGIN/src/infra/storage/query/aggregate_tests.rs` | **New file**: unit test pinning `agg_select_expr` strings. |
| `PLUGIN/src/infra/storage/query/translate.rs` | `Binary` arm: `Eq/Ne` + `ODataValue::Null` → `IS NULL` / `IS NOT NULL` (no bind). |
| `PLUGIN/src/infra/storage/query/translate_tests.rs` | New unit tests for `eq null` / `ne null` / `gt null`. |
| `PLUGIN/tests/common/mod.rs` | New `fixture_usage_record_with_subject` helper + `SubjectRef` import. |
| `PLUGIN/tests/records_query_integration_pg.rs` | New integration tests: MIN/MAX/AVG, group-by SubjectId, subject round-trip, `subject_id eq null`. |

---

## Task 1: Metric names → contract (Issue #1)

Drop `.count` from all 9 counter names (code + the metrics-module doc comments + the 2 unit-test assertions), and bring DESIGN.md's §Observability to the code's `uc.timescaledb.*` prefix while dropping the now-redundant `.count` there too. TDD order: update the failing test assertions first, then the code, then the docs.

**Files:**
- Modify: `PLUGIN/src/infra/metrics_tests.rs`
- Modify: `PLUGIN/src/infra/metrics.rs`
- Modify: `gears/system/usage-collector/docs/timescaledb-usage-collector-storage-plugin/DESIGN.md`

- [ ] **Step 1: Update the two counter-name assertions in `metrics_tests.rs` (these will now fail)**

In `PLUGIN/src/infra/metrics_tests.rs`, apply these exact replacements (replace all occurrences):

- `uc.timescaledb.dedup.absorbed.count` → `uc.timescaledb.dedup.absorbed`
- `uc.timescaledb.backend.error.count` → `uc.timescaledb.backend.error`

This affects the `counter_sum`/`counter_sum_with_label` assertions at lines ~170, ~174, ~180. Leave the meter scope `provider.meter("uc.timescaledb")` (line 147) unchanged — the prefix stays.

- [ ] **Step 2: Run the metrics test to verify it fails**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --lib metrics::recording_helpers_emit_expected_series -- --nocapture`
Expected: FAIL — `counter_sum` returns `0` (the instrument is still named `…dedup.absorbed.count` in code, so the new `…dedup.absorbed` name matches nothing), assertion `assert_eq!(…, 3)` fails.

- [ ] **Step 3: Drop `.count` from the 9 counter names in `metrics.rs`**

In `PLUGIN/src/infra/metrics.rs`, apply these exact replacements (replace all occurrences — each hits both the doc comment and the `.u64_counter("…")` builder string):

- `uc.timescaledb.dedup.absorbed.count` → `uc.timescaledb.dedup.absorbed`
- `uc.timescaledb.backend.error.count` → `uc.timescaledb.backend.error`
- `uc.timescaledb.idempotency.conflict.count` → `uc.timescaledb.idempotency.conflict`
- `uc.timescaledb.usage_type.referenced.count` → `uc.timescaledb.usage_type.referenced`
- `uc.timescaledb.migration.failure.count` → `uc.timescaledb.migration.failure`
- `uc.timescaledb.compensation.count` → `uc.timescaledb.compensation`
- `uc.timescaledb.dedup.stale.count` → `uc.timescaledb.dedup.stale`
- `uc.timescaledb.query.requests.count` → `uc.timescaledb.query.requests`
- `uc.timescaledb.tls.handshake.failure.count` → `uc.timescaledb.tls.handshake.failure`

Do **not** touch the histograms/gauges (`insert.duration`, `query.duration`, `deactivate.duration`, `pool.acquire.duration`, `batch.rows`, `pool.connections.active`, `pool.connections.idle`, `usage_type_catalog.size`, `ready`) — they have no `.count` suffix. Do **not** change `SCOPE_NAME` (`"uc.timescaledb"`).

- [ ] **Step 4: Run the metrics tests to verify they pass**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --lib metrics`
Expected: PASS (both `new_constructs_without_panic` and `recording_helpers_emit_expected_series`).

- [ ] **Step 5: Bring DESIGN.md §Observability to the contract**

In `gears/system/usage-collector/docs/timescaledb-usage-collector-storage-plugin/DESIGN.md`:

a) Replace all `usage_collector.timescaledb.` → `uc.timescaledb.` (the inventory tables, the `ready` note, and the alert table — this also rewrites the "sub-namespace" mention). Note: `usage_collector.plugin.ready`, `usage_collector.pdp.ready`, and the request-path `usage_collector.*` mentions do **not** contain `.timescaledb.` and are correctly left untouched.

b) Then drop `.count` from these 8 counter names (replace all occurrences):
- `uc.timescaledb.dedup.absorbed.count` → `uc.timescaledb.dedup.absorbed`
- `uc.timescaledb.dedup.stale.count` → `uc.timescaledb.dedup.stale`
- `uc.timescaledb.backend.error.count` → `uc.timescaledb.backend.error`
- `uc.timescaledb.idempotency.conflict.count` → `uc.timescaledb.idempotency.conflict`
- `uc.timescaledb.usage_type.referenced.count` → `uc.timescaledb.usage_type.referenced`
- `uc.timescaledb.migration.failure.count` → `uc.timescaledb.migration.failure`
- `uc.timescaledb.tls.handshake.failure.count` → `uc.timescaledb.tls.handshake.failure`
- `uc.timescaledb.compensation.count` → `uc.timescaledb.compensation`

(`query.requests` already has no `.count` in DESIGN. The histogram-`count` prose "rate of `uc.timescaledb.insert.duration` count" is a separate word and is fine.)

c) Soften the parent-convention sentence (the §Observability intro). Replace:

`an active plugin owns its backend-internal series under its own sub-namespace, not part of the gear's contract.`

with:

`an active plugin owns its backend-internal series under its own sub-namespace (here \`uc.timescaledb.\`, abbreviating the collector namespace for brevity), not part of the gear's contract.`

- [ ] **Step 6: Sanity-grep for stragglers**

Run: `grep -rn 'timescaledb\.[a-z_.]*\.count\b' gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src gears/system/usage-collector/docs/timescaledb-usage-collector-storage-plugin/DESIGN.md`
Expected: no matches (every counter name now ends without `.count`).

Run: `grep -rn 'usage_collector\.timescaledb' gears/system/usage-collector/docs/timescaledb-usage-collector-storage-plugin/DESIGN.md`
Expected: no matches (all rewritten to `uc.timescaledb.`).

- [ ] **Step 7: Commit**

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/metrics.rs \
        gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/metrics_tests.rs \
        gears/system/usage-collector/docs/timescaledb-usage-collector-storage-plugin/DESIGN.md
git commit -m "fix(usage-collector): drop .count from plugin metric names; reconcile DESIGN to uc.timescaledb prefix (REVIEW #1)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `ready` gauge cleared on pool-acquire failure (Issue #2)

DESIGN says `uc.timescaledb.ready` is "cleared on a pool-acquire failure" and the readiness alert is "0 for > 1m". Today `set_ready(false)` fires only from the shutdown watcher, so a live pool outage never clears it. Fix: in both stores' `timed_acquire`, set the gauge to `1` on a successful acquire (auto re-arm) and `0` on an acquire failure. TDD: failing unit test first.

**Files:**
- Modify: `PLUGIN/src/infra/storage/record_store_tests.rs`
- Modify: `PLUGIN/src/infra/storage/record_store.rs:97-106`
- Modify: `PLUGIN/src/infra/storage/catalog_store.rs:137-146`

- [ ] **Step 1: Write the failing unit test**

Append to `PLUGIN/src/infra/storage/record_store_tests.rs` (this file is a submodule of `record_store.rs`, so it can call the private `timed_acquire`):

```rust
#[tokio::test]
async fn acquire_failure_clears_ready_gauge() {
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{
        InMemoryMetricExporter, PeriodicReader, SdkMeterProvider,
    };

    // Local in-memory meter provider so the gauge read is parallel-safe (never
    // touches opentelemetry::global), mirroring metrics_tests.
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();

    // A lazy pool pointed at a dead port: the first acquire is refused fast.
    let pool = PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy("postgres://user:pass@127.0.0.1:1/db")
        .expect("a syntactically valid DSN yields a lazy pool without connecting");
    let metrics = Arc::new(Metrics::with_meter(
        &provider.meter("uc.timescaledb"),
        pool.clone(),
    ));
    let store = PgRecordStore::new(pool, metrics);

    // Every operation routes through timed_acquire; call it directly.
    let result = store.timed_acquire().await;
    assert!(result.is_err(), "acquire against a dead port must fail");

    provider.force_flush().expect("flush in-memory metrics");

    // Read the last value of the `uc.timescaledb.ready` gauge.
    let ready = {
        let metrics = exporter.get_finished_metrics().expect("collected metrics");
        let mut found = None;
        for rm in &metrics {
            for sm in rm.scope_metrics() {
                for m in sm.metrics() {
                    if m.name() == "uc.timescaledb.ready"
                        && let AggregatedMetrics::U64(MetricData::Gauge(g)) = m.data()
                    {
                        found = g.data_points().next().map(
                            opentelemetry_sdk::metrics::data::GaugeDataPoint::value,
                        );
                    }
                }
            }
        }
        found
    };
    assert_eq!(
        ready,
        Some(0),
        "a pool-acquire failure must clear the readiness gauge to 0"
    );
}
```

(`PgPoolOptions`, `Duration`, `Arc`, and `Metrics` are already imported at the top of `record_store_tests.rs`.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --lib record_store::record_store_tests::acquire_failure_clears_ready_gauge -- --nocapture`
Expected: FAIL — `assert_eq!(ready, Some(0))` fails with `ready == None` (the gauge is never recorded on the acquire-failure path today).

- [ ] **Step 3: Set the gauge in `record_store.rs::timed_acquire`**

In `PLUGIN/src/infra/storage/record_store.rs`, replace the body of `timed_acquire` (lines ~97-106):

```rust
    async fn timed_acquire(&self) -> Result<PoolConnection<Postgres>, UsageCollectorPluginError> {
        let t = Instant::now();
        match self.pool.acquire().await {
            Ok(conn) => {
                self.metrics.record_pool_acquire(t.elapsed().as_secs_f64());
                // A successful acquire re-arms readiness (DESIGN §Observability:
                // `ready` recovers once the pool serves a connection again).
                self.metrics.set_ready(true);
                Ok(conn)
            }
            Err(e) => {
                // Clear readiness on a pool-acquire failure so the
                // `uc.timescaledb.ready == 0` alert can fire on a live outage.
                self.metrics.set_ready(false);
                Err(self.record_backend_error(&e))
            }
        }
    }
```

- [ ] **Step 4: Make the same change in `catalog_store.rs::timed_acquire`**

In `PLUGIN/src/infra/storage/catalog_store.rs`, replace the body of `timed_acquire` (lines ~137-146):

```rust
    async fn timed_acquire(&self) -> Result<PoolConnection<Postgres>, UsageCollectorPluginError> {
        let t = std::time::Instant::now();
        match self.pool.acquire().await {
            Ok(conn) => {
                self.metrics.record_pool_acquire(t.elapsed().as_secs_f64());
                // A successful acquire re-arms readiness (DESIGN §Observability).
                self.metrics.set_ready(true);
                Ok(conn)
            }
            Err(e) => {
                // Clear readiness on a pool-acquire failure (see record_store).
                self.metrics.set_ready(false);
                Err(self.record_backend_error(&e))
            }
        }
    }
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --lib record_store::record_store_tests::acquire_failure_clears_ready_gauge`
Expected: PASS.

Note on coverage: the success → `ready = 1` re-arm is exercised by `metrics_tests::recording_helpers_emit_expected_series` (which asserts `set_ready(true)` ⇒ gauge `1`) plus every Docker integration test's first successful acquire; a no-DB success-path assertion isn't possible because acquiring a real connection requires a live database.

- [ ] **Step 6: Run the full unit test suite to confirm no regressions**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --lib`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/record_store.rs \
        gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/catalog_store.rs \
        gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/record_store_tests.rs
git commit -m "fix(usage-collector): clear ready gauge on pool-acquire failure, re-arm on success (REVIEW #2)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: MIN/MAX/AVG aggregation casts + tests (Issue #3)

`MIN`/`MAX`/`AVG` are stable SPI ops with zero coverage and, unlike `SUM`/`COUNT`, are not cast to `::numeric`. Cast all five so the "every variant casts to numeric" doc is literally true and read-back survives a future `value` type change; add a unit test pinning the strings (TDD) and integration tests asserting behavior.

**Files:**
- Modify: `PLUGIN/src/infra/storage/query/aggregate.rs`
- Create: `PLUGIN/src/infra/storage/query/aggregate_tests.rs`
- Modify: `PLUGIN/tests/records_query_integration_pg.rs`

- [ ] **Step 1: Create the failing unit test file**

Create `PLUGIN/src/infra/storage/query/aggregate_tests.rs`:

```rust
//! Unit tests for the aggregation SELECT-expression builders. Pure (no DB):
//! they pin the exact SQL each [`AggregationOp`] emits, so a cast regression is
//! caught without Docker.

use usage_collector_sdk::AggregationOp;

use super::agg_select_expr;

#[test]
fn every_aggregate_op_casts_to_numeric() {
    assert_eq!(agg_select_expr(AggregationOp::Sum), "SUM(value)::numeric");
    assert_eq!(agg_select_expr(AggregationOp::Count), "COUNT(*)::numeric");
    assert_eq!(agg_select_expr(AggregationOp::Min), "MIN(value)::numeric");
    assert_eq!(agg_select_expr(AggregationOp::Max), "MAX(value)::numeric");
    assert_eq!(agg_select_expr(AggregationOp::Avg), "AVG(value)::numeric");
}
```

- [ ] **Step 2: Wire the test module at the bottom of `aggregate.rs`**

Append to `PLUGIN/src/infra/storage/query/aggregate.rs` (mirrors the `translate.rs` pattern):

```rust
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "aggregate_tests.rs"]
mod aggregate_tests;
```

- [ ] **Step 3: Run the unit test to verify it fails**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --lib aggregate`
Expected: FAIL — `MIN(value)` ≠ `MIN(value)::numeric` (and likewise MAX/AVG).

- [ ] **Step 4: Cast MIN/MAX/AVG to `::numeric` in `agg_select_expr`**

In `PLUGIN/src/infra/storage/query/aggregate.rs`, replace the `agg_select_expr` match (lines ~31-39):

```rust
#[must_use]
pub fn agg_select_expr(op: AggregationOp) -> &'static str {
    match op {
        AggregationOp::Sum => "SUM(value)::numeric",
        AggregationOp::Count => "COUNT(*)::numeric",
        AggregationOp::Min => "MIN(value)::numeric",
        AggregationOp::Max => "MAX(value)::numeric",
        AggregationOp::Avg => "AVG(value)::numeric",
    }
}
```

And update the module + function doc comments so they're accurate. Replace the bullet in the module doc (lines ~8-10):

```rust
//! - [`agg_select_expr`] — the aggregate column. Every variant casts to
//!   `numeric` (`COUNT(*)::numeric`, `SUM(value)::numeric`, `MIN/MAX/AVG(value)::numeric`)
//!   so the result reads back uniformly as `Option<Decimal>` regardless of the
//!   chosen op. `AVG` read-back assumes the average fits `rust_decimal::Decimal`
//!   (~28 significant digits), which holds for realistic usage quantities.
```

and the function doc (lines ~24-29):

```rust
/// SQL aggregate expression for an [`AggregationOp`].
///
/// Every op casts to `numeric` so the result — including the integer-typed
/// `COUNT(*)` — reads back uniformly as `Option<Decimal>` in `aggregate`. The
/// returned string is a `'static` constant from the closed enum match, never
/// caller text.
```

- [ ] **Step 5: Run the unit test to verify it passes**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --lib aggregate`
Expected: PASS.

- [ ] **Step 6: Add the MIN/MAX/AVG integration test**

Append to `PLUGIN/tests/records_query_integration_pg.rs` (the `AggregationOp`/`AggregationSpec` imports already exist; `record_at`/`setup_with_type`/`BASE_TS`/`VCPU_GTS` are file-local helpers):

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_aggregate_min_max_avg_over_active_rows() {
    let (_h, store) = setup_with_type(VCPU_GTS, &[]).await;
    let tenant = Uuid::from_u128(0x3008);

    // Values {2, 8}: min 2, max 8, avg 5 — exact, so no fractional precision.
    for (i, v) in [2_i64, 8].into_iter().enumerate() {
        let seq = 0x3008_0000 + u128::try_from(i).unwrap();
        let mut rec = record_at(VCPU_GTS, tenant, seq, i64::try_from(i).unwrap());
        rec.value = Decimal::new(v, 0);
        store.create(rec).await.expect("create record");
    }

    for (op, expected) in [
        (AggregationOp::Min, Decimal::new(2, 0)),
        (AggregationOp::Max, Decimal::new(8, 0)),
        (AggregationOp::Avg, Decimal::new(5, 0)),
    ] {
        let spec = AggregationSpec {
            op,
            group_by: Vec::new(),
        };
        let result = store
            .aggregate(
                common::fixture_gts_id(VCPU_GTS),
                &ODataQuery::new(),
                &[],
                spec,
            )
            .await
            .unwrap_or_else(|e| panic!("aggregate {op:?}: {e:?}"));

        assert_eq!(result.buckets.len(), 1, "{op:?}: empty group_by -> one bucket");
        // normalize() strips trailing-zero scale so AVG's numeric `5.0000…`
        // compares equal to `5`.
        assert_eq!(
            result.buckets[0].value.map(|v| v.normalize()),
            Some(expected.normalize()),
            "{op:?} over {{2, 8}}"
        );
    }
}
```

- [ ] **Step 7: Run the integration test (Docker required)**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test records_query_integration_pg pg_aggregate_min_max_avg_over_active_rows -- --nocapture`
Expected: PASS (pulls/starts the `timescale/timescaledb:2.17.2-pg16` container).

- [ ] **Step 8: Commit**

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/query/aggregate.rs \
        gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/query/aggregate_tests.rs \
        gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/tests/records_query_integration_pg.rs
git commit -m "test(usage-collector): cover MIN/MAX/AVG aggregation; cast all agg ops to numeric (REVIEW #3)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Subject dimension + `subject_ref` round-trip (Issue #4)

Every fixture sets `subject_ref: None`, so the subject insert/read path and the `group_by SubjectId` `IS NOT NULL` guard are untested. Add a subject-bearing fixture and two integration tests. These are characterization tests of already-correct behavior (no production-code change).

**Files:**
- Modify: `PLUGIN/tests/common/mod.rs`
- Modify: `PLUGIN/tests/records_query_integration_pg.rs`

- [ ] **Step 1: Add `SubjectRef` to the SDK import in `common/mod.rs`**

In `PLUGIN/tests/common/mod.rs`, change the `usage_collector_sdk` import (lines ~20-22) to add `SubjectRef`:

```rust
use usage_collector_sdk::{
    IdempotencyKey, MetadataKey, ResourceRef, SubjectRef, UsageKind, UsageRecord, UsageType,
    UsageTypeGtsId,
};
```

- [ ] **Step 2: Add the subject fixture helper**

Append to `PLUGIN/tests/common/mod.rs` (after `fixture_usage_record_with_resource`):

```rust
/// Build a [`UsageRecord`] fixture carrying a `subject_ref`.
///
/// [`fixture_usage_record`] leaves `subject_ref` absent; the subject-dimension
/// aggregation and the subject round-trip tests need records that actually
/// persist a subject, so this variant sets [`UsageRecord::subject_ref`] via
/// [`SubjectRef::new`]. `subject_type` is optional. All other fields match
/// [`fixture_usage_record`].
#[must_use]
pub fn fixture_usage_record_with_subject(
    gts: &str,
    tenant_id: Uuid,
    idem: &str,
    value: Decimal,
    seq: u128,
    subject_id: &str,
    subject_type: Option<&str>,
) -> UsageRecord {
    let mut rec = fixture_usage_record(gts, tenant_id, idem, value, seq);
    rec.subject_ref = Some(
        SubjectRef::new(subject_id, subject_type).expect("fixture subject_ref must be valid"),
    );
    rec
}
```

- [ ] **Step 3: Add the round-trip integration test**

Append to `PLUGIN/tests/records_query_integration_pg.rs`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_subject_ref_round_trips_through_create_and_get() {
    let (_h, store) = setup_with_type(VCPU_GTS, &[]).await;
    let tenant = Uuid::from_u128(0x3006);

    let rec = common::fixture_usage_record_with_subject(
        VCPU_GTS,
        tenant,
        "idem-3006-1",
        Decimal::new(1, 0),
        0x3006_0001,
        "subj-1",
        Some("user"),
    );
    let uuid = rec.uuid;
    store.create(rec).await.expect("create with subject");

    let got = store.get(uuid).await.expect("get the subject-bearing record");
    let subject = got.subject_ref.expect("subject_ref must round-trip");
    assert_eq!(subject.subject_id(), "subj-1", "subject_id round-trips");
    assert_eq!(
        subject.subject_type(),
        Some("user"),
        "subject_type round-trips"
    );
}
```

- [ ] **Step 4: Add the group-by-SubjectId integration test**

Append to `PLUGIN/tests/records_query_integration_pg.rs`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_aggregate_group_by_subject_id_excludes_subjectless() {
    let (_h, store) = setup_with_type(VCPU_GTS, &[]).await;
    let tenant = Uuid::from_u128(0x3007);

    // subj-a: 4 + 6 = 10; subj-b: 5; plus one subject-less row (7) that must be
    // excluded from a group-by-subject aggregation per the SDK contract.
    let subject_rows = [
        ("idem-3007-1", 4_i64, 0x3007_0001_u128, "subj-a", 0_i64),
        ("idem-3007-2", 6, 0x3007_0002, "subj-a", 1),
        ("idem-3007-3", 5, 0x3007_0003, "subj-b", 2),
    ];
    for (idem, value, seq, subject_id, ts) in subject_rows {
        let mut rec = common::fixture_usage_record_with_subject(
            VCPU_GTS,
            tenant,
            idem,
            Decimal::new(value, 0),
            seq,
            subject_id,
            None,
        );
        rec.created_at = OffsetDateTime::from_unix_timestamp(BASE_TS + ts).unwrap();
        store.create(rec).await.expect("create subject row");
    }
    // A subject-less row (the IS NOT NULL guard must exclude it from grouping).
    let mut subjectless = record_at(VCPU_GTS, tenant, 0x3007_0004, 3);
    subjectless.value = Decimal::new(7, 0);
    store.create(subjectless).await.expect("create subjectless row");

    let spec = AggregationSpec {
        op: AggregationOp::Sum,
        group_by: vec![AggregationDimension::SubjectId],
    };
    let result = store
        .aggregate(
            common::fixture_gts_id(VCPU_GTS),
            &ODataQuery::new(),
            &[],
            spec,
        )
        .await
        .expect("aggregate group by subject_id");

    assert_eq!(
        result.buckets.len(),
        2,
        "subject-less row is excluded -> two subject buckets"
    );
    let mut got: Vec<(String, Option<Decimal>)> = result
        .buckets
        .iter()
        .map(|b| {
            assert_eq!(b.key.len(), 1, "single grouped dimension -> one key entry");
            (b.key[0].clone(), b.value)
        })
        .collect();
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        got,
        vec![
            ("subj-a".to_owned(), Some(Decimal::new(10, 0))),
            ("subj-b".to_owned(), Some(Decimal::new(5, 0))),
        ],
        "each subject_id bucket carries its summed value"
    );
}
```

- [ ] **Step 5: Run both integration tests (Docker required)**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test records_query_integration_pg pg_subject -- --nocapture`
Expected: PASS for `pg_subject_ref_round_trips_through_create_and_get` and `pg_aggregate_group_by_subject_id_excludes_subjectless`.

- [ ] **Step 6: Commit**

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/tests/common/mod.rs \
        gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/tests/records_query_integration_pg.rs
git commit -m "test(usage-collector): cover subject_ref round-trip and group-by-subject exclusion (REVIEW #4)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: `eq null` / `ne null` filters (Issue #5) — DEFERRED

> **DEFERRED (2026-06-23, user decision):** Implementation revealed that making `eq null`
> work end-to-end requires a fix in the **shared** `libs/toolkit-odata` library —
> `validate_value_type` rejects a `null` operand during type-checking, *before* it
> reaches the plugin's translator. (Notably this is a latent bug: the rest of the stack
> already supports it — `toolkit_odata::schema`'s `is_null()`/`is_not_null()` builders and
> `toolkit-db`'s `filter_node_to_condition` already translate `Null` → `IS NULL`.) Because
> the plugin-only change in this task is dead without that shared-lib fix, and the user
> opted not to modify shared platform infrastructure in this branch, **Task 5 was reverted
> in full** and `subject_id eq null` remains unsupported (errors at validation). Tracked as
> a follow-up: fix `validate_value_type` in `libs/toolkit-odata` (with a regression test)
> as its own change, owned by the toolkit-odata maintainers, then the plugin-side
> translation below can land unchanged.

The steps below are retained for when the follow-up is picked up.

`subject_id eq null` currently errors the whole query (`odata_value_to_bind` rejects `Null`), so callers can't filter nullable columns (`subject_id`, `subject_type`, `corrects_id`) for absence. Fix: in the `Binary` arm, translate `Eq`/`Ne` + `Null` to `IS NULL` / `IS NOT NULL` with no bind; other ops + `Null` keep erroring (ordered comparison to NULL is undefined). TDD: failing unit tests first. **NOTE:** this also requires the `libs/toolkit-odata` `validate_value_type` fix described in the DEFERRED callout above, or the integration test will fail at the validation gate.

**Files:**
- Modify: `PLUGIN/src/infra/storage/query/translate_tests.rs`
- Modify: `PLUGIN/src/infra/storage/query/translate.rs:174-181`
- Modify: `PLUGIN/tests/records_query_integration_pg.rs`

- [ ] **Step 1: Write the failing unit tests**

Append to `PLUGIN/src/infra/storage/query/translate_tests.rs` (the `binary` helper, `SqlCtx`, `translate_record_filter`, `FilterOp`, and `ODataValue` are already imported):

```rust
// ── Null handling (IS NULL / IS NOT NULL) ─────────────────────────────────────

#[test]
fn eq_null_emits_is_null_with_no_bind() {
    let node = binary("subject_id", FilterOp::Eq, ODataValue::Null);
    let mut ctx = SqlCtx::new(1);
    let sql = translate_record_filter(&node, &mut ctx).unwrap();
    assert_eq!(sql, "subject_id IS NULL");
    assert!(ctx.binds.is_empty(), "IS NULL pushes no bind");
}

#[test]
fn ne_null_emits_is_not_null_with_no_bind() {
    let node = binary("subject_id", FilterOp::Ne, ODataValue::Null);
    let mut ctx = SqlCtx::new(1);
    let sql = translate_record_filter(&node, &mut ctx).unwrap();
    assert_eq!(sql, "subject_id IS NOT NULL");
    assert!(ctx.binds.is_empty(), "IS NOT NULL pushes no bind");
}

#[test]
fn ordered_comparison_with_null_is_rejected() {
    let node = binary("created_at", FilterOp::Gt, ODataValue::Null);
    let mut ctx = SqlCtx::new(1);
    assert!(
        translate_record_filter(&node, &mut ctx).is_err(),
        "`> null` has no defined SQL semantics and must error"
    );
}
```

(Allowlisting still runs first: `col(field.name())` is checked before the null branch, and a record filter field can only be one of the nine typed `UsageRecordFilterField` variants, so `eq null` cannot smuggle in an arbitrary column.)

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --lib translate::translate_tests::eq_null_emits_is_null_with_no_bind translate::translate_tests::ne_null_emits_is_not_null_with_no_bind -- --nocapture`
Expected: FAIL — current `Binary` arm calls `odata_value_to_bind(Null)` which returns `Err("null filter value unsupported")`, so `translate_record_filter` errors and `.unwrap()` panics.

- [ ] **Step 3: Handle `Null` in the `Binary` arm of `translate_filter`**

In `PLUGIN/src/infra/storage/query/translate.rs`, replace the `FilterNode::Binary` arm (lines ~175-181):

```rust
        FilterNode::Binary { field, op, value } => {
            let column = col(field.name())
                .ok_or_else(|| format!("field not allowlisted: {}", field.name()))?;
            // `eq null` / `ne null` map to SQL `IS [NOT] NULL` (no bind): the
            // nullable columns (subject_id, subject_type, corrects_id) are
            // legitimate filter targets, and an ordered comparison to NULL is
            // undefined, so only Eq/Ne accept a null operand.
            if matches!(value, ODataValue::Null) {
                return match op {
                    FilterOp::Eq => Ok(format!("{column} IS NULL")),
                    FilterOp::Ne => Ok(format!("{column} IS NOT NULL")),
                    other => Err(format!("operator {other:?} does not accept a null operand")),
                };
            }
            let operator = op_sql(*op)?;
            let n = ctx.push(odata_value_to_bind(value)?);
            Ok(format!("{column} {operator} ${n}"))
        }
```

(`ODataValue` is already in scope via `pub use toolkit_odata::filter::ODataValue;` at the top of `translate.rs`.)

- [ ] **Step 4: Run the unit tests to verify they pass**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --lib translate`
Expected: PASS (all translate tests, including the four new null tests and the existing `null_and_date_and_time_values_are_rejected`, which still asserts `odata_value_to_bind(Null)` is `Err` — that path is now unreachable for Eq/Ne but the bind function's contract is unchanged).

- [ ] **Step 5: Add the integration test**

Append to `PLUGIN/tests/records_query_integration_pg.rs` (depends on `fixture_usage_record_with_subject` from Task 4; `Expr`/`CompareOperator`/`Value` are imported at the top of the file):

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_filter_subject_id_eq_null_returns_only_subjectless() {
    let (_h, store) = setup_with_type(VCPU_GTS, &[]).await;
    let tenant = Uuid::from_u128(0x3009);

    // One row with a subject, one without.
    let with_subject = common::fixture_usage_record_with_subject(
        VCPU_GTS,
        tenant,
        "idem-3009-1",
        Decimal::new(1, 0),
        0x3009_0001,
        "subj-x",
        None,
    );
    store.create(with_subject).await.expect("create subject row");
    let subjectless = record_at(VCPU_GTS, tenant, 0x3009_0002, 1);
    let subjectless_uuid = subjectless.uuid;
    store.create(subjectless).await.expect("create subjectless row");

    // OData `subject_id eq null` -> WHERE subject_id IS NULL.
    let filter = Expr::Compare(
        Box::new(Expr::Identifier("subject_id".to_owned())),
        CompareOperator::Eq,
        Box::new(Expr::Value(Value::Null)),
    );
    let query = ODataQuery::new()
        .with_order(created_at_uuid_asc())
        .with_filter(filter);

    let page = store
        .list(common::fixture_gts_id(VCPU_GTS), &query, &[])
        .await
        .expect("list subject_id eq null");

    assert_eq!(
        page.items.len(),
        1,
        "only the subject-less row matches IS NULL"
    );
    assert_eq!(
        page.items[0].uuid, subjectless_uuid,
        "the matched row is the subject-less one"
    );
}
```

- [ ] **Step 6: Run the integration test (Docker required)**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test records_query_integration_pg pg_filter_subject_id_eq_null -- --nocapture`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/query/translate.rs \
        gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/query/translate_tests.rs \
        gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/tests/records_query_integration_pg.rs
git commit -m "feat(usage-collector): translate eq/ne null to IS [NOT] NULL in record filters (REVIEW #5)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Final verification

Confirm formatting, lints, and the full suites are green before handing off.

**Files:** none (verification only).

- [ ] **Step 1: Format the touched files**

Run: `cargo fmt -p cf-gears-timescaledb-usage-collector-plugin`
Then confirm clean: `cargo fmt -p cf-gears-timescaledb-usage-collector-plugin -- --check`
Expected: no diff.

- [ ] **Step 2: Clippy (both feature sets)**

Run: `cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets -- -D warnings`
Run: `cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets --features postgres -- -D warnings`
Expected: no warnings.

- [ ] **Step 3: Full unit test suite (no Docker)**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --lib`
Expected: PASS.

- [ ] **Step 4: Full integration suite (Docker required)**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --features postgres`
Expected: PASS (all `*_integration_pg` binaries, including the four new tests).

- [ ] **Step 5: If `cargo fmt` changed anything, commit it**

```bash
git add -u gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
git commit -m "style(usage-collector): fmt touched files after review fixes

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Notes / Out of Scope

- **Task 5 (`eq null`) deferred:** reverted in full per user decision — it required a change to the shared `libs/toolkit-odata` library (a latent `validate_value_type` bug) that is out of scope for this plugin branch. `subject_id eq null` remains unsupported (errors at validation). Follow-up: land the `toolkit-odata` fix + regression test as a standalone change owned by that library's maintainers, then apply the retained plugin-side translation in Task 5.
- **Parent-gear convention:** the parent gear DESIGN (`gears/system/usage-collector/docs/DESIGN.md` §3.11.5) documents plugin metrics as `usage_collector.<backend>.*` (e.g. `usage_collector.clickhouse.*`). This plan uses `uc.timescaledb.*` per the user's decision and softens the plugin DESIGN's wording, but deliberately does **not** edit the parent gear DESIGN. Flag for the team whether the parent doc should gain a matching note about the `uc.` abbreviation.
- The other review findings (Important #6–12 and all Minor items) are intentionally **not** addressed here. No unrelated refactoring is performed.
