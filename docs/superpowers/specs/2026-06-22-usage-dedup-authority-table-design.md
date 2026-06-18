# Design: `usage_dedup` authority table (fixes A1 — concurrent dedup race)

**Date:** 2026-06-22
**Crate:** `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin`
**Status:** Approved design, pre-implementation
**Fixes:** REVIEW.md **A1** (HIGH) — TOCTOU dedup race under concurrency

---

## 1. Problem

The ingest dedup key is the 3-tuple `(tenant_id, gts_id, idempotency_key)`, but the only
DB-enforceable unique constraint on the `usage_records` hypertable is the 4-tuple
`(tenant_id, gts_id, idempotency_key, created_at)` — the partition column `created_at`
*must* be part of any unique key on a hypertable. The 3-tuple guarantee is therefore
enforced only by an application guard (`lookup_by_dedup_key` then `INSERT … ON CONFLICT
(4-tuple) DO NOTHING`) in `create_inner`
([record_store.rs:95-162](../../../gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/record_store.rs)),
which is **not serialized**.

**The race:** two concurrent calls with the same dedup key but *different* `created_at`
both miss the lookup and both insert; the 4-tuple differs, so neither conflicts → two rows
share one dedup key. Result on a SUM-based billing aggregate: silent double-counting.
(A same-`created_at` concurrent retry is *not* affected — the 4-tuple constraint catches it.)

## 2. Decision

Replace the all-chunk app-guard lookup with a dedicated **`usage_dedup` authority table**:
a *normal* (non-hypertable) table whose primary key is the true 3-tuple. Its unique
constraint becomes the atomic serialization point — no advisory lock needed — and the
ingest path dual-writes the dedup row and the record in one transaction.

### Why option 2 over an advisory lock (option 1)

A micro-benchmark (TimescaleDB 2.17.2-pg16, 1-day chunks, pgbench 4 clients) measured both:

| Chunks | Records | Opt1 lookup | Opt1 tps | Opt2 probe | Opt2 tps |
|--:|--:|--:|--:|--:|--:|
| 1 | 5k | 0.022 ms | 2150 | 0.025 ms | **3702** |
| 50 | 250k | 0.563 ms | 1283 | 0.031 ms | **3705** |
| 200 | 1M | 2.428 ms | 301 | 0.038 ms | **3205** |
| 500 | 2.5M | 7.905 ms | 124 | 0.045 ms | **3077** |

Option 1's dedup lookup is an `Append` over **every chunk's** local index (no `created_at`
predicate ⇒ no chunk exclusion), so its cost grows linearly with chunk count and tps
collapses ~17× from 1→500 chunks. Option 2's probe is a single compact-table index hit —
flat and faster at every operating point. At the default 7-day-chunk / 365-day-retention
operating point (~52 chunks) option 2 is ~2.9× the throughput; for high-volume / long-retention
configs the gap widens to 10–25×, and it *grows* as the table ages — the wrong direction for
a billing ingest path.

## 3. Schema — migration `0002_dedup_authority.sql`

Greenfield: the plugin is pre-production, so this only creates the empty table + procedure.
No backfill of existing `usage_records` rows.

```sql
-- Normal table (NOT a hypertable) so the unique key needs no created_at.
CREATE TABLE usage_dedup (
    tenant_id         uuid        NOT NULL,
    gts_id            text        NOT NULL,
    idempotency_key   text        NOT NULL,
    record_uuid       uuid        NOT NULL,   -- pointer to the authoritative record
    record_created_at timestamptz NOT NULL,   -- enables a chunk-pruned PK lookup of the record
    PRIMARY KEY (tenant_id, gts_id, idempotency_key)
);

-- Serves the cleanup anti-join (§5).
CREATE INDEX usage_dedup_record_created_at_idx ON usage_dedup (record_created_at);

-- Cleanup procedure invoked by the scheduled job (§5).
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
END $$;
```

Notes:
- **No FK** from `usage_dedup` to `usage_records`: you cannot cleanly FK into a hypertable,
  and chunk-drop would not cascade. The cleanup job (§5) maintains the relationship.
- The table is deliberately **narrow** (key + pointer, no canonical fields). That narrowness
  is what keeps the probe flat/fast — the whole point of the change. The rare conflict path
  fetches the record by `(record_uuid, record_created_at)`, a chunk-pruned PK lookup.

## 4. Ingest write path — rewrite of `create_inner`

The `usage_dedup` unique constraint is the serialization authority: a concurrent same-key
insert blocks on the first transaction's row lock and resolves correctly on commit. **No
advisory lock.** Dedup row + record are written in one transaction so a dedup row can never
exist without its record.

```
BEGIN
  INSERT INTO usage_dedup (tenant_id, gts_id, idempotency_key, record_uuid, record_created_at)
    VALUES (key…, <new uuid>, <new created_at>)
    ON CONFLICT (tenant_id, gts_id, idempotency_key) DO NOTHING
    RETURNING 1;

  if inserted (won the slot):
      INSERT INTO usage_records (…);              -- the fresh record
      COMMIT
      inc_compensation() if corrects_id.is_some()
      return record

  else (slot already held):
      SELECT record_uuid, record_created_at FROM usage_dedup WHERE key…;   -- existing pointer
      SELECT <RECORD_COLUMNS> FROM usage_records WHERE uuid = ? AND created_at = ?;  -- PK, chunk-pruned
      match fetched record:
          Some(row) if canonical_equal(row, incoming) -> inc_dedup_absorbed(); COMMIT; return stored row
          Some(row)                                   -> inc_idempotency_conflict(); ROLLBACK;
                                                         return IdempotencyConflict { existing_uuid: row.uuid }
          None (stale — record aged out; see §4.1)    -> return Transient { detail, retry_after_seconds: None }
COMMIT
```

Existing helpers `canonical_equal` and `resolve_dedup_hit`
([record_store.rs:187-208,288-303](../../../gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/record_store.rs))
are reused unchanged for the `Some(row)` arms. Happy path = two statements in one
transaction (the exact shape benchmarked at ~3100-3700 tps, flat).

### 4.1 Stale-hit handling (decided: `Transient`)

Because record chunk-drop and the dedup prune job run on independent schedules, there is a
small window in which a record is gone but its dedup row is not yet reclaimed. A new ingest
for that key would conflict on the dedup row, then fetch `None` for the record. This is only
reachable by **replaying an idempotency key whose original record is older than retention
(365+ days)** inside the prune-job interval — already out of spec per the
retention-bounded-dedup constraint.

**Behavior:** return `UsageCollectorPluginError::Transient` (one match arm; identical code
cost to returning `IdempotencyConflict`). `Transient` is retryable, so the client retries,
the prune job clears the stale row, and the retry inserts fresh — no data loss. We
deliberately do **not** implement inline "repoint the slot" handling: it would reintroduce a
concurrent-double-insert hazard (two simultaneous stale replays) into the exact path this
change is simplifying.

## 5. Cleanup job — in-DB, registered at init

Cleanup is an in-database TimescaleDB user-defined action (`add_job`), consistent with the
design principle that retention/expiry is a backend job and the plugin issues no row-level
deletes itself and runs no background loop. Registered in `gear.rs::init` right after
`apply_retention_policy` ([gear.rs:52](../../../gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/gear.rs)).

**Predicate (decided: anti-join).** `prune_usage_dedup` (§3) deletes a dedup row older than
retention **only when its pointed-to record no longer exists** (`NOT EXISTS`). This holds the
exact invariant "a dedup row exists iff its record exists," so the stale window is just the
job interval. The time bound keeps the anti-join's candidate set small (rows crossing the
retention boundary since the last run).

**`retention_secs`** is the same `cfg.retention_period_secs` passed to `apply_retention_policy`,
so the dedup cleanup and the record retention are aligned by construction.

### 5.1 Config updates on restart (fix `if_not_exists` non-update)

Both retention registrations must apply a **changed** `retention_period_secs` on restart.
The current `apply_retention_policy` uses `add_retention_policy(… if_not_exists => TRUE)`
([pool.rs:33](../../../gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/pool.rs)),
which **skips** an existing policy rather than updating it — a pre-existing latent bug: a
changed retention is silently ignored after first init. **Both** the existing record
retention policy and the new dedup cleanup job are fixed to update-on-restart.

- **New** `apply_dedup_cleanup_job(pool, retention_secs)` in `pool.rs`, called from `init`:
  update-if-exists then create-if-missing, so there is exactly one job whose config always
  reflects current config:
  ```sql
  SELECT alter_job(j.job_id, schedule_interval => INTERVAL '1 day',
                   config => jsonb_build_object('retention_secs', $1))
    FROM timescaledb_information.jobs j WHERE j.proc_name = 'prune_usage_dedup';
  SELECT add_job('prune_usage_dedup', INTERVAL '1 day',
                 config => jsonb_build_object('retention_secs', $1), if_not_exists => TRUE);
  ```
  (Explicit catalog lookup rather than relying on `add_job`'s `if_not_exists` matching.)
- **Fix** `apply_retention_policy` so a changed `retention_period_secs` is applied on
  restart: `remove_retention_policy('usage_records', if_exists => TRUE)` then
  `add_retention_policy('usage_records', drop_after => make_interval(secs => $1::float8))`.
  The sub-second gap between remove and add (both run in `init`) is harmless — retention is
  a slow background job. (Chosen over alter-in-place for simplicity; both are correct.)

## 6. `create_batch`

Unchanged in shape: a per-row loop over the now-transactional `create_inner`
([record_store.rs:318-347](../../../gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/record_store.rs)).
Per-row independent transactions preserve today's exact semantics (per-row results,
input order, one conflict does not poison the batch) and avoid cross-key deadlocks. No
batch-level atomicity change.

## 7. Observability

- Existing per-row counters (`dedup.absorbed`, `idempotency.conflict`, `compensation`,
  backend-error class) are preserved and increment in the same logical places.
- **New** counter `dedup.stale.count` for §4.1 hits (bounded label set, per the metrics
  philosophy in DESIGN §3.8 — no unbounded identifiers as labels).
- Cleanup-job observability is via TimescaleDB's `timescaledb_information.job_stats` /
  `job_errors` views (it is an in-DB job), not an application metric.

## 8. Testing

Docker-gated integration suite (`testcontainers`, existing harness in `tests/common/mod.rs`),
plus fast unit tests where possible.

- **A1 regression (headline):** fire two concurrent `create` calls with the same dedup key
  and **different** `created_at`; assert exactly **one** `usage_records` row and exactly one
  `IdempotencyConflict` (or one absorb for the equal-canonical case). This is the test that
  would have caught A1.
- Exact-retry absorb still returns the stored record (`dedup.absorbed`).
- Canonical-field mismatch still returns `IdempotencyConflict`.
- Compensation row (own idempotency key, `corrects_id` set) inserts and counts.
- `create_batch` row independence and input-order preserved; one conflict does not poison
  the batch.
- `prune_usage_dedup`: dedup row whose record is gone is deleted; dedup row whose record
  exists is kept; row newer than retention is kept.
- Stale-hit: a dedup row pointing at a non-existent record yields `Transient`.
- `apply_dedup_cleanup_job` / fixed `apply_retention_policy`: idempotent re-run; a changed
  `retention_secs` is reflected in the job/policy config after re-init.

## 9. Docs to update

- Plugin `DESIGN.md` §2.2 (Retention-Bounded Dedup-Key Preservation), §3.6 (ingest-dedup
  sequence), §3.7 (retention): describe the authority table + anti-join cleanup job.
- Plugin `README.md` dedup section.
- `REVIEW.md`: mark **A1 resolved**.

## 10. Out of scope

- Backfill migration (greenfield — no existing data).
- Inline stale-hit "repoint" handling (§4.1 — deliberately rejected).
- Option 1 (advisory lock) — rejected on the benchmark evidence in §2.
- Other REVIEW.md findings (C1–C8, T1–T3) — tracked separately.

## File-change summary

| File | Change |
|------|--------|
| `migrations/0002_dedup_authority.sql` | **new** — `usage_dedup` table, index, `prune_usage_dedup` procedure |
| `src/infra/storage/record_store.rs` | rewrite `create_inner` (txn dual-write); drop `lookup_by_dedup_key` all-chunk scan |
| `src/infra/storage/pool.rs` | **new** `apply_dedup_cleanup_job`; fix `apply_retention_policy` to update-on-restart |
| `src/gear.rs` | call `apply_dedup_cleanup_job` in `init` after `apply_retention_policy` |
| `src/infra/metrics.rs` | **new** `dedup.stale.count` counter + recorder |
| `tests/records_ingest_integration_pg.rs` (+ harness) | A1 regression + dedup/compensation/stale/prune tests |
| `DESIGN.md`, `README.md`, `REVIEW.md` | doc updates; mark A1 resolved |
