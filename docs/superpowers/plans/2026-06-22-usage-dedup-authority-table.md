# usage_dedup Authority Table Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix REVIEW.md A1 — the concurrent dedup TOCTOU race — by making a dedicated `usage_dedup` normal table the atomic 3-tuple uniqueness authority, with an in-DB cleanup job that keeps it in sync with hypertable retention.

**Architecture:** `create_inner` becomes a single transaction that claims a dedup slot (`INSERT … usage_dedup … ON CONFLICT (tenant_id, gts_id, idempotency_key) DO NOTHING RETURNING 1`) and only inserts the record if it won the slot; on conflict it resolves absorb-vs-conflict against the stored record, and returns retryable `Transient` for the (out-of-spec) case where the stored record has aged out. A TimescaleDB user-defined action (`add_job`) prunes orphaned dedup rows via an anti-join. Both retention registrations are made update-on-restart.

**Tech stack:** Rust, `sqlx` (Postgres), TimescaleDB 2.17.2-pg16, OpenTelemetry metrics, `testcontainers` for Docker-gated integration tests.

**Reference spec:** `docs/superpowers/specs/2026-06-22-usage-dedup-authority-table-design.md`

**Crate:** `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin`
(package `cf-gears-timescaledb-usage-collector-plugin`, lib `timescaledb_usage_collector_plugin`)

**Running tests:**
- Integration (needs Docker running): `cargo test -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test <file> <name> -- --nocapture`
- Unit (no Docker): `cargo test -p cf-gears-timescaledb-usage-collector-plugin --lib <name>`

All paths below are relative to the crate root unless noted.

---

## File Structure

| File | Responsibility | Change |
|------|----------------|--------|
| `migrations/0002_dedup_authority.sql` | `usage_dedup` table, its record_created_at index, `prune_usage_dedup` procedure | **create** |
| `src/infra/storage/record_store.rs` | transactional dual-write `create_inner`; remove dead `lookup_by_dedup_key` | modify |
| `src/infra/storage/pool.rs` | `apply_dedup_cleanup_job`; make `apply_retention_policy` update-on-restart | modify |
| `src/gear.rs` | call `apply_dedup_cleanup_job` in `init` | modify |
| `src/infra/metrics.rs` | `dedup_stale` counter + `inc_dedup_stale` | modify |
| `tests/common/mod.rs` | harness registers the cleanup job | modify |
| `tests/schema_integration_pg.rs` | assert table + procedure exist | modify |
| `tests/records_ingest_integration_pg.rs` | A1 regression, stale, batch tests | modify |
| `tests/cleanup_integration_pg.rs` | prune + retention-update tests | **create** |
| `DESIGN.md`, `README.md`, `REVIEW.md` | doc updates; mark A1 resolved | modify |

---

## Task 1: Migration — `usage_dedup` table + prune procedure

**Files:**
- Create: `migrations/0002_dedup_authority.sql`
- Test: `tests/schema_integration_pg.rs` (append)

- [ ] **Step 1: Write the failing test**

Append to `tests/schema_integration_pg.rs`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_migration_creates_usage_dedup_and_prune_procedure() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let table: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_name = 'usage_dedup')",
    )
    .fetch_one(&h.pool)
    .await
    .expect("usage_dedup existence query");
    assert!(table, "usage_dedup table must be created by migration 0002");

    let proc: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_proc WHERE proname = 'prune_usage_dedup')",
    )
    .fetch_one(&h.pool)
    .await
    .expect("prune_usage_dedup existence query");
    assert!(proc, "prune_usage_dedup procedure must be created by migration 0002");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test schema_integration_pg pg_migration_creates_usage_dedup -- --nocapture`
Expected: FAIL — `usage_dedup table must be created` (table/procedure do not exist yet).

- [ ] **Step 3: Write the migration**

Create `migrations/0002_dedup_authority.sql`:

```sql
-- Dedup authority: a NORMAL table (not a hypertable) so its unique key is the
-- true 3-tuple with no created_at. Its PK is the atomic serialization point
-- for concurrent same-key ingest (fixes the A1 TOCTOU race).
CREATE TABLE usage_dedup (
    tenant_id         uuid        NOT NULL,
    gts_id            text        NOT NULL,
    idempotency_key   text        NOT NULL,
    record_uuid       uuid        NOT NULL,
    record_created_at timestamptz NOT NULL,
    PRIMARY KEY (tenant_id, gts_id, idempotency_key)
);

-- Serves the cleanup anti-join's time bound.
CREATE INDEX usage_dedup_record_created_at_idx ON usage_dedup (record_created_at);

-- Cleanup procedure for the scheduled job (registered by apply_dedup_cleanup_job).
-- Deletes a dedup row older than retention ONLY when its pointed-to record no
-- longer exists, holding the invariant "dedup row exists iff its record exists".
CREATE PROCEDURE prune_usage_dedup(job_id int, config jsonb)
LANGUAGE plpgsql AS $$
DECLARE
    retention interval := make_interval(secs => (config->>'retention_secs')::float8);
BEGIN
    DELETE FROM usage_dedup d
     WHERE d.record_created_at < now() - retention
       AND NOT EXISTS (
           SELECT 1 FROM usage_records r
            WHERE r.uuid = d.record_uuid
              AND r.created_at = d.record_created_at);
END;
$$;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test schema_integration_pg pg_migration_creates_usage_dedup -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add migrations/0002_dedup_authority.sql tests/schema_integration_pg.rs
git commit -m "feat(usage-collector): add usage_dedup table + prune procedure (A1)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: Metrics — `dedup_stale` counter

**Files:**
- Modify: `src/infra/metrics.rs`
- Test: `src/infra/metrics_tests.rs`

- [ ] **Step 1: Add the failing test line**

In `src/infra/metrics_tests.rs`, inside `new_constructs_without_panic`, add after the `metrics.inc_dedup_absorbed();` line:

```rust
    metrics.inc_dedup_stale();
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --lib metrics`
Expected: FAIL — compile error `no method named inc_dedup_stale found`.

- [ ] **Step 3: Add the counter field**

In `src/infra/metrics.rs`, in the struct, after the `compensation: Counter<u64>,` field (the line preceded by `/// uc.timescaledb.compensation.count.`):

```rust
    /// `uc.timescaledb.dedup.stale.count`.
    dedup_stale: Counter<u64>,
```

- [ ] **Step 4: Build the counter**

In `Metrics::new`, after the `let compensation = meter…build();` block:

```rust
        let dedup_stale = meter
            .u64_counter("uc.timescaledb.dedup.stale.count")
            .with_description("Dedup hits whose stored record had aged out (retryable)")
            .build();
```

- [ ] **Step 5: Add to the struct initializer**

In the `Self { … }` initializer at the end of `new`, after `compensation,`:

```rust
            dedup_stale,
```

- [ ] **Step 6: Add the helper**

After the `inc_compensation` method:

```rust
    /// Increment the stale-dedup counter (dedup hit whose record had aged out).
    pub fn inc_dedup_stale(&self) {
        self.dedup_stale.add(1, &[]);
    }
```

- [ ] **Step 7: Run test to verify it passes**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --lib metrics`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/infra/metrics.rs src/infra/metrics_tests.rs
git commit -m "feat(usage-collector): add dedup.stale.count metric

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Rewrite `create_inner` to the transactional dual-write

**Files:**
- Modify: `src/infra/storage/record_store.rs` (rewrite `create_inner` ~lines 95-162; remove `lookup_by_dedup_key` ~lines 170-185)
- Test: `tests/records_ingest_integration_pg.rs` (append)

This task carries the headline A1 regression test. The existing absorb/conflict/new-record tests in this file must continue to pass after the rewrite.

- [ ] **Step 1: Write the failing A1 regression test**

Append to `tests/records_ingest_integration_pg.rs`:

```rust
/// A1 regression: two concurrent submissions sharing a dedup key but carrying
/// DIFFERENT created_at must resolve to exactly one stored record (one insert
/// wins, the other sees IdempotencyConflict) — never two rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pg_concurrent_same_key_different_created_at_inserts_one_row() {
    let (h, store) = setup().await;
    let tenant = Uuid::from_u128(0x1A1A);

    let rec_a =
        common::fixture_usage_record(VCPU_GTS, tenant, "idem-a1", Decimal::new(3, 0), 0xA1A);
    let mut rec_b =
        common::fixture_usage_record(VCPU_GTS, tenant, "idem-a1", Decimal::new(4, 0), 0xB1B);
    // Shift B's created_at by +1s: same dedup key, different 4-tuple — the exact
    // input the old app-guard could not serialize.
    rec_b.created_at = rec_a.created_at + time::Duration::seconds(1);

    let s1 = store.clone();
    let s2 = store.clone();
    let (r1, r2) = tokio::join!(
        tokio::spawn(async move { s1.create(rec_a).await }),
        tokio::spawn(async move { s2.create(rec_b).await }),
    );
    let r1 = r1.expect("task a join");
    let r2 = r2.expect("task b join");

    let oks = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
    let conflicts = [&r1, &r2]
        .iter()
        .filter(|r| matches!(r, Err(UsageCollectorPluginError::IdempotencyConflict { .. })))
        .count();
    assert_eq!(oks, 1, "exactly one insert wins: {r1:?} / {r2:?}");
    assert_eq!(conflicts, 1, "the loser sees IdempotencyConflict: {r1:?} / {r2:?}");

    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM usage_records \
         WHERE tenant_id = $1 AND gts_id = $2 AND idempotency_key = $3",
    )
    .bind(tenant)
    .bind(VCPU_GTS)
    .bind("idem-a1")
    .fetch_one(&h.pool)
    .await
    .expect("count rows for dedup key");
    assert_eq!(n, 1, "the dedup key maps to exactly one stored record");
}

/// A dedup row whose pointer references a record that no longer exists (the
/// record aged out before the prune job reclaimed the dedup row) yields a
/// retryable Transient, never a phantom absorb or a duplicate insert.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_dedup_row_pointing_at_missing_record_yields_transient() {
    let (h, store) = setup().await;
    let tenant = Uuid::from_u128(0x57A1E);

    let ghost = Uuid::from_u128(0xDEAD_BEEF);
    sqlx::query(
        "INSERT INTO usage_dedup \
         (tenant_id, gts_id, idempotency_key, record_uuid, record_created_at) \
         VALUES ($1, $2, $3, $4, now())",
    )
    .bind(tenant)
    .bind(VCPU_GTS)
    .bind("idem-stale")
    .bind(ghost)
    .execute(&h.pool)
    .await
    .expect("inject stale dedup row");

    let rec =
        common::fixture_usage_record(VCPU_GTS, tenant, "idem-stale", Decimal::new(9, 0), 0x5A1E);
    let err = store.create(rec).await.expect_err("stale dedup hit must error");
    assert!(
        matches!(err, UsageCollectorPluginError::Transient { .. }),
        "stale dedup hit returns retryable Transient, got {err:?}"
    );
}
```

- [ ] **Step 2: Run the A1 test to verify it fails**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test records_ingest_integration_pg pg_concurrent_same_key -- --nocapture`
Expected: FAIL — current code double-inserts, so `n == 2` and both calls return `Ok` (assertion on `oks`/row count fails).

- [ ] **Step 3: Rewrite `create_inner`**

In `src/infra/storage/record_store.rs`, replace the entire `create_inner` method body (the doc comment may stay; replace from `async fn create_inner` through its closing brace) with:

```rust
    async fn create_inner(
        &self,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        // 1. Claim the dedup slot. The usage_dedup 3-tuple PK is the atomic
        //    serialization authority: a concurrent same-key insert blocks on the
        //    row lock and resolves on commit. DO NOTHING + RETURNING 1
        //    distinguishes "won the slot" (Some) from "already held" (None).
        let won = sqlx::query_scalar::<_, i32>(
            "INSERT INTO usage_dedup \
             (tenant_id, gts_id, idempotency_key, record_uuid, record_created_at) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (tenant_id, gts_id, idempotency_key) DO NOTHING \
             RETURNING 1",
        )
        .bind(record.tenant_id)
        .bind(gts_id_str(&record.gts_id))
        .bind(record.idempotency_key.as_str())
        .bind(record.uuid)
        .bind(record.created_at)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| self.record_backend_error(&e))?;

        if won.is_some() {
            // 2a. Won the slot — insert the record. We own the dedup key, so the
            //     4-tuple unique cannot conflict; a missing RETURNING row is an
            //     invariant break.
            let insert_sql = format!(
                "INSERT INTO usage_records \
                 (uuid, tenant_id, gts_id, value, created_at, resource_id, resource_type, \
                  subject_id, subject_type, idempotency_key, corrects_id, metadata) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
                 RETURNING {RECORD_COLUMNS}"
            );
            let subject_id = record
                .subject_ref
                .as_ref()
                .map(usage_collector_sdk::SubjectRef::subject_id);
            let subject_type = record.subject_ref.as_ref().and_then(|s| s.subject_type());
            let metadata = metadata_map_to_jsonb(&record.metadata);
            let is_compensation = record.corrects_id.is_some();

            let inserted = sqlx::query_as::<_, UsageRecordRow>(&insert_sql)
                .bind(record.uuid)
                .bind(record.tenant_id)
                .bind(gts_id_str(&record.gts_id))
                .bind(record.value)
                .bind(record.created_at)
                .bind(record.resource_ref.resource_id())
                .bind(record.resource_ref.resource_type())
                .bind(subject_id)
                .bind(subject_type)
                .bind(record.idempotency_key.as_str())
                .bind(record.corrects_id)
                .bind(metadata)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| self.record_backend_error(&e))?;

            let row = inserted.ok_or_else(|| {
                UsageCollectorPluginError::internal(
                    "won the dedup slot but the record insert returned no row \
                     (concurrent-insert invariant break)",
                )
            })?;

            tx.commit()
                .await
                .map_err(|e| self.record_backend_error(&e))?;

            if is_compensation {
                self.metrics.inc_compensation();
            }
            return record_row_to_model(row);
        }

        // 2b. Slot already held — read the stored pointer, then the record, and
        //     resolve absorb-vs-conflict. The read path mutates nothing.
        let pointer = sqlx::query_as::<_, (Uuid, OffsetDateTime)>(
            "SELECT record_uuid, record_created_at FROM usage_dedup \
             WHERE tenant_id = $1 AND gts_id = $2 AND idempotency_key = $3",
        )
        .bind(record.tenant_id)
        .bind(gts_id_str(&record.gts_id))
        .bind(record.idempotency_key.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| self.record_backend_error(&e))?;

        let Some((existing_uuid, existing_created_at)) = pointer else {
            // The conflicting dedup row was deleted (cleanup) between our failed
            // insert and this read. Retryable: a retry re-claims the slot.
            tx.rollback().await.ok();
            return Err(UsageCollectorPluginError::transient(
                "dedup slot disappeared during conflict resolution; retry",
            ));
        };

        let select_sql = format!(
            "SELECT {RECORD_COLUMNS} FROM usage_records WHERE uuid = $1 AND created_at = $2"
        );
        let stored = sqlx::query_as::<_, UsageRecordRow>(&select_sql)
            .bind(existing_uuid)
            .bind(existing_created_at)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        tx.rollback().await.ok();

        match stored {
            Some(row) => self.resolve_dedup_hit(row, &record),
            None => {
                // Stale: the dedup row outlived its record (record chunk dropped
                // before the prune job reclaimed the dedup row). Reachable only by
                // replaying a key older than retention. Return retryable Transient
                // so a retry lands after cleanup (spec §4.1).
                self.metrics.inc_dedup_stale();
                Err(UsageCollectorPluginError::transient(
                    "dedup entry references an aged-out record; retry",
                ))
            }
        }
    }
```

- [ ] **Step 4: Remove the now-dead `lookup_by_dedup_key`**

Delete the entire `lookup_by_dedup_key` method (its doc comment + `async fn lookup_by_dedup_key…` through its closing brace). It is no longer referenced; leaving it triggers a dead-code warning.

- [ ] **Step 5: Run the new tests + the existing ingest suite**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test records_ingest_integration_pg -- --nocapture`
Expected: PASS — `pg_concurrent_same_key…` (one Ok + one conflict, n==1), `pg_dedup_row_pointing_at_missing_record…` (Transient), and all pre-existing tests (`pg_insert_new_record_returns_active`, `pg_exact_retry_is_absorbed`, the conflict and compensation tests) still pass.

- [ ] **Step 6: Verify the crate still builds clean (no dead-code warning)**

Run: `cargo build -p cf-gears-timescaledb-usage-collector-plugin --features postgres`
Expected: builds with no warnings about `lookup_by_dedup_key`.

- [ ] **Step 7: Commit**

```bash
git add src/infra/storage/record_store.rs tests/records_ingest_integration_pg.rs
git commit -m "fix(usage-collector): serialize ingest dedup via usage_dedup table (A1)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: Verify `create_batch` row independence under the new model

`create_batch` is unchanged (it loops over the now-transactional `create_inner`, one transaction per row). This task adds a test confirming per-row outcomes and input order survive.

**Files:**
- Test: `tests/records_ingest_integration_pg.rs` (append)

- [ ] **Step 1: Write the test**

Append to `tests/records_ingest_integration_pg.rs`:

```rust
/// A batch mixing a fresh insert, an exact retry (absorb), and a canonical
/// mismatch (conflict) on a pre-seeded key returns one positionally-aligned
/// result per row; a conflict is isolated to its own slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_batch_mixes_insert_absorb_conflict_per_row() {
    let (_h, store) = setup().await;
    let tenant = Uuid::from_u128(0xBA7C);

    let seed =
        common::fixture_usage_record(VCPU_GTS, tenant, "idem-dup", Decimal::new(5, 0), 0xB01);
    store.create(seed.clone()).await.expect("seed the dup key");

    let fresh =
        common::fixture_usage_record(VCPU_GTS, tenant, "idem-fresh", Decimal::new(2, 0), 0xB02);
    let absorb = seed.clone(); // exact retry of the seeded row
    let conflict =
        common::fixture_usage_record(VCPU_GTS, tenant, "idem-dup", Decimal::new(9, 0), 0xB03);

    let results = store
        .create_batch(vec![fresh, absorb, conflict])
        .await
        .expect("batch call succeeds");

    assert_eq!(results.len(), 3, "one result per input row, in order");
    assert!(results[0].is_ok(), "fresh row inserted: {:?}", results[0]);
    assert!(results[1].is_ok(), "exact retry absorbed: {:?}", results[1]);
    assert!(
        matches!(
            results[2],
            Err(UsageCollectorPluginError::IdempotencyConflict { .. })
        ),
        "canonical mismatch on seeded key conflicts: {:?}",
        results[2]
    );
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test records_ingest_integration_pg pg_batch_mixes -- --nocapture`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add tests/records_ingest_integration_pg.rs
git commit -m "test(usage-collector): batch per-row dedup outcomes under txn model

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: Cleanup job + update-on-restart retention registration

**Files:**
- Modify: `src/infra/storage/pool.rs` (add `apply_dedup_cleanup_job`; rewrite `apply_retention_policy`)
- Modify: `src/gear.rs` (call `apply_dedup_cleanup_job` in `init`)
- Modify: `tests/common/mod.rs` (harness registers the cleanup job)
- Create: `tests/cleanup_integration_pg.rs`

- [ ] **Step 1: Write the failing tests**

Create `tests/cleanup_integration_pg.rs`:

```rust
#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! TimescaleDB-backed tests for the dedup cleanup job and update-on-restart
//! retention registration. Requires Docker.

mod common;

use uuid::Uuid;

use timescaledb_usage_collector_plugin::infra::storage::pool::apply_dedup_cleanup_job;

const VCPU_GTS: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1";

/// The harness registers exactly one prune_usage_dedup job.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_dedup_cleanup_job_registered() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM timescaledb_information.jobs WHERE proc_name = 'prune_usage_dedup'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("jobs query");
    assert_eq!(n, 1, "exactly one dedup cleanup job must be registered");
}

/// Re-registering with a changed retention updates the existing job's config in
/// place (no duplicate job) — i.e. retention is live on restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_dedup_cleanup_job_updates_retention_on_reapply() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    apply_dedup_cleanup_job(&h.pool, 100)
        .await
        .expect("first re-apply");
    apply_dedup_cleanup_job(&h.pool, 200)
        .await
        .expect("second re-apply with changed retention");

    let cfg: serde_json::Value = sqlx::query_scalar(
        "SELECT config FROM timescaledb_information.jobs WHERE proc_name = 'prune_usage_dedup'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("config query");
    assert_eq!(
        cfg["retention_secs"],
        serde_json::json!(200),
        "changed retention must be applied on re-apply"
    );

    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM timescaledb_information.jobs WHERE proc_name = 'prune_usage_dedup'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("jobs count");
    assert_eq!(n, 1, "re-apply must not create a duplicate job");
}

/// prune_usage_dedup deletes an orphaned dedup row (record gone) older than
/// retention, keeps a live one (record present), and keeps a recent orphan.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_prune_removes_only_aged_orphans() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    // Satisfy the usage_records.gts_id FK.
    let catalog = common::catalog_store(&h.pool);
    catalog
        .create(common::fixture_usage_type(VCPU_GTS, "counter", &[]))
        .await
        .expect("register usage type");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x9C1E);

    // Live: a real record (created_at = fixture's 2023 instant) + its dedup row.
    let live =
        common::fixture_usage_record(VCPU_GTS, tenant, "idem-live", rust_decimal::Decimal::ONE, 0xC01);
    store.create(live).await.expect("create live record");

    // Aged orphan: dedup row pointing at a non-existent record, old timestamp.
    sqlx::query(
        "INSERT INTO usage_dedup \
         (tenant_id, gts_id, idempotency_key, record_uuid, record_created_at) \
         VALUES ($1, $2, 'idem-orphan', $3, TIMESTAMPTZ '2023-01-01 00:00:00+00')",
    )
    .bind(tenant)
    .bind(VCPU_GTS)
    .bind(Uuid::from_u128(0xDEAD01))
    .execute(&h.pool)
    .await
    .expect("insert aged orphan");

    // Recent orphan: same, but record_created_at = now() (inside retention).
    sqlx::query(
        "INSERT INTO usage_dedup \
         (tenant_id, gts_id, idempotency_key, record_uuid, record_created_at) \
         VALUES ($1, $2, 'idem-recent', $3, now())",
    )
    .bind(tenant)
    .bind(VCPU_GTS)
    .bind(Uuid::from_u128(0xDEAD02))
    .execute(&h.pool)
    .await
    .expect("insert recent orphan");

    // Run the prune with a 1-day retention: 2023 rows are candidates, now() is not.
    sqlx::query("CALL prune_usage_dedup(0, $1)")
        .bind(serde_json::json!({ "retention_secs": 86400 }))
        .execute(&h.pool)
        .await
        .expect("call prune procedure");

    let key_exists = |idem: &'static str| {
        let pool = h.pool.clone();
        let tenant = tenant;
        async move {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM usage_dedup \
                 WHERE tenant_id = $1 AND gts_id = $2 AND idempotency_key = $3)",
            )
            .bind(tenant)
            .bind(VCPU_GTS)
            .bind(idem)
            .fetch_one(&pool)
            .await
            .expect("existence query")
        }
    };

    assert!(key_exists("idem-live").await, "live dedup row (record present) kept");
    assert!(!key_exists("idem-orphan").await, "aged orphan (record gone) deleted");
    assert!(key_exists("idem-recent").await, "recent orphan (inside retention) kept");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test cleanup_integration_pg -- --nocapture`
Expected: FAIL to compile — `apply_dedup_cleanup_job` does not exist; and (once it compiles) `pg_dedup_cleanup_job_registered` fails because the harness does not register it.

- [ ] **Step 3: Add `apply_dedup_cleanup_job` and fix `apply_retention_policy`**

In `src/infra/storage/pool.rs`, replace the existing `apply_retention_policy` function with:

```rust
/// Idempotently register the config-driven retention policy, **updating** it if
/// it already exists so a changed `retention_secs` takes effect on restart.
/// Runs after migrations.
///
/// `add_retention_policy(if_not_exists => TRUE)` would *skip* an existing
/// policy and silently keep the old window; remove-then-add applies the new
/// one. The sub-second gap with no policy is harmless — retention is a slow
/// background job.
///
/// # Errors
/// Returns `sqlx::Error` if either statement fails.
pub async fn apply_retention_policy(pool: &PgPool, retention_secs: u64) -> Result<(), sqlx::Error> {
    let secs = i64::try_from(retention_secs).unwrap_or(i64::MAX);
    sqlx::query("SELECT remove_retention_policy('usage_records', if_exists => TRUE)")
        .execute(pool)
        .await?;
    sqlx::query(
        "SELECT add_retention_policy('usage_records', \
         drop_after => make_interval(secs => $1::double precision))",
    )
    .bind(secs)
    .execute(pool)
    .await?;
    Ok(())
}

/// Idempotently register (or update) the `usage_dedup` cleanup job. Runs after
/// migrations, mirroring [`apply_retention_policy`]. The job calls the
/// `prune_usage_dedup` procedure (migration 0002), which deletes dedup rows
/// whose record has aged out. Update-then-create so a changed retention is
/// applied on restart and exactly one job exists.
///
/// # Errors
/// Returns `sqlx::Error` if either statement fails.
pub async fn apply_dedup_cleanup_job(pool: &PgPool, retention_secs: u64) -> Result<(), sqlx::Error> {
    let secs = i64::try_from(retention_secs).unwrap_or(i64::MAX);
    // Update the existing job's config if present.
    sqlx::query(
        "SELECT alter_job(j.job_id, schedule_interval => INTERVAL '1 day', \
                config => jsonb_build_object('retention_secs', $1::bigint)) \
         FROM timescaledb_information.jobs j WHERE j.proc_name = 'prune_usage_dedup'",
    )
    .bind(secs)
    .execute(pool)
    .await?;
    // Create it if it was not there.
    sqlx::query(
        "SELECT add_job('prune_usage_dedup', INTERVAL '1 day', \
                config => jsonb_build_object('retention_secs', $1::bigint), if_not_exists => TRUE)",
    )
    .bind(secs)
    .execute(pool)
    .await?;
    Ok(())
}
```

- [ ] **Step 4: Wire it into `init`**

In `src/gear.rs`, update the import on the `use crate::infra::storage::pool::…` line to add `apply_dedup_cleanup_job`:

```rust
use crate::infra::storage::pool::{MIGRATOR, apply_dedup_cleanup_job, apply_retention_policy, build_pool};
```

Then, immediately after the existing `apply_retention_policy(&pool, cfg.retention_period_secs).await?;` line:

```rust
        apply_dedup_cleanup_job(&pool, cfg.retention_period_secs).await?;
```

- [ ] **Step 5: Register the job in the test harness**

In `tests/common/mod.rs`, update the import:

```rust
use timescaledb_usage_collector_plugin::infra::storage::pool::{
    MIGRATOR, apply_dedup_cleanup_job, apply_retention_policy, build_pool,
};
```

Then, after the existing `apply_retention_policy(&pool, cfg.retention_period_secs).await?;` line in `bring_up`:

```rust
    apply_dedup_cleanup_job(&pool, cfg.retention_period_secs).await?;
```

- [ ] **Step 6: Run the cleanup tests + a schema regression check**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test cleanup_integration_pg -- --nocapture`
Expected: PASS — job registered (exactly one), retention updates on re-apply, prune removes only aged orphans.

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test schema_integration_pg -- --nocapture`
Expected: PASS — `pg_migrations_create_hypertable_and_retention` still passes (retention policy still registered after the remove-then-add change).

- [ ] **Step 7: Commit**

```bash
git add src/infra/storage/pool.rs src/gear.rs tests/common/mod.rs tests/cleanup_integration_pg.rs
git commit -m "feat(usage-collector): dedup cleanup job + update-on-restart retention

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: Documentation

**Files:**
- Modify: `DESIGN.md` (at `gears/system/usage-collector/docs/timescaledb-usage-collector-storage-plugin/DESIGN.md`)
- Modify: `README.md` (crate root)
- Modify: `REVIEW.md` (crate root)

- [ ] **Step 1: Update the README dedup section**

In `README.md`, replace the `**Deduplication**` bullet with:

```markdown
- **Deduplication** — a dedicated `usage_dedup` table (normal table, `PRIMARY KEY (tenant_id, gts_id, idempotency_key)`) is the atomic 3-tuple uniqueness authority. Ingest runs in one transaction: it claims the dedup slot (`INSERT … ON CONFLICT DO NOTHING`) and inserts the record only if it won the slot; a concurrent same-key submission blocks on the slot's row lock and then resolves as a silent absorb (canonical-equal) or an `IdempotencyConflict`. This serializes the same-key/different-`created_at` case the hypertable's `UNIQUE (…, created_at)` constraint structurally cannot. A TimescaleDB cleanup job (`prune_usage_dedup`) reclaims a dedup row once its record's chunk has been dropped, keeping the table in step with retention.
```

- [ ] **Step 2: Update DESIGN.md**

In `gears/system/usage-collector/docs/timescaledb-usage-collector-storage-plugin/DESIGN.md`:

- In the **§3.6 "Ingest with idempotency dedup"** description (the line after the mermaid block, currently "the unique constraint plus a canonical-field comparison decides insert vs absorb vs conflict."), replace with:

```markdown
**Description**: a dedicated `usage_dedup` table whose `(tenant_id, gts_id, idempotency_key)` primary key is the atomic serialization authority decides insert vs absorb vs conflict. Ingest claims the slot in a transaction and inserts the record only on winning it; the loser of a concurrent same-key race blocks on the slot's row lock and resolves absorb-vs-conflict against the stored record. A retryable `Transient` is returned in the (retention-bounded, out-of-spec) window where the stored record has aged out but its dedup row is not yet reclaimed.
```

- In **§2.2 (Retention-Bounded Dedup-Key Preservation)** and **§3.7 (retention)**, add a sentence noting that dedup keys now live in `usage_dedup` and are reclaimed by the `prune_usage_dedup` job, which deletes a dedup row only once its record no longer exists (anti-join), so the key-preservation window still tracks `retention_period`.

- [ ] **Step 3: Mark A1 resolved in REVIEW.md**

In `REVIEW.md`, in the Architecture table A1 row, prepend `**RESOLVED** — ` to the Issue cell and append to the Fix cell:

```markdown
Implemented: `usage_dedup` authority table + transactional dual-write + `prune_usage_dedup` cleanup job (see `docs/superpowers/specs/2026-06-22-usage-dedup-authority-table-design.md`).
```

Also update the **Summary** line: change "1 HIGH (architecture)" context to note A1 is resolved.

- [ ] **Step 4: Commit**

```bash
git add README.md REVIEW.md "../../docs/timescaledb-usage-collector-storage-plugin/DESIGN.md"
git commit -m "docs(usage-collector): document usage_dedup authority + mark A1 resolved

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

(Adjust the DESIGN.md path to the repo-relative path if committing from the repo root: `gears/system/usage-collector/docs/timescaledb-usage-collector-storage-plugin/DESIGN.md`.)

---

## Final verification

- [ ] **Run the whole crate test suite (Docker required):**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --features postgres -- --nocapture`
Expected: all integration + unit tests pass, including the A1 regression, stale-hit, batch, cleanup, and schema tests.

- [ ] **Lint:**

Run: `cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --features postgres --all-targets`
Expected: no warnings (in particular, no dead-code warning for the removed `lookup_by_dedup_key`).

---

## Self-review notes (author checklist — done at plan time)

- **Spec coverage:** §3 schema → Task 1; §4/§4.1 write path + stale → Task 3; §5/§5.1 cleanup job + update-on-restart → Task 5; §6 batch → Task 4; §7 metric → Task 2; §8 tests → Tasks 1,3,4,5; §9 docs → Task 6. All spec sections mapped.
- **Type/name consistency:** `apply_dedup_cleanup_job`, `inc_dedup_stale`, `prune_usage_dedup`, `usage_dedup`, `UsageCollectorPluginError::transient(...)`, `IdempotencyConflict { idempotency_key, existing_uuid }`, `record_row_to_model`, `resolve_dedup_hit`, `RECORD_COLUMNS` used consistently across tasks and match the existing code.
- **Out of scope (unchanged):** REVIEW.md C1–C8 / T1–T3; no backfill (greenfield); no inline stale-hit repoint.
