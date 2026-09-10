# TimescaleDB Storage Plugin Port Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port `cf-gears-timescaledb-usage-collector-plugin` onto the SPI that slices 1-6 reshaped, until `usage_collector_sdk::contract::run_all` reports no violations against it and it is a workspace member again.

**Architecture:** The crate keeps its DDD-light layering — `gear` (GTS handshake) → `domain` (SPI adapter + store port) → `infra` (sqlx/Postgres). Three things change shape underneath it: the usage-type catalog is deleted outright (typing moved to `types-registry` in slice 2), the ledger table is replaced by a fresh schema carrying the covered period, the append-only invalidation pair, `origin`, and a plugin-assigned `acceptance_sequence`, and the query plane is rebuilt around `(window_end, id)` keyset pagination over the published eight-field `$filter` surface.

**Tech Stack:** Rust 2024, `sqlx` (Postgres + TimescaleDB), `toolkit-odata` (filter AST, cursors, keyset), `rust_decimal` / `bigdecimal`, `time`, `tokio`, `cf-gears-usage-collector-sdk` (SPI + contract suite).

---

## Ground rules — read before Task 1

These are inherited from slices 4-6 and each one was paid for. They are not
optional and they are not restated per task.

- **Name the mutation, or it is not a test.** Before accepting any test, state
  the one-token edit to production code that makes it red. If you cannot name
  one, it is not a test. Tautological length assertions over fixed-size arrays,
  `std::any::type_name` checks, and fixtures that are green under the old
  behaviour too have all shipped here before.
- **Do not run a workspace-wide test build.** `target/` reaches ~110 GB and
  fills the disk. Scope every run with `-p`. **This bites harder now**: Task 16
  adds this crate to the workspace, so `--workspace` grows.
- **Tests live in a sibling `*_tests.rs` file** with a
  `#[cfg(test)] #[cfg_attr(coverage_nightly, coverage(off))] #[path = "..."]`
  hook, never an inline `mod tests`.
- **Commits are Conventional Commits with a `Signed-off-by` trailer.** A
  breaking change takes a `!` and a `BREAKING CHANGE:` trailer. **No
  attribution lines.**
- **Clippy is deny-warnings in CI and `clippy::pedantic` is deny at workspace
  level.** `clippy::non_ascii_literal` means no em dashes inside string
  literals; doc comments are fine.
- **Cite ADRs by `cpt-cf-usage-collector-adr-*` id, never by number.**
- **Verify any `§N.M` DESIGN citation falls inside that section's line span.**
  Measured spans: §3.1 = 476-592, §3.2 = 593-866, §3.3 = 867-1245,
  §3.7 = 1526-1552, §3.10 = 1637-1721, §3.11 = 1722-1844
  (§3.11.5 = 1767-1824).
- **Never `git add -A`.** Six files in this tree are uncommitted and are not
  yours to commit or restore: the deleted
  `docs/superpowers/plans/2026-09-07-usage-collector-record-model-handoff.md`,
  and untracked `NEXT-SLICE.md`, `SLICE4.md`, `SLICE5.md`, `SLICE6.md`,
  `SLICE7.md`.
- **`grep -c` returning 0 exits non-zero** and will silently truncate a `&&`
  chain.
- **`timeout` does not exist in this shell.** `timeout 30 docker info` fails
  with "command not found", so `timeout … && echo up || echo down` reports
  *down* regardless of the daemon's actual state. This produced a wrong
  conclusion in this session — Docker was reported unavailable twice while it
  was running. **Run the command bare, and when a probe reports "unavailable",
  check the probe before believing it.** The general form of this trap: a
  wrapper that fails for its own reasons is indistinguishable, through `&&`/`||`,
  from the thing it wraps failing.
- **Docker is available** (server 28.3.3), so Tasks 3, 14, 15 and 17 can be
  verified for real. Clean up containers you start, including on failure.
- **Never post-filter `grep -rn` on digit patterns** — it matches grep's own
  line numbers and drops every hit on a line >= 10.
- **A claim outliving the code is the characteristic defect here.** A retracted
  claim that is reworded rather than replaced ships anyway. The specific shape
  that survives review: a conclusion that is correct resting on a supporting
  mechanism that was invented. Nothing in a test suite sees it.
- **Rustdoc drops `//` inside a `///` block.** A retraction written that way is
  not rendered.

### Decisions already made — do not relitigate

Four questions were put to the owner before this plan was written. The answers
are settled inputs:

1. **`acceptance_sequence` is plugin-only.** It becomes a stored column this
   backend assigns and orders on. It does **not** go on the SDK `UsageRecord`.
   `latest-tie-break` therefore stays in `BLOCKED_CHECKS` and entry 19 stands
   unchanged. Do not add either field to the SDK.
2. **Fresh schema.** `0001_init.sql` and `0002_rename_uuid_to_id.sql` are
   replaced by a single new init. No migration path is owed — migration 0002's
   own comment records that the gear is unreleased and any rows at cutover live
   on disposable dev databases.
3. **Both CI steps are restored** (Task 17), and the e2e python is brought
   current with them.
4. **`docs/api/api.json` is regenerated at the end of this slice** (Task 18).

### Verification bar

Run after every task that touches Rust:

```bash
cargo check --workspace --all-targets
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk \
  -p cf-gears-noop-usage-collector-plugin --no-fail-fast
cargo nextest run -p cf-gears-usage-collector-sdk --features contract --no-fail-fast
cargo clippy --workspace --all-targets --all-features
cargo +nightly fmt
cargo doc --no-deps -p cf-gears-usage-collector-sdk -p cf-gears-usage-collector
```

**Baseline measured at `863a1396e`, the commit this slice starts from:**

| Measure | Value |
| --- | --- |
| `cargo nextest` across the three packages | **716 passed / 0 skipped** |
| `cargo nextest -p …-sdk --features contract` | **166 passed / 0 skipped** |
| `cargo doc` warnings, `cf-gears-usage-collector` | **35** |
| `cargo doc` warnings, `cf-gears-usage-collector-sdk` | **0** |

Skipped must stay 0. Neither doc-warning count may grow — and an intra-doc link
from a public doc to a `pub(crate)` item adds one. Read the `generated N
warnings` line; `grep -c '^warning:'` over-counts by one because rustdoc's own
summary line begins with `warning:`.

From Task 16 onward the nextest run gains
`-p cf-gears-timescaledb-usage-collector-plugin`. Its Postgres integration
tests are behind the `postgres` feature and need a reachable Docker daemon.

---

## File Structure

### Deleted outright

| Path | Lines | Why |
| --- | --- | --- |
| `src/infra/storage/catalog_store.rs` | 501 | Slice 2 removed the usage-type catalog from the gear. `types-registry` owns typing; the SPI never sees a declaration. |
| `src/infra/storage/catalog_store_tests.rs` | 164 | Tests of the above. |
| `tests/catalog_integration_pg.rs` | 212 | Integration tests of the above. |

### Rewritten

| Path | Responsibility after the port |
| --- | --- |
| `migrations/0001_init.sql` | The whole schema, replacing both existing migrations. |
| `src/infra/storage/entity.rs` | `UsageRecordRow` only, mirroring the new columns. `UsageTypeRow` is deleted. |
| `src/infra/storage/mapper.rs` | Row <-> `UsageRecord`. Loses status/kind/catalog helpers; gains the invalidation pair, `origin`, and the covered period. |
| `src/infra/storage/query/translate.rs` | `record_column` brought to the model that exists (DIVERGENCES entry 16). `usage_type_column` deleted. |
| `src/infra/storage/query/keyset.rs` | `(window_end, id)` canonical keyset; honours `query.order` as given; carries `filter_hash` into `next_cursor.f`. |
| `src/infra/storage/query/aggregate.rs` | `AggregationFold` (not `AggregationOp`); the two withdrawal-exclusion clauses; `LATEST` via the declared tie-break. |
| `src/infra/storage/record_store.rs` | The five SPI operations. Gains `acceptance_sequence` assignment and the atomic at-most-one-invalidation rule; loses `deactivate`. |
| `src/domain/ports.rs` | `RecordStore` only, with the new signatures. `CatalogStore` deleted. |
| `src/domain/adapter.rs` | Five methods, down from ten. |
| `src/gear.rs` | Drops the catalog store from the wiring. |

### Created

| Path | Responsibility |
| --- | --- |
| `tests/contract_conformance_pg.rs` | The acceptance criterion: `run_all` against a live TimescaleDB backend. |

---

## Task 0: Make the crate buildable at all — DONE before Task 1

**This was done by the controller before Task 1 was dispatched. It is recorded
here because it changes every verification command in this plan.**

The plugin could not be compiled *in any form* at the start of this slice.
`cargo check --manifest-path …/Cargo.toml` does not fall back to a standalone
build — cargo refuses outright:

```
error: current package believes it's in a workspace when it's not:
current:   …/plugins/timescaledb-usage-collector-plugin/Cargo.toml
workspace: /Users/binarycode/code/virtuozzo/gears-rust/Cargo.toml
```

An empty `[workspace]` table in the plugin manifest is not a way around it
either: that makes the crate its own workspace root, and every
`{ workspace = true }` dependency it declares stops resolving.

So workspace membership — originally Task 16 Step 1 — was moved to the front.
The single line added to the root `Cargo.toml` `members` array:

```toml
    "gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin",
```

**The consequence, and it is not optional: `cargo check --workspace` is RED
from here until Task 13.** That is the whole point of Task 13, and it was
always going to be true; what changed is that it is now visible in the
workspace build rather than hidden behind a crate cargo would not look at.

**Intermediate verification bar, Tasks 1-12.** Use this instead of the
full bar at the top of this file:

```bash
# The crate under construction — expect errors, and expect them to shrink.
cargo check -p cf-gears-timescaledb-usage-collector-plugin --all-targets 2>&1 | tail -40

# The three packages that must stay green throughout. These are unaffected by
# the plugin and a regression here means you broke something outside your task.
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk \
  -p cf-gears-noop-usage-collector-plugin --no-fail-fast
cargo nextest run -p cf-gears-usage-collector-sdk --features contract --no-fail-fast
```

The full bar — `--workspace` check, clippy, fmt, doc — resumes at Task 13,
which is the task whose success criterion is that it passes.

**Measured baseline immediately after adding the member line, at `490d42c8e`:**

**Read cargo's own summary lines. Do NOT grep-count diagnostics.**

```bash
cargo check -p cf-gears-timescaledb-usage-collector-plugin --all-targets 2>&1 \
  | grep 'previous error'
```

| Target | Errors |
| --- | --- |
| lib | **44** |
| lib test | **63** |

**Two ways a grep count lies here, both hit during Task 1.**

1. **`grep -c '^error\[\|^error:'` over-counts by exactly 2**, because cargo's
   two `error: could not compile … due to N previous errors` summary lines
   themselves begin with `error:`. This is the same trap this plan's ground
   rules already record for `cargo doc`'s `generated N warnings` line, made a
   second time against a different tool. An earlier draft of this file reported
   the baseline as "74 total" on that basis; the real figure was 44 + 63.
2. **`grep -c '^error\['` is not stable across runs.** Cargo does not re-emit
   every diagnostic on a cached rebuild, so two people at the same commit get
   different numbers — 59 and 56 were both measured at `ba285ff53`. The summary
   lines are stable because cargo recomputes them per target.

So the progress metric for Tasks 1-12 is **the pair `(lib, lib test)` from
cargo's own summary**, and a task's report should quote both.

Error codes present: E0050, E0308, E0407, E0425, E0432, E0433, E0560, E0599,
E0609. This is the "before" picture Task 1 Step 1 asks for. Judge each of
Tasks 1-12 by whether this number moves in the right direction and by whether
the errors that remain are the ones the next task owns.

Task 16 keeps the rest of the rewiring — `Cargo.lock`, the `Makefile` target,
the example server, the e2e config. Only the `members` line moved.

---

## Task 1: Delete the usage-type catalog

Slice 2 removed the usage-type catalog from the gear entirely. `types-registry`
owns typing and the SPI never sees a declaration, so `CatalogStore`,
`PgCatalogStore`, `UsageTypeRow`, `usage_type_column` and the four catalog SPI
methods have nothing behind them. This is deletion work, not port work, and it
is first because it makes every later task smaller.

**Measured blast radius:** 877 lines in three dedicated files, plus references
in 19 other files (`grep -rn 'catalog' --include='*.rs' .` reports 114 hits
total in the crate).

**Files:**
- Delete: `src/infra/storage/catalog_store.rs`, `src/infra/storage/catalog_store_tests.rs`, `tests/catalog_integration_pg.rs`
- Modify: `src/infra/storage.rs`, `src/domain/ports.rs`, `src/domain/adapter.rs`, `src/gear.rs`, `src/infra/storage/entity.rs`, `src/infra/storage/mapper.rs`, `src/infra/storage/query/translate.rs`, `src/infra/storage/error.rs`, `src/infra/metrics.rs`

- [ ] **Step 1: Confirm the starting error count**

Task 0 already made the crate visible to cargo and recorded the baseline: **44
lib errors, 63 lib-test errors, 74 `error…` lines total** at `490d42c8e`.
Re-confirm it rather than trusting this file:

```bash
cargo check -p cf-gears-timescaledb-usage-collector-plugin --all-targets 2>&1 | tail -40
```

Save the full output to the session scratchpad — **not to the repository**.

This task will not make the crate compile. Nothing before Task 13 will. Judge
it by whether the error count drops and by whether the errors that remain are
the ones a later task owns.

- [ ] **Step 2: Delete the three catalog-dedicated files**

```bash
cd gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
git rm src/infra/storage/catalog_store.rs \
       src/infra/storage/catalog_store_tests.rs \
       tests/catalog_integration_pg.rs
```

- [ ] **Step 3: Remove the module declaration**

`src/infra/storage.rs` is 7 lines and declares the storage submodules. Remove
the `pub mod catalog_store;` line. Leave the others.

- [ ] **Step 4: Delete the `CatalogStore` port**

In `src/domain/ports.rs`, delete the entire `CatalogStore` trait (the
`/// Catalog operations on ‘usage_type_catalog‘. Implemented by infra.` doc
comment and the `#[async_trait] pub trait CatalogStore { … }` block). Remove
`UsageType` and `UsageTypeGtsId` from the `usage_collector_sdk` import list if
nothing else in the file uses them.

- [ ] **Step 5: Delete the four catalog methods from the adapter**

In `src/domain/adapter.rs`, delete `create_usage_type`, `get_usage_type`,
`list_usage_types` and `delete_usage_type` from the
`impl UsageCollectorPluginV1 for StorageAdapter` block. Delete the `catalog`
field from the `StorageAdapter` struct and its parameter from
`StorageAdapter::new`, leaving:

```rust
#[domain_model]
pub(crate) struct StorageAdapter {
    record: Arc<dyn RecordStore>,
}

impl StorageAdapter {
    #[must_use]
    pub(crate) fn new(record: Arc<dyn RecordStore>) -> Self {
        Self { record }
    }
}
```

Update the struct doc comment: it currently says "Delegates record ops to the
[`RecordStore`] port and catalog ops to the [`CatalogStore`] port." The second
half is now false. **Replace the sentence, do not append a retraction to it** —
a reworded claim that keeps the dead half ships the dead half.

- [ ] **Step 6: Unwire the catalog store from the gear**

In `src/gear.rs`, delete the `PgCatalogStore` import, the
`use crate::domain::ports::{CatalogStore, RecordStore};` becomes
`use crate::domain::ports::RecordStore;`, delete the `let catalog: Arc<dyn CatalogStore> = …`
binding, and change `StorageAdapter::new(record, catalog)` to
`StorageAdapter::new(record)`. The `metrics` clone that fed `PgCatalogStore`
was the last use of the un-cloned `metrics`; pass `metrics` to `PgRecordStore`
directly rather than `metrics.clone()`.

Update the comment `// Wire the storage stack: record + catalog stores behind
the adapter.` and the sentence after it (`Both stores share the one metric
inventory via ‘Arc<Metrics>‘.`) — there is one store now.

- [ ] **Step 7: Delete `UsageTypeRow` and the catalog mapper helpers**

In `src/infra/storage/entity.rs`, delete the `UsageTypeRow` struct. Update the
module doc: it opens "`sqlx` row structs mirroring the `usage_records`
hypertable and the `usage_type_catalog` table (see `migrations/0001_init.sql`)"
and lists `text[]` -> `Vec<String>` among the column type mappings. Both are now
wrong. Rewrite the paragraph.

In `src/infra/storage/mapper.rs`, delete `parse_kind`, `kind_to_sql` and
`type_row_to_model`, and remove `UsageKind`, `UsageType` and `UsageTypeRow` from
the imports. Leave `parse_status`, `status_to_sql`, `gts_id_str` and
`gts_id_from_str` for now — Task 5 owns them.

- [ ] **Step 8: Delete `usage_type_column`**

In `src/infra/storage/query/translate.rs`, delete the `usage_type_column`
function and its doc comment. The `SqlCtx::binds` field doc says it is "read
only by the in-crate stores (record/catalog) and the query tests" — correct it
to name the record store alone.

- [ ] **Step 9: Sweep the remaining catalog references**

```bash
cd gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
grep -rn -i 'catalog\|usage_type\|UsageType' --include='*.rs' .
```

Work the list to empty. The known remainders are in `src/infra/storage/error.rs`
and `src/infra/storage/error_tests.rs` (a catalog foreign-key error path),
`src/infra/metrics.rs` and `src/infra/metrics_tests.rs` (catalog-labelled
metrics), `src/infra/storage/query/bind.rs` (a doc reference), and the four
surviving `tests/*_pg.rs` files (which seed the catalog in their fixtures).

For the `tests/*_pg.rs` files, delete only the catalog seeding — the rest of
each file is rewritten in Task 15, and deleting them wholesale now loses the
record of what they asserted.

**The `--include='*.rs'` filter above is too narrow and will miss things.** It
structurally cannot see `README.md`, `migrations/*.sql`, or any doc. Run the
sweep again without it and handle what it finds:

```bash
grep -rn -i 'catalog\|usage_type' \
  gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/
```

The plugin's own `README.md` **is** in scope — no other task owns it, and it
advertises the catalog in its opening description. `migrations/0001_init.sql`
is **not**: it still creates `usage_type_catalog`, and Task 3 replaces the
whole file. The plugin's `docs/` directory is **not**: it is deliberately
stale, it is where the `@cpt-` traceability marker ids resolve, and DIVERGENCES
entries 5 and 14 cover it. Leave both alone, and say in your report that you
did.

**For every deletion in this step, state which of the two it is:** "deleted
because the thing it tested no longer exists" or "deleted because it fails".
Those look identical in a diff and only the first is legitimate here. Report a
per-item verdict.

**A third category is illegitimate and is the one to watch for: "deleted so a
grep would come back empty."** A test asserting something that still exists is
a live test even when its subject is scheduled for removal two tasks later.
`tests/schema_integration_pg.rs` is the specific trap: it probes
`usage_type_catalog` with raw SQL naming no Rust symbol, so that table still
exists until Task 3 replaces the schema. If a sweep pressures you toward
deleting a live assertion, stop and report it — the sweep is what is wrong.

- [ ] **Step 10: Confirm the catalog is gone and the error count dropped**

```bash
cd gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
grep -rn -i 'catalog' --include='*.rs' . ; echo "exit=$?"
```

Expected: no output, `exit=1`. (`grep` exits 1 on no match — the `echo` is why
this is not chained with `&&`.)

```bash
cargo check -p cf-gears-timescaledb-usage-collector-plugin --all-targets 2>&1 | grep -c '^error\[\|^error:'
```

Expected: fewer errors than Step 1 recorded, and no error naming bare
`UsageType`, `UsageKind` or `CatalogStore`.

**`UsageTypeGtsId` errors are expected to remain and are not yours.** Step 4
says to keep it where it is still used, and it survives on `RecordStore::list`
and `RecordStore::aggregate` (`ports.rs`), on the adapter's two matching
methods, and throughout `mapper.rs`. Those are *record*-path signatures, not
catalog ones: the SDK replaced the type with `MeterTypeId`, and Tasks 5, 11 and
12 do the replacement. `UsageTypeNotFound` likewise stays until Task 9 rewrites
`map_insert_error`.

**Do not delete anything merely to make a grep return empty.** If a grep hit
belongs to a later task, leave it and say so in your report.

- [ ] **Step 11: Commit**

```bash
cd /Users/binarycode/code/virtuozzo/gears-rust
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
git commit -s -m "refactor(timescaledb-plugin)!: delete the usage-type catalog

Slice 2 moved typing to types-registry and the SPI never sees a declaration,
so CatalogStore, PgCatalogStore, UsageTypeRow and usage_type_column have
nothing behind them. Delete the catalog store, its tests, its integration
suite, and the four catalog methods on the SPI adapter.

BREAKING CHANGE: the plugin no longer serves the usage-type catalog SPI
methods, which the SPI no longer declares."
```

---

## Task 2: Delete `deactivate` and the correction-era store surface

Slice 4 replaced `status` / `corrects_id` with an appended invalidation entry.
A correction is no longer a mutation of an existing row, so
`deactivate_usage_record` has no meaning: the SPI does not declare it and
nothing can call it.

**Files:**
- Modify: `src/domain/ports.rs`, `src/domain/adapter.rs`, `src/infra/storage/record_store.rs`, `src/infra/storage/record_store_tests.rs`, `src/infra/metrics.rs`, `src/infra/metrics_tests.rs`, `src/config.rs`, `src/infra/storage/pool.rs`, and three `tests/*_pg.rs`

**Measured blast radius — wider than the trait method.** `grep -rn 'deactivate'
--include='*.rs'` reports **41 hits across 9 files**, not the 4 an earlier draft
of this task listed. Beyond the port, adapter and store:

- **`src/infra/metrics.rs` publishes a whole metric for the operation**:
  `uc_timescaledb_deactivate_duration_seconds`, its `deactivate_duration`
  histogram field, its registration in `Metrics::new`, the `record_deactivate`
  recorder, and a `TimedOp::Deactivate` variant with its match arm. **All of it
  goes.** A metric measuring an operation that cannot be invoked is worse than
  a dead function: an operator can build an alert on it and the alert never
  fires. That is the same defect class DIVERGENCES entry 4 is filed under, and
  this task is where it would be introduced rather than inherited.
- ~~**`src/infra/metrics_tests.rs`** asserts on that metric.~~ **This was wrong.**
  That file contains zero `deactivate` references and never did; Task 2's
  implementer verified it at the base commit and correctly deleted nothing
  rather than inventing a deletion to satisfy the step.
- **`src/config.rs:79` and `src/infra/storage/pool.rs:69`** each cite the
  deactivate `SELECT … FOR UPDATE` as the worked example in a doc comment about
  lock timeouts. The surrounding guidance is still true; only the example is
  dead. **Re-point the example at a live statement rather than deleting the
  paragraph** — the lock-timeout rationale is load-bearing and losing it to
  tidy away one clause would be a real regression.

  **Amended after execution: "re-point at a live `SELECT … FOR UPDATE`" was
  impossible.** The `deactivate` body held the only `FOR UPDATE`, and indeed the
  only `UPDATE` or `DELETE`, anywhere in `src/`. After this task the plugin is
  insert-and-select only and takes no explicit row lock, so no equivalent
  example existed to point at. The right answer was to cite a *different* real
  statement and adjust the surrounding claim to match: the example became an
  ingest `INSERT … ON CONFLICT … DO NOTHING` meeting an uncommitted duplicate,
  and "row lock" was widened to "contended lock", because that conflict waits
  on the inserting transaction's XID lock rather than a row lock. Keeping "row
  lock" would have swapped one invented mechanism for another.
- Three `tests/*_pg.rs` files. Delete only the deactivate coverage; the rest is
  Task 15's.

- [ ] **Step 1: Delete `deactivate` from the port**

In `src/domain/ports.rs`, delete this line from the `RecordStore` trait:

```rust
    async fn deactivate(&self, id: Uuid) -> Result<(), UsageCollectorPluginError>;
```

- [ ] **Step 2: Delete `deactivate_usage_record` from the adapter**

In `src/domain/adapter.rs`, delete:

```rust
    async fn deactivate_usage_record(&self, id: Uuid) -> Result<(), UsageCollectorPluginError> {
        self.record.deactivate(id).await
    }
```

- [ ] **Step 3: Delete the implementation**

In `src/infra/storage/record_store.rs`, delete the `async fn deactivate` body
from `impl RecordStore for PgRecordStore` (it starts at `:1343` and runs to the
end of the impl block — verify the line before cutting; this file is edited by
several tasks and line numbers move).

- [ ] **Step 4: Delete its tests, with a per-test verdict**

```bash
cd gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
grep -rn 'deactivate' --include='*.rs' .
```

**Not every hit is a deletion.** Sort them into three piles and say which pile
each went in:

1. **Delete** — the trait method, the adapter method, the store impl, the
   metric and its recorder, and every test whose subject is one of those. For
   each, report whether it was deleted "because its question no longer exists"
   or "because it failed". All should be the former; if any is the latter, stop
   and say so.
2. **Re-point** — `src/config.rs:79` and `src/infra/storage/pool.rs:69`. The
   lock-timeout guidance around the example is still true and load-bearing.
   Swap the dead `SELECT … FOR UPDATE` example for a live statement; do not
   delete the paragraph.
3. **Leave** — anything belonging to a later task. Say what you left and why.

- [ ] **Step 5: Verify**

```bash
grep -rn 'deactivate' \
  gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/
```

**Expected: a small number of hits, not zero.** The re-pointed doc comments in
`config.rs` and `pool.rs` may still name the concept if that reads better than
a contrived substitute — what must be gone is every *executable* reference and
every metric.

**Do not delete anything merely to make this grep return empty.** Task 1 shows
what that pressure produces: a live assertion deleted because a sweep wanted a
clean result. Confirm instead that no surviving hit is code:

```bash
grep -rn 'deactivate' \
  gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/ \
  | grep -v '^\s*//' | grep -v '///'
```

Then read what that leaves and account for each line in your report.

- [ ] **Step 6: Commit**

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
git commit -s -m "refactor(timescaledb-plugin)!: delete deactivate_usage_record

Slice 4 replaced status/corrects_id with an appended invalidation entry, so a
correction is no longer a mutation of an existing row and the SPI no longer
declares this method.

BREAKING CHANGE: deactivate_usage_record is gone; withdrawal is an appended
invalidation entry submitted through create_usage_record."
```

---

## Task 3: Replace the schema

The table predates three model changes. Slice 3 replaced `created_at` with the
covered period `[window_start, window_end)`; slice 4 replaced
`status` / `corrects_id` with `invalidates` + `reason_code`; slice 5 added
`origin`. Per the owner's decision this is a **fresh schema**, not a third
migration.

**Files:**
- Rewrite: `migrations/0001_init.sql`
- Delete: `migrations/0002_rename_uuid_to_id.sql`

### What binds this schema

DESIGN §3.7 (lines 1526-1552) says concrete table shapes are plugin-internal
per `DATA-DESIGN-NO-001`, and binds them with exactly two obligations:

> The dedup identity must be enforced as a uniqueness constraint over the
> 5-tuple, and preserved for the retention horizon. The plugin assigns
> `acceptance_sequence` and must keep it strictly monotonic per
> `(tenant_id, gts_type_id)`.

The 5-tuple is `(tenant, gts_type, key, window_start, window_end)`
(`DESIGN.md:62`). `id` is a deterministic UUIDv5 over the same 5-tuple, and
`entry_type` is deliberately excluded from the derivation (`DESIGN.md:63`).

- [ ] **Step 1: Delete the second migration**

```bash
cd gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
git rm migrations/0002_rename_uuid_to_id.sql
```

- [ ] **Step 2: Write the new schema**

Replace the whole of `migrations/0001_init.sql` with:

```sql
-- TimescaleDB Usage Collector storage backend — base schema.
--
-- One table, the entry ledger. There is no usage-type catalog: declarations
-- live in types-registry and the storage SPI never sees one (DESIGN §3.7).
--
-- This file replaces the pre-slice-4 schema and its rename migration outright
-- rather than migrating from them. The gear is unreleased, so no deployment
-- holds rows worth a migration path; the retired 0002 said as much in its own
-- header.
CREATE EXTENSION IF NOT EXISTS timescaledb;

CREATE TABLE IF NOT EXISTS usage_records (
    -- Deterministic gateway-derived entry identity: UUIDv5 over the 5-tuple
    -- dedup identity (cpt-cf-usage-collector-adr-record-identity-derivation).
    -- `entry_type` is deliberately not an input to it.
    id                  uuid        NOT NULL,
    tenant_id           uuid        NOT NULL,
    gts_type_id         text        NOT NULL,
    value               numeric     NOT NULL,
    -- The covered period [window_start, window_end). The only emitter-supplied
    -- time attribution. Every selection predicate reads the end alone
    -- (cpt-cf-usage-collector-adr-window-end-selection), which is why the end
    -- is the hypertable partition column.
    window_start        timestamptz NOT NULL,
    window_end          timestamptz NOT NULL,
    resource_id         text        NOT NULL,
    resource_type       text        NOT NULL,
    subject_id          text,
    subject_type        text,
    idempotency_key     text        NOT NULL,
    -- The append-only invalidation pair. An invalidation entry names the entry
    -- it withdraws and carries a reason; an ordinary measurement carries
    -- neither (cpt-cf-usage-collector-adr-append-only-invalidation).
    invalidates         uuid,
    reason_code         text,
    origin              text        NOT NULL
        CHECK (origin IN ('live', 'backfill')),
    -- Materialized so `$filter=entry_type eq 'invalidation'` resolves to a
    -- column. The SDK prescribes exactly this expression and notes that the
    -- value hook cannot carry the field instead (models.rs, UsageRecordQuery).
    entry_type          text        GENERATED ALWAYS AS
        (CASE WHEN invalidates IS NULL THEN 'record' ELSE 'invalidation' END) STORED,
    -- Strictly monotonic per (tenant_id, gts_type_id); assigned by this plugin,
    -- never by the gear (DESIGN §3.7). Claimed from `usage_acceptance_sequence`
    -- below. Gaps are permitted: the obligation is monotonicity, not density,
    -- and an absorbed idempotent retry consumes a value it does not store.
    acceptance_sequence bigint      NOT NULL,
    metadata            jsonb       NOT NULL DEFAULT '{}'::jsonb,
    ingested_at         timestamptz NOT NULL DEFAULT now(),

    -- A hypertable's PRIMARY KEY and every UNIQUE must contain the partition
    -- column, so both carry `window_end`.
    PRIMARY KEY (id, window_end),

    -- The DESIGN §3.7 dedup obligation, over the 5-tuple verbatim.
    CONSTRAINT usage_records_dedup_uniq
        UNIQUE (tenant_id, gts_type_id, idempotency_key, window_start, window_end),

    -- A point event is window_start == window_end; a period is strictly
    -- ordered. Nothing admits window_end < window_start.
    CONSTRAINT usage_records_window_ordered
        CHECK (window_start <= window_end),

    -- The pair is all-or-nothing: an invalidation names a target and carries a
    -- reason, an ordinary measurement does neither. A reason without a target
    -- would be an unmarked correction, which the model has no room for.
    CONSTRAINT usage_records_invalidation_pairing
        CHECK (
            (invalidates IS NULL AND reason_code IS NULL)
            OR (invalidates IS NOT NULL AND reason_code IS NOT NULL)
        )
);

SELECT create_hypertable('usage_records', 'window_end', if_not_exists => TRUE);

-- At most one accepted invalidation per entry, enforced by the database rather
-- than by a read-then-write in the store.
--
-- Why this can be a plain UNIQUE despite the hypertable partition-column rule:
-- an invalidation is a faithful copy of the entry it withdraws, so it shares
-- that entry's covered period and therefore its `window_end`. Two invalidations
-- of one target necessarily collide on (invalidates, window_end), so including
-- the partition column costs nothing and satisfies the constraint rule.
CREATE UNIQUE INDEX IF NOT EXISTS usage_records_one_invalidation_uniq
    ON usage_records (invalidates, window_end)
    WHERE invalidates IS NOT NULL;

-- Per-scope acceptance-sequence counters.
--
-- A Postgres SEQUENCE is global, and per-scope monotonicity would need one
-- sequence per (tenant, meter) — unbounded DDL driven by tenant data. A counter
-- row claimed with `ON CONFLICT DO UPDATE … RETURNING` is per-scope by
-- construction and serializes concurrent ingest for one scope on the row lock,
-- which is what strict monotonicity costs.
CREATE TABLE IF NOT EXISTS usage_acceptance_sequence (
    tenant_id   uuid   NOT NULL,
    gts_type_id text   NOT NULL,
    next_value  bigint NOT NULL,
    PRIMARY KEY (tenant_id, gts_type_id)
);

-- Read paths select on the period end within a (tenant, meter) scope.
CREATE INDEX IF NOT EXISTS usage_records_tenant_type_window_idx
    ON usage_records (tenant_id, gts_type_id, window_end DESC);
CREATE INDEX IF NOT EXISTS usage_records_tenant_window_idx
    ON usage_records (tenant_id, window_end DESC);
-- The fold's second withdrawal-exclusion obligation resolves through this:
-- "is this entry named by an accepted invalidation?"
CREATE INDEX IF NOT EXISTS usage_records_invalidates_idx
    ON usage_records (invalidates) WHERE invalidates IS NOT NULL;
-- The LATEST fold's declared tie-break, and the feed's future keyset.
CREATE INDEX IF NOT EXISTS usage_records_acceptance_seq_idx
    ON usage_records (tenant_id, gts_type_id, acceptance_sequence DESC);
```

- [ ] **Step 2b: Note what dropping the catalog table releases**

Task 1 removed the catalog seeding from the surviving `tests/*_pg.rs` fixtures,
and that seeding was the only thing satisfying `usage_records_gts_id_fk` in the
retired schema. Until this task lands, every record insert in those suites
would fail with SQLSTATE 23503. Replacing the schema drops both the constraint
and the table, which resolves it.

**One test is now green for the wrong reason and Task 15 must delete it:**
`pg_insert_with_unregistered_gts_id_is_usage_type_not_found` asserted that an
unregistered `gts_id` is refused. After this task there is no registry for an
id to be absent from, so the assertion cannot fail and cannot discriminate.
That is a dead question wearing a green tick. Carry it to Task 15 Step 1's
inventory as an explicit **delete**, not a repoint.

- [ ] **Step 2c: Retire what the dropped constraint stranded**

Dropping `usage_type_catalog` drops `usage_records_gts_id_fk` with it — the
**only** foreign key in the schema. Three live things exist solely to classify
its violation and are dead the moment this migration lands:

- `DbErrorClass::ForeignKeyViolation` (`src/infra/storage/error.rs`)
- its `"23503"` match arm in `classify_db`, and the test
  `fk_violation_is_foreign_key_class` (`src/infra/storage/error_tests.rs`)
- `map_insert_error`'s FK branch (`src/infra/storage/record_store.rs`), which
  returns a `UsageTypeNotFound` the SDK no longer declares

**No other task owns these** — Task 9 is scoped to `map_insert_error` alone, so
without this step the enum arm and its test survive as live code guarding a
constraint that no longer exists. Task 1's code review found the gap; this is
where it closes, because the task that deletes a constraint should delete what
guards it.

Delete all three, with a per-item verdict. If removing the FK branch from
`map_insert_error` conflicts with Task 9's rewrite of the same function, leave
a comment saying so rather than half-doing it, and report it.

**Note for whoever reads this later:** Task 1 already narrowed this class by
dropping the unreachable `"23001"` arm, having verified no DELETE path exists
in the plugin and that the FK carried no `ON UPDATE` clause. That was a
behavior narrowing, not just a comment fix, and it is moot once the FK is gone.

- [ ] **Step 3: Confirm the retention policy needs no change — it does not**

**Measured before this plan was revised: `grep -c 'created_at'
src/infra/storage/pool.rs` returns 0.** An earlier draft of this step asserted
the file "names the hypertable's time column" and told you to fix it. That was
wrong, and the correction matters because acting on it would have produced a
change with nothing behind it.

`apply_retention_policy` calls:

```sql
SELECT add_retention_policy('usage_records',
       drop_after => make_interval(secs => $1::double precision))
```

`add_retention_policy` takes **no column argument**. It drops chunks by the
hypertable's own time dimension, whatever `create_hypertable` partitioned on.
So re-partitioning on `window_end` in Step 2 is sufficient on its own, and this
file needs no edit.

Confirm rather than assume:

```bash
grep -n 'created_at\|add_retention_policy\|drop_after' \
  gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/pool.rs
```

Expected: the two `retention_policy` calls, no `created_at`.

**Say in your report that you checked and changed nothing here.** A step that
correctly results in no edit is a finding, not a skipped step.

Worth recording once, because it is the reason the outcome is right rather than
lucky: retention measured from the covered period is what
`cpt-cf-usage-collector-fr-idempotency` requires — *"The horizon is the type's
retention policy, measured from the covered period"* — so `window_end` is the
semantically correct dimension to age chunks on, not merely the mechanically
required one. Partitioning on it gets both properties from one decision.

- [ ] **Step 4: Verify the SQL parses**

The migration cannot be applied without Docker, and the crate does not compile
yet, so the check available now is a syntax read. If a TimescaleDB container is
available, apply it directly:

```bash
docker run --rm -d --name uc-schema-check -e POSTGRES_PASSWORD=pw -p 55433:5432 \
  timescale/timescaledb:latest-pg16
sleep 5
PGPASSWORD=pw psql -h localhost -p 55433 -U postgres -f \
  gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/migrations/0001_init.sql
docker rm -f uc-schema-check
```

Expected: `CREATE EXTENSION`, `CREATE TABLE`, a `create_hypertable` row,
`CREATE INDEX` x4, `CREATE TABLE`. No `ERROR:`.

If Docker is unavailable, say so plainly in the task report rather than
claiming the schema was verified. Task 15 applies it for real.

- [ ] **Step 5: Commit**

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/migrations
git commit -s -m "feat(timescaledb-plugin)!: replace the schema with the current model

The table predated three model changes: the covered period replaced
created_at, invalidates + reason_code replaced status/corrects_id, and origin
was added. Replace both migrations with a single init carrying the 5-tuple
dedup constraint DESIGN 3.7 requires, a plugin-assigned acceptance_sequence
monotonic per (tenant_id, gts_type_id), and a partial unique index enforcing
at most one invalidation per entry.

The gear is unreleased, so no migration path is owed.

BREAKING CHANGE: the usage_records table shape is replaced and the
usage_type_catalog table is dropped. An existing database must be recreated."
```

---

## Task 4: Bring `UsageRecordRow` to the new columns

**Files:**
- Modify: `src/infra/storage/entity.rs`

- [ ] **Step 1: Rewrite `UsageRecordRow`**

Replace the struct with one mirroring the Task 3 schema. Column order here is
load-bearing: `record_store.rs` decodes positionally against a `RECORD_COLUMNS`
constant, and Task 9 keeps the two in the same order.

```rust
/// One row of the `usage_records` hypertable.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UsageRecordRow {
    /// `id` — deterministic gateway-derived entry identity (part of the
    /// composite PK).
    pub id: Uuid,
    /// `tenant_id` — owning tenant.
    pub tenant_id: Uuid,
    /// `gts_type_id` — the meter this entry was submitted against.
    pub gts_type_id: String,
    /// `value` — signed `numeric` quantity.
    pub value: Decimal,
    /// `window_start` — inclusive start of the covered period.
    pub window_start: OffsetDateTime,
    /// `window_end` — exclusive end of the covered period, and the
    /// hypertable time dimension. Every selection predicate reads this.
    pub window_end: OffsetDateTime,
    /// `resource_id` — resource attribution leaf.
    pub resource_id: String,
    /// `resource_type` — resource attribution leaf.
    pub resource_type: String,
    /// `subject_id` — optional subject attribution leaf.
    pub subject_id: Option<String>,
    /// `subject_type` — optional subject attribution leaf.
    pub subject_type: Option<String>,
    /// `idempotency_key` — caller-supplied dedup key.
    pub idempotency_key: String,
    /// `invalidates` — the entry this one withdraws, when it is an
    /// invalidation. `NULL` on an ordinary measurement.
    pub invalidates: Option<Uuid>,
    /// `reason_code` — why the withdrawal was issued. Present exactly when
    /// `invalidates` is, by table constraint.
    pub reason_code: Option<String>,
    /// `origin` — `'live'` / `'backfill'`; the ingestion path that admitted
    /// this entry.
    pub origin: String,
    /// `acceptance_sequence` — plugin-assigned, strictly monotonic per
    /// `(tenant_id, gts_type_id)`. Not carried on the SDK model; this
    /// backend assigns it and orders on it, and nothing reads it back out
    /// through the SPI.
    pub acceptance_sequence: i64,
    /// `metadata` — `jsonb` object of declared metadata keys → string values.
    pub metadata: serde_json::Value,
    /// `ingested_at` — server insert timestamp (`DEFAULT now()`).
    pub ingested_at: OffsetDateTime,
}
```

Note `entry_type` is **not** a field. It is a generated column that exists so
`$filter` can name it; nothing decodes it, because the SDK model derives the
same fact from `invalidation.is_some()`. Say that in the struct doc so the next
reader does not "fix" the omission.

- [ ] **Step 2: Fix the module doc**

The header names the `usage_type_catalog` table and lists `text[]` ->
`Vec<String>`. Both are gone. Rewrite it to name one table and the types the
new columns actually use.

- [ ] **Step 3: Commit**

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/entity.rs
git commit -s -m "refactor(timescaledb-plugin): bring UsageRecordRow to the new columns

Replaces created_at with the covered-period pair, status/corrects_id with
invalidates/reason_code, and adds origin and the plugin-assigned
acceptance_sequence. entry_type is a generated column nothing decodes."
```

---

## Task 5: Bring the mapper to the current model

**Files:**
- Modify: `src/infra/storage/mapper.rs`, `src/infra/storage/mapper_tests.rs`

- [ ] **Step 1: Write the failing test first**

Add to `src/infra/storage/mapper_tests.rs` a test that a row carrying an
invalidation maps to a model carrying one:

```rust
#[test]
fn an_invalidation_row_maps_to_a_record_carrying_the_pair() {
    let target = Uuid::new_v4();
    let row = UsageRecordRow {
        invalidates: Some(target),
        reason_code: Some("duplicate_submission".to_owned()),
        ..sample_row()
    };

    let model = record_row_to_model(row).expect("row must map");

    let invalidation = model
        .invalidation
        .expect("a row with invalidates must map to Some(Invalidation)");
    assert_eq!(invalidation.target, target);
    assert_eq!(invalidation.reason.as_str(), "duplicate_submission");
}
```

**The mutation that makes this red:** change `record_row_to_model` to write
`invalidation: None` unconditionally. If that edit leaves the test green, the
test is not testing anything.

Add a second test for the half-populated row, which the table constraint
forbids but a mapper must still refuse rather than silently drop:

```rust
#[test]
fn a_row_naming_a_target_without_a_reason_is_an_invariant_break() {
    let row = UsageRecordRow {
        invalidates: Some(Uuid::new_v4()),
        reason_code: None,
        ..sample_row()
    };

    let err = record_row_to_model(row).expect_err("half a pair must not map");

    assert!(
        matches!(err, UsageCollectorPluginError::Internal(_)),
        "a malformed stored row is a plugin invariant break, not a caller error; got {err:?}"
    );
}
```

**The mutation:** make the `(Some(target), None)` arm return
`Ok(None)` instead of an error.

You will need a `sample_row()` helper returning a valid `UsageRecordRow`. Write
it by hand from the schema — **not** by calling the mapper's own inverse. A
fixture built by the code under test proves the round trip is self-consistent
and nothing else; that is the "two layers tested against themselves" defect,
and it has shipped here before.

- [ ] **Step 2: Run the tests and watch them fail**

```bash
cd gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --no-fail-fast -E 'test(mapper)' 2>&1 | tail -20
```

Expected: compile errors naming `invalidation`, `invalidates` and
`reason_code`. That is the correct failure at this point — the field does not
exist yet.

- [ ] **Step 3: Delete the dead helpers**

From `src/infra/storage/mapper.rs` delete `parse_status` and `status_to_sql`
(slice 4 removed `UsageRecordStatus`), and rename the GTS helpers to the type
that replaced `UsageTypeGtsId`:

```rust
/// Borrow the raw GTS type id string out of a [`MeterTypeId`] (for binding).
#[must_use]
pub fn meter_type_id_str(gts_type_id: &MeterTypeId) -> &str {
    gts_type_id.as_ref()
}

/// Reconstruct a validated [`MeterTypeId`] from a stored string.
///
/// # Errors
///
/// Returns [`UsageCollectorPluginError::Internal`] when the stored value is not
/// a valid meter type id (a stored-data invariant break).
pub fn meter_type_id_from_str(raw: &str) -> Result<MeterTypeId, UsageCollectorPluginError> {
    MeterTypeId::new(raw).map_err(|e| {
        UsageCollectorPluginError::internal(format!("stored gts_type_id `{raw}` invalid: {e}"))
    })
}
```

Confirm `MeterTypeId::new` is the constructor and `AsRef<str>` is implemented
before writing this — check
`gears/system/usage-collector/usage-collector-sdk/src/models.rs:559` onward.
If the constructor is spelled differently, follow the SDK, not this plan.

- [ ] **Step 4: Add the origin and invalidation parsers**

```rust
/// Parse a stored `origin` string into [`RecordOrigin`].
///
/// Matches the DDL `CHECK (origin IN ('live', 'backfill'))`.
///
/// # Errors
///
/// Returns [`UsageCollectorPluginError::Internal`] for any other value.
pub fn parse_origin(raw: &str) -> Result<RecordOrigin, UsageCollectorPluginError> {
    match raw {
        "live" => Ok(RecordOrigin::Live),
        "backfill" => Ok(RecordOrigin::Backfill),
        other => Err(UsageCollectorPluginError::internal(format!(
            "stored origin `{other}` is not 'live'/'backfill'"
        ))),
    }
}

/// SQL string form of a [`RecordOrigin`] (inverse of [`parse_origin`]).
#[must_use]
pub fn origin_to_sql(origin: RecordOrigin) -> &'static str {
    match origin {
        RecordOrigin::Live => "live",
        RecordOrigin::Backfill => "backfill",
    }
}

/// Reassemble the stored invalidation pair into an [`Invalidation`].
///
/// The table constrains the two columns to be both present or both absent, so
/// a half-populated row is a stored-invariant break rather than a shape the
/// model can carry.
///
/// # Errors
///
/// Returns [`UsageCollectorPluginError::Internal`] when exactly one of the two
/// is present, or when the stored reason fails [`ReasonCode`] validation.
pub fn invalidation_from_row(
    invalidates: Option<Uuid>,
    reason_code: Option<String>,
) -> Result<Option<Invalidation>, UsageCollectorPluginError> {
    match (invalidates, reason_code) {
        (None, None) => Ok(None),
        (Some(target), Some(raw)) => {
            let reason = ReasonCode::new(raw).map_err(|e| {
                UsageCollectorPluginError::internal(format!("stored reason_code invalid: {e}"))
            })?;
            Ok(Some(Invalidation { target, reason }))
        }
        (Some(target), None) => Err(UsageCollectorPluginError::internal(format!(
            "stored entry `{target}` names an invalidation target with no reason_code"
        ))),
        (None, Some(raw)) => Err(UsageCollectorPluginError::internal(format!(
            "stored entry carries reason_code `{raw}` with no invalidation target"
        ))),
    }
}
```

Verify `ReasonCode`'s constructor name and whether it exposes `as_str` before
writing the test assertion in Step 1 against it:

```bash
grep -n 'impl ReasonCode' -A25 \
  gears/system/usage-collector/usage-collector-sdk/src/reason.rs
```

- [ ] **Step 5: Rewrite `record_row_to_model`**

```rust
pub fn record_row_to_model(row: UsageRecordRow) -> Result<UsageRecord, UsageCollectorPluginError> {
    let gts_type_id = meter_type_id_from_str(&row.gts_type_id)?;

    let resource_ref = ResourceRef::new(row.resource_id, row.resource_type).map_err(|e| {
        UsageCollectorPluginError::internal(format!("stored resource_ref invalid: {e}"))
    })?;

    let subject_ref = match row.subject_id {
        Some(subject_id) => Some(SubjectRef::new(subject_id, row.subject_type).map_err(|e| {
            UsageCollectorPluginError::internal(format!("stored subject_ref invalid: {e}"))
        })?),
        None => None,
    };

    let idempotency_key = IdempotencyKey::new(row.idempotency_key).map_err(|e| {
        UsageCollectorPluginError::internal(format!("stored idempotency_key invalid: {e}"))
    })?;

    let metadata = metadata_jsonb_to_map(row.metadata)?;
    let origin = parse_origin(&row.origin)?;
    let invalidation = invalidation_from_row(row.invalidates, row.reason_code)?;

    Ok(UsageRecord {
        id: row.id,
        gts_type_id,
        tenant_id: row.tenant_id,
        resource_ref,
        subject_ref,
        metadata,
        value: row.value,
        idempotency_key,
        origin,
        invalidation,
        window_start: row.window_start,
        window_end: row.window_end,
    })
}
```

That is all twelve `UsageRecord` fields. `acceptance_sequence` and
`ingested_at` are read off the row and deliberately dropped: neither is on the
model. Write that down at the function, because "the mapper silently discards
two columns" is exactly the kind of thing a later reader files as a bug.

- [ ] **Step 6: Update the module header**

It currently explains `UsageTypeGtsId` and the `gts_id_from_str` signature at
length, and cites "the task skeleton". Rewrite for the types that exist.

- [ ] **Step 7: Run the tests**

```bash
cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --no-fail-fast -E 'test(mapper)' 2>&1 | tail -20
```

Expected: the mapper tests pass. Other modules still fail to compile; that is
Tasks 6-13.

- [ ] **Step 8: Prove the two new tests discriminate**

Copy the tree to the scratchpad first — **`git checkout` restores from HEAD and
would discard uncommitted work, and there is uncommitted work in this tree.**

```bash
SNAP=/private/tmp/claude-501/-Users-binarycode-code-virtuozzo-gears-rust/347fc0db-cecd-4d4a-a9cb-c0475d8144d6/scratchpad/mapper-snap
cp -a gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/mapper.rs "$SNAP"
```

Apply each named mutation with an **absolute** path, `touch` the file (`mv`
preserves mtime and cargo skips the rebuild), confirm a `Compiling` line
appears, **grep the mutated line to confirm the edit actually landed**, and
confirm the expected test goes red. Restore from `$SNAP`, never from git.

- [ ] **Step 9: Commit**

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/
git commit -s -m "feat(timescaledb-plugin): map rows to the current record model

Reassembles the invalidation pair from invalidates + reason_code, parses
origin, and carries the covered period. Drops the status and usage-type-kind
parsers, which have no model behind them.

A half-populated invalidation pair is refused as an Internal: the table
constrains the two columns together, so half a pair is a stored-invariant
break rather than a shape the model can carry."
```

---

## Task 6: Fix `record_column` (DIVERGENCES entry 16)

This is entry 16's own proposed resolution. The allowlist is the closed
security boundary every `$filter` conjunct passes, and it is wrong in both
directions.

**Verified against the file at the time of writing:** `record_column` is at
`src/infra/storage/query/translate.rs:55` and maps nine identifiers — `id`,
`created_at`, `tenant_id`, `resource_id`, `resource_type`, `subject_id`,
`subject_type`, `corrects_id`, `status`. Re-verify before editing; earlier
tasks in this plan do not touch this function, but the file has moved before.

**The published `$filter` field set is eight** (`usage-collector-v1.yaml:440`):
`tenant_id`, `resource_id`, `resource_type`, `subject_id`, `subject_type`,
`entry_type`, `origin`, `invalidates`. The allowlist covers five of them.

**Files:**
- Modify: `src/infra/storage/query/translate.rs`, `src/infra/storage/query/translate_tests.rs`

**Two stale doc blocks in this file are yours, and Task 1's spec review found
them.** Both assert a field list the SDK no longer has:

- **`translate.rs:22-25`** — the module doc claims `UsageRecordFilterField`'s
  names "are exactly `"id"`, `"created_at"`, … `"corrects_id"`, `"status"`".
  The SDK's `UsageRecordQuery` (`usage-collector-sdk/src/models.rs`) declares
  `id`, `window_start`, `window_end`, `tenant_id`, `resource_id`,
  `resource_type`, `subject_id`, `subject_type`. Task 1 edited this block
  (stripping its usage-type half) and left the stale list standing.
- **`record_column`'s own doc** — "only these nine identifiers", corrected by
  Step 3 below.

Re-verify both line numbers before editing.

- [ ] **Step 1: Write the failing tests**

In `src/infra/storage/query/translate_tests.rs`:

```rust
/// The published `$filter` field set (`usage-collector-v1.yaml:440`). Every
/// one of these must resolve to a column, or a valid request is answered with
/// an `Internal`.
const PUBLISHED_FILTER_FIELDS: &[&str] = &[
    "tenant_id",
    "resource_id",
    "resource_type",
    "subject_id",
    "subject_type",
    "entry_type",
    "origin",
    "invalidates",
];

#[test]
fn every_published_filter_field_resolves_to_a_column() {
    for field in PUBLISHED_FILTER_FIELDS {
        assert!(
            record_column(field).is_some(),
            "`$filter={field} eq …` is a predicate the published contract names \
             and the gear's reject_reserved_filter_fields guard admits, so an \
             allowlist that drops it answers a valid request with a 500"
        );
    }
}

#[test]
fn every_keyset_safe_field_resolves_to_a_column() {
    for field in usage_collector_sdk::KEYSET_SAFE_RECORD_FIELDS {
        assert!(
            record_column(field).is_some(),
            "`{field}` is an admissible `$orderby` key, and the canonical \
             (window_end, id) keyset cannot render at all unless it resolves"
        );
    }
}

#[test]
fn no_retired_model_field_resolves_to_a_column() {
    for field in ["created_at", "corrects_id", "status"] {
        assert!(
            record_column(field).is_none(),
            "`{field}` was removed from the model by slices 3 and 4; an \
             allowlist that still maps it lets a `$filter` naming it past the \
             boundary and fail against the table"
        );
    }
}
```

**The mutations:** drop `"origin"` from the match (test 1 red); drop
`"window_end"` (test 2 red); re-add `"status"` (test 3 red). Each is a single
line, and each targets a different one of the three.

Note the second test iterates `KEYSET_SAFE_RECORD_FIELDS` rather than a local
copy. That is deliberate: the SDK growing an eighth keyset field makes this
test fail here, which is the coupling you want. A hand-copied list would go
quietly stale — the failure mode this repository is named for.

- [ ] **Step 2: Run them and watch them fail**

```bash
cd gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --no-fail-fast -E 'test(translate)' 2>&1 | tail -30
```

Expected: tests 1 and 2 fail (`entry_type`, `origin`, `invalidates`,
`window_start`, `window_end` all resolve to `None`); test 3 fails on all three.

- [ ] **Step 3: Rewrite the allowlist**

```rust
/// Closed allowlist mapping a `usage_records` filter-field name to its column.
///
/// The map is the identity (field name == column name); the closed `match` is
/// the security boundary — only these eleven identifiers can ever reach the SQL
/// string. `gts_type_id` is intentionally absent: it is a typed parameter on
/// the SPI, not a `$filter` field, and neither is the covered period, which
/// arrives as `time_range`.
///
/// The set is the published eight (`usage-collector-v1.yaml:440`) plus `id`,
/// which the filterable schema carries so a caller can pin one entry and so the
/// canonical cursor tiebreaker resolves, plus `window_start` and `window_end`.
/// Those last two are reserved on `$filter` but sit in
/// [`usage_collector_sdk::KEYSET_SAFE_RECORD_FIELDS`], and `window_end` must
/// resolve for the canonical `(window_end, id)` keyset to render at all.
///
/// `entry_type` resolves to the stored generated column
/// (`CASE WHEN invalidates IS NULL THEN 'record' ELSE 'invalidation' END`),
/// which is why the field is filterable here at all: the SDK stores no such
/// attribute and its value hook cannot carry one.
#[must_use]
pub fn record_column(field_name: &str) -> Option<&'static str> {
    match field_name {
        "id" => Some("id"),
        "tenant_id" => Some("tenant_id"),
        "resource_id" => Some("resource_id"),
        "resource_type" => Some("resource_type"),
        "subject_id" => Some("subject_id"),
        "subject_type" => Some("subject_type"),
        "entry_type" => Some("entry_type"),
        "origin" => Some("origin"),
        "invalidates" => Some("invalidates"),
        "window_start" => Some("window_start"),
        "window_end" => Some("window_end"),
        _ => None,
    }
}
```

**Count the arms before writing "eleven".** The doc comment on the old version
said "these nine identifiers" and was correct; a count in prose that disagrees
with the match below it is precisely the defect this plan's ground rules name.
A count can also be spelled with no numeral at all, so grep for the members
rather than for a number.

- [ ] **Step 4: Run the tests**

```bash
cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --no-fail-fast -E 'test(translate)' 2>&1 | tail -20
```

Expected: all three pass.

- [ ] **Step 5: Sweep the rest of the file's tests**

`translate_tests.rs` is 569 lines and was written against the nine-identifier
map. Some tests will name `created_at` or `status`. Fix each, and give a
**per-test verdict**: repointed to a live field, or deleted because its question
no longer exists.

- [ ] **Step 6: Prove the mutations**

Run each of the three named mutations from Step 1 under the discipline in
Task 5 Step 8 (absolute paths, `cp` snapshot, `touch`, confirm `Compiling`,
grep the mutated line). A count under a `-E` filter is a lower bound; use
`--no-fail-fast` and no filter for any number you report.

- [ ] **Step 7: Commit**

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/query/
git commit -s -m "fix(timescaledb-plugin): serve the published \$filter field set

record_column mapped nine identifiers, three of which (created_at,
corrects_id, status) slices 3 and 4 removed from the model, and was missing
five the gear needs: entry_type, origin and invalidates are published \$filter
fields, and window_start/window_end are keyset-safe order keys that must
resolve for the canonical (window_end, id) keyset to render.

A port that brought the allowlist across unchanged would answer
\$filter=origin eq 'backfill' with a 500 — a predicate the published contract
names and the gear's own guard admits.

Closes DIVERGENCES entry 16."
```

---

## Task 7: Keyset pagination over `(window_end, id)`

`keyset.rs` is structurally sound — the tuple-comparison predicate, the
fail-closed nullable check, and the kind-driven bind are all still right. What
is wrong is everything that names `created_at`, and one obligation it does not
yet meet.

**The obligation it does not meet** is the one the gear says has no compiler
backstop. `require_cursor_fingerprint` in
`usage-collector/src/domain/query.rs` says carrying `query.filter_hash` into
`next_cursor.f` is *"the one requirement in this gear's Plugin SPI that gives
an implementor no compiler error — a plugin written before it recompiles clean
and paginates exactly once"*. `encode_next_cursor` already takes a
`filter_hash` parameter; what matters is that the **caller** in
`record_store.rs` passes `query.filter_hash` through verbatim (Task 11).

**Files:**
- Modify: `src/infra/storage/query/keyset.rs`

- [ ] **Step 1: Fix every `created_at` in the docs**

```bash
cd gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
grep -n 'created_at' src/infra/storage/query/keyset.rs
```

Known hits: the module header ("The v1 gateway default order is the
all-ascending `(created_at, id)` tuple"), the `to_signed_tokens` example
(`"+created_at,+id"`), the `keyset_predicate` v1-limitation note, the
`render_order_by` example (`"created_at ASC, id ASC"`), and the
`encode_next_cursor` doc. Every one becomes `window_end`.

The module header also names `usage_type_column` in the allowlist-closure list.
That function was deleted in Task 1.

- [ ] **Step 2: Correct the mixed-direction claim**

The `keyset_predicate` doc says mixed-direction orders are a "documented
limitation". That is now stronger than a limitation and should say why it is
safe: the gateway guarantees `query.order` "uses one sort direction
throughout" (SPI doc on `list_usage_records`), so a mixed-direction order is a
**gateway breach**, not a caller-reachable case. Keep the fail-closed error;
change the reason from "unimplemented" to "cannot occur, and is refused rather
than papered over".

Do not overclaim in the other direction either. The SPI also says the two
canonical names are guaranteed **present, not last** — *"a caller ordering by
`id` is handed on as `(id, window_end)`. A plugin MUST read the order it is
given rather than assume a position for either key."* `keyset_predicate`
already iterates `order_pairs` positionally and assumes nothing, so it is
correct today; say so at the function rather than leaving it to be rediscovered.

- [ ] **Step 3: Add the test that pins position-independence**

```rust
#[test]
fn the_predicate_follows_the_order_it_is_given_rather_than_a_canonical_position() {
    let mut ctx = SqlCtx::new(1);
    let sql = keyset_predicate(
        &[("id", true), ("window_end", true)],
        &[
            "3f2504e0-4f89-11d3-9a0c-0305e82c3301".to_owned(),
            "2026-09-09T00:00:00Z".to_owned(),
        ],
        record_column,
        field_kind,
        usage_collector_sdk::is_keyset_safe_record_field,
        &mut ctx,
    )
    .expect("an id-led order is admissible; the SPI guarantees presence, not position");

    assert_eq!(sql, "(id, window_end) > ($1, $2)");
}
```

**The mutation:** make `keyset_predicate` sort `order_pairs` so `window_end`
leads, or hard-code the canonical order. Either makes this red.

Confirm the helper names (`field_kind`, `is_keyset_safe_record_field`) against
the crate and SDK before writing — this test names four things it does not
define.

- [ ] **Step 4: Run and verify**

```bash
cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --no-fail-fast -E 'test(keyset)' 2>&1 | tail -20
```

- [ ] **Step 5: Commit**

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/query/keyset.rs
git commit -s -m "refactor(timescaledb-plugin): key pagination on the covered-period end

The canonical keyset is (window_end, id); created_at is gone. Corrects every
doc naming it, and pins that the predicate follows the order it is handed
rather than assuming a position for either canonical key — the SPI guarantees
both are present, not that either is last."
```

---

## Task 8: Aggregation over the current fold and withdrawal rules

Two semantic changes here, and the second inverts the existing behaviour.

**`AggregationOp` became `AggregationFold`**: `Avg` is gone, `Latest` is new.
Verify against
`gears/system/usage-collector/usage-collector-sdk/src/models.rs` before
writing — the measured variants are `Sum`, `Count`, `Max`, `Min`, `Latest`.

**Withdrawal exclusion replaced compensation netting, and the old rule was the
opposite of the new one.** `corrects_id_partition_clause` implements "SUM nets
across compensations; every other op filters `corrects_id IS NULL`". Under the
append-only model an invalidation **echoes** the quantity it withdraws rather
than negating it, so netting double-counts. The SPI states two obligations that
hold under *every* fold:

> 1. An **invalidation entry** contributes nothing to any fold, whether or not
>    its target is in the selection.
> 2. A **record an accepted invalidation names** contributes nothing either.

Obligation 1 standing alone is not pedantry: retention is plugin-owned, so a
conforming deployment can purge a target and keep the invalidation that
withdrew it. That orphan still contributes nothing.

**Files:**
- Modify: `src/infra/storage/query/aggregate.rs`, `src/infra/storage/query/aggregate_tests.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn every_fold_excludes_both_halves_of_a_withdrawn_pair() {
    for fold in [
        AggregationFold::Sum,
        AggregationFold::Count,
        AggregationFold::Max,
        AggregationFold::Min,
        AggregationFold::Latest,
    ] {
        let clause = withdrawal_exclusion_clause();
        assert!(
            clause.contains("invalidates IS NULL"),
            "an invalidation entry contributes nothing to {fold:?}, whether or \
             not its target is in the selection"
        );
        assert!(
            clause.contains("NOT EXISTS"),
            "an entry an accepted invalidation names contributes nothing to \
             {fold:?} either; leaving out only the invalidation double-counts \
             the measurement the withdrawal was meant to remove"
        );
    }
}
```

The loop is doing real work here and is not decoration: the old rule made `SUM`
the exception, so a clause that reintroduces a per-fold branch is exactly what
this catches.

**The mutation:** make `withdrawal_exclusion_clause` return `None`/empty for
`Sum`, restoring the netting behaviour.

- [ ] **Step 2: Replace `corrects_id_partition_clause`**

Delete it. In its place:

```rust
/// The two withdrawal-exclusion obligations, as one `WHERE` fragment.
///
/// A withdrawn pair contributes nothing to any fold
/// (`cpt-cf-usage-collector-adr-append-only-invalidation`), and this is two
/// obligations rather than one conditional:
///
/// 1. An invalidation entry contributes nothing, whether or not its target is
///    in the selection — `invalidates IS NULL`.
/// 2. An entry an accepted invalidation names contributes nothing either —
///    the `NOT EXISTS` correlated subquery.
///
/// Both hold under every fold, with no per-fold branch. That is a change of
/// rule and not only of spelling: the retired `corrects_id` model had `SUM`
/// net across signed compensation rows, so it deliberately did *not* filter
/// them. An invalidation echoes the quantity it withdraws rather than negating
/// it, so netting would now double-count.
///
/// Obligation 1 standing alone matters because retention is plugin-owned
/// (DESIGN §3.10): a conforming deployment can purge a target and keep the
/// invalidation that withdrew it. That orphan still contributes nothing.
///
/// The returned string is a `'static` constant, never caller text. `r` is the
/// outer query's alias for `usage_records`.
#[must_use]
pub fn withdrawal_exclusion_clause() -> &'static str {
    "r.invalidates IS NULL \
     AND NOT EXISTS (SELECT 1 FROM usage_records w \
                     WHERE w.invalidates = r.id)"
}
```

The caller must alias the table as `r` for this to bind. Make that a documented
precondition, and grep the call sites in Task 12 to confirm it holds.

- [ ] **Step 3: Rewrite `agg_select_expr` for the new fold set**

```rust
/// SQL aggregate expression for an [`AggregationFold`].
///
/// Every fold casts to `numeric` so the result — including the integer-typed
/// `COUNT(*)` — reads back uniformly as `Option<BigDecimal>`. Reading into
/// arbitrary-precision `bigdecimal::BigDecimal` (the SDK's
/// `AggregationBucket.value` type) is why a wide `SUM` no longer hits
/// `rust_decimal::Decimal`'s ~7.9×10²⁸ ceiling and turns into a 500 on decode.
///
/// [`AggregationFold::Latest`] is absent: it is not an aggregate function but
/// an ordered pick, and [`latest_select_expr`] renders it.
#[must_use]
pub fn agg_select_expr(fold: AggregationFold) -> Option<&'static str> {
    match fold {
        AggregationFold::Sum => Some("SUM(r.value)::numeric"),
        AggregationFold::Count => Some("COUNT(*)::numeric"),
        AggregationFold::Min => Some("MIN(r.value)::numeric"),
        AggregationFold::Max => Some("MAX(r.value)::numeric"),
        AggregationFold::Latest => None,
    }
}
```

`Avg` and its `ROUND(…, 6)` scale cap are deleted along with the variant. **Do
not leave the module-header paragraph explaining the rounding scale** — it
would be a doc explaining a mechanism that no longer exists, which is this
repository's characteristic defect.

- [ ] **Step 4: Implement `LATEST` with the declared tie-break**

```rust
/// SQL expression picking the [`AggregationFold::Latest`] quantity.
///
/// DESIGN §3.1 declares the rule as *greatest `window_end`, then greatest
/// `acceptance_sequence`*, and it terminates because the sequence is strictly
/// monotonic inside the group's scope.
///
/// This backend can implement the declared rule exactly, because it assigns
/// and stores `acceptance_sequence` itself (DESIGN §3.7). That is worth
/// stating because the SDK's own reference backend cannot: `UsageRecord`
/// carries no such field, so `InMemoryReferencePlugin` substitutes the
/// greatest `id`, and the `latest-tie-break` contract check is blocked for the
/// same reason (DIVERGENCES entries 10 and 19). A green contract run therefore
/// says nothing about this expression in either direction — nothing asserts it.
#[must_use]
pub fn latest_select_expr() -> &'static str {
    "(ARRAY_AGG(r.value ORDER BY r.window_end DESC, r.acceptance_sequence DESC))[1]::numeric"
}
```

`ARRAY_AGG(… ORDER BY …)[1]` is used rather than `DISTINCT ON` because it
composes with `GROUP BY` over arbitrary dimensions, which `DISTINCT ON` does
not. Note that in the doc; a reader will otherwise reach for `DISTINCT ON`.

- [ ] **Step 5: Add `Origin` to the dimension match — or confirm it is absent**

`dimension_select_expr` matches `AggregationDimension`. **DIVERGENCES entry 15
records that `AggregationDimension` carries five of the eight fixed dimensions
DESIGN gives `group_by`, and growing it is explicitly out of scope for this
slice.** So the match arms stay as they are: `TenantId`, `ResourceId`,
`ResourceType`, `SubjectId`, `SubjectType`, `Metadata`.

Alias every column with `r.` to match the exclusion clause. Confirm the variant
list against the SDK rather than trusting this plan.

**Do not add an `Origin` arm.** If the enum has grown one since this plan was
written, stop and report it — that is entry 15 moving, and it is not yours.

- [ ] **Step 6: Note the absent-dimension question without deciding it**

DIVERGENCES §G records that the reference backend **drops** a row with no
`subject_ref` from a `GROUP BY subject_id`, where naive SQL collects them into
a NULL group — so an exemplar backend and a SQL projection give different sums
for the same ledger, with nothing failing.

**§G says explicitly: do not write the check first, because a check pins
whichever answer its author picked, and here that would be a decision made by a
test rather than by the contract.**

So: in SQL, `GROUP BY subject_id` over a NULL column produces a NULL group, and
`dimension_select_expr` returns a bare column. That means this backend will
**collect** them where the reference backend **drops** them. Do not silently
pick either. Add a doc comment at `dimension_select_expr` recording that the
two disagree, naming §G, and saying the answer is a spec owner's. Report it in
the task summary so it reaches the DIVERGENCES update in Task 18.

- [ ] **Step 7: Run and commit**

```bash
cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --no-fail-fast -E 'test(aggregate)' 2>&1 | tail -20
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/query/aggregate.rs \
        gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/query/aggregate_tests.rs
git commit -s -m "feat(timescaledb-plugin)!: fold under the append-only withdrawal rules

Replaces the corrects_id partition with the SPI's two withdrawal-exclusion
obligations, which hold under every fold. This inverts the old rule rather
than respelling it: the retired model had SUM net across signed compensation
rows, and an invalidation echoes the quantity it withdraws rather than
negating it, so netting would double-count.

Adds LATEST on the declared tie-break (greatest window_end, then greatest
acceptance_sequence), which this backend can implement exactly because it
assigns the sequence itself. Drops AVG with its variant.

BREAKING CHANGE: AggregationOp::Avg is no longer served; the fold set is
Sum, Count, Max, Min, Latest."
```

---

## Task 9: Persist an entry — `create` and `create_batch`

The largest task. `record_store.rs` is 1,401 lines and this is the half that
writes.

Three obligations land here, and one of them cannot be met by a read followed
by a write.

**Files:**
- Modify: `src/infra/storage/record_store.rs`, `src/infra/storage/record_store_tests.rs`

### Obligation 1 — the dedup identity is the 5-tuple

The old dedup key was the 4-tuple `(tenant_id, gts_id, idempotency_key,
created_at)`. It is now `(tenant_id, gts_type_id, idempotency_key,
window_start, window_end)`. `dedup_key`, `row_dedup_key` and `canonical_equal`
all encode the old shape.

### Obligation 2 — at most one invalidation, atomically

The SPI is unusually explicit, and explains why the gateway will not help:

> Where the entry carries an [`UsageRecord::invalidation`], the store MUST
> reject it if the record it names already has an accepted invalidation, and
> MUST make that check atomic with the entry it admits — **one backend
> transaction, not a read followed by a write.** … The gateway does not
> pre-read for this and will not: a gateway-side check cannot exclude a
> concurrent second submission, so it would fail exactly when it matters.

The Task 3 partial unique index is the mechanism. The store's job is to catch
its violation and translate it, not to pre-read.

> a batch is where it is easiest to get wrong: two withdrawals of one record
> can arrive in the same call, so admitting them one at a time against the
> state each read is not enough.

The index handles the in-batch case too **only if the batch inserts in one
statement or one transaction**. Confirm which `insert_records_on_conflict`
does before relying on it.

### Obligation 3 — assign `acceptance_sequence`

Claim from `usage_acceptance_sequence` in the same transaction as the insert.

### You are adding the first transaction in the crate

**Measured after Task 2: the plugin opens no transactions at all.** `deactivate`
held the store's only one, and its `sqlx::Connection` import went dead when Task
2 removed it — `grep -rn 'begin()\|Transaction'` over `src/` returns nothing.

That matters because both obligations above are transactional and the SPI is
explicit that a read-then-write will not do:

> the store MUST reject it if the record it names already has an accepted
> invalidation, and MUST make that check atomic with the entry it admits —
> **one backend transaction, not a read followed by a write.**

So Step 4's `claim_acceptance_sequence(tx: &mut sqlx::Transaction<'_, Postgres>, …)`
is not slotting into existing machinery; you are re-introducing it. Expect to
restore the `sqlx::Connection` (or `sqlx::Acquire`) import, and expect the
single-row and batch insert paths to change shape rather than gain a parameter.

Do not read this as "the crate used to have a transaction, so this is a
revert" — the one Task 2 deleted wrapped a `SELECT … FOR UPDATE`
read-modify-write, which is the shape the SPI forbids here.

- [ ] **Step 1: Write the failing tests**

These are unit tests over the SQL and the key derivation; the behavioural
proof is the contract suite in Task 14 and the pg integration tests in Task 15.

```rust
#[test]
fn the_dedup_key_is_the_five_tuple() {
    let base = sample_record();
    let shifted = UsageRecord {
        window_start: base.window_start - time::Duration::hours(1),
        ..base.clone()
    };

    assert_ne!(
        dedup_key(&base),
        dedup_key(&shifted),
        "window_start is one of the five dedup-identity inputs, so two entries \
         differing only in it are distinct entries, not a retry"
    );
}

#[test]
fn a_unique_violation_on_the_invalidation_index_reads_as_already_invalidated() {
    let err = map_insert_error(
        &pg_unique_violation("usage_records_one_invalidation_uniq"),
        &sample_invalidation_record(),
    );

    assert!(
        matches!(err, UsageCollectorPluginError::AlreadyInvalidated { .. }),
        "the partial unique index is how at-most-one is enforced atomically; \
         its violation is the caller-visible rejection, not an Internal. got {err:?}"
    );
}
```

**The mutations:** drop `window_start` from `dedup_key` (test 1 red); route the
`usage_records_one_invalidation_uniq` constraint name to `Internal` (test 2
red).

`pg_unique_violation` needs to build an `sqlx::Error::Database` carrying a
constraint name. Check how `error_tests.rs` already constructs one — the crate
has this problem solved somewhere, and inventing a second way is worse than
finding the first.

- [ ] **Step 2: Update `RECORD_COLUMNS`**

At `:65`. It must match `UsageRecordRow`'s field order from Task 4 exactly —
the store decodes positionally.

```rust
const RECORD_COLUMNS: &str = "id, tenant_id, gts_type_id, value, window_start, \
     window_end, resource_id, resource_type, subject_id, subject_type, \
     idempotency_key, invalidates, reason_code, origin, acceptance_sequence, \
     metadata, ingested_at";
```

**Seventeen columns, and `entry_type` is deliberately not among them** — it is
generated, and nothing decodes it. Count the names against `UsageRecordRow`'s
fields one by one; a positional decode that is one column out fails at runtime
with a type error that names neither column.

- [ ] **Step 3: Rewrite the dedup helpers**

`dedup_key`, `row_dedup_key` and `canonical_equal` (at `:627`, `:638`, `:884`)
all carry the 4-tuple. Bring each to the 5-tuple. `canonical_equal` compares a
submitted record against a stored row to decide absorb-vs-conflict; it must now
compare the covered-period pair, `origin`, and the invalidation pair too — an
exact-equality retry means every caller-supplied field matches.

Check what `to_micros` (`:832`) is for: it normalizes an `OffsetDateTime` for
comparison. It now has two timestamps to normalize per record, not one.

- [ ] **Step 4: Claim the acceptance sequence**

Add to `impl PgRecordStore`:

```rust
/// Claim the next `acceptance_sequence` for `(tenant_id, gts_type_id)`.
///
/// Strictly monotonic per scope, which is the DESIGN §3.7 obligation. It is
/// **not** gapless, and does not need to be: an insert that is absorbed as an
/// idempotent retry consumes a value it never stores. Density is not the
/// obligation and nothing reads the sequence expecting it.
///
/// Runs inside the caller's transaction so the claim and the insert commit or
/// roll back together. The row lock this takes serializes concurrent ingest
/// for one scope, which is what per-scope monotonicity costs; scopes do not
/// contend with each other.
async fn claim_acceptance_sequence(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    tenant_id: Uuid,
    gts_type_id: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO usage_acceptance_sequence (tenant_id, gts_type_id, next_value) \
         VALUES ($1, $2, 1) \
         ON CONFLICT (tenant_id, gts_type_id) \
         DO UPDATE SET next_value = usage_acceptance_sequence.next_value + 1 \
         RETURNING next_value",
    )
    .bind(tenant_id)
    .bind(gts_type_id)
    .fetch_one(&mut **tx)
    .await
}
```

- [ ] **Step 5: Rewrite the insert**

`create_inner` (`:197`) and `insert_records_on_conflict` (`:312`) carry the
`ON CONFLICT (tenant_id, gts_id, idempotency_key, created_at) DO NOTHING`
dedup authority. The conflict target becomes the 5-tuple, matching the Task 3
constraint. The column list and the bind order become the seventeen above,
minus `ingested_at` (defaulted) and `acceptance_sequence` (claimed per Step 4).

Both must run inside a transaction that also holds the sequence claim.

- [ ] **Step 6: Translate the new constraint violation**

**First, a stale doc this task inherits.** `map_insert_error`'s doc comment
describes `UsageTypeNotFound`, an SDK error variant that no longer exists, and
the code below it still constructs one. Doc and code are stale together, which
is why Task 1 left both. Rewrite the doc to match what you implement here
rather than carrying the sentence forward.

`map_insert_error` (`:128`) maps a unique violation to `IdempotencyConflict`.
It must now discriminate on the constraint name:

- `usage_records_dedup_uniq` -> the existing absorb-or-conflict path
- `usage_records_one_invalidation_uniq` -> `AlreadyInvalidated`, naming the
  invalidation already in place

`AlreadyInvalidated`'s fields need reading before you populate them:

```bash
sed -n '/AlreadyInvalidated/,/}/p' \
  gears/system/usage-collector/usage-collector-sdk/src/error.rs
```

If naming the existing invalidation requires a read, that read happens *after*
the constraint has already rejected the write — so it is diagnostic, not a
check, and the atomicity obligation is still met by the index. Say that at the
call site, because it looks like the pre-read the SPI forbids and is not one.

- [ ] **Step 6b: Settle every transaction claim in this file, and the 55P03 gap**

**Eight sites, not two.** An earlier draft named `:778` and `:940`; Task 2's
code review found three more, and a re-count found a further one. Measured
with `grep -n 'transaction' src/infra/storage/record_store.rs`:

| Line | Says | Status |
| --- | --- | --- |
| `:189` | "no explicit transaction is needed" | true today, **you make it false** |
| `:536` | "atomic, so no explicit transaction is required" | true today, **you make it false** |
| `:717` | "the surviving transaction has already committed or aborted" | defensible — true of the server's implicit transaction |
| `:750` | "the whole transaction rolled back" | defensible, same reason |
| `:775` | "…transaction. `operation` is an `Fn` invoked fresh each attempt" | check in context |
| `:778` | "opens a fresh transaction" | **false about the plugin's code** |
| `:940` | "opens a fresh transaction (`create_batch_inner` does both)" | **false about the plugin's code** |
| `:942` | "transaction is atomic and the dedup keys make it idempotent" | defensible |

The split matters: `:717`, `:750` and `:942` read as true of *Postgres*, which
wraps every statement in an implicit transaction, whereas `:778` and `:940` are
false about *this crate*, which after Task 2 opens none. **Decide all eight
deliberately** — do not fix the two obvious ones and leave six lookalikes, which
is how this file got into its current state. Report a per-line verdict.

Note the direction of travel: `:189` and `:536` are correct *now* and your work
makes them wrong, so they need rewriting too even though they read fine today.

**`pool.rs:70` is the sibling case and is also yours:** "the same dedup
4-tuple", correct until this task moves identity to the 5-tuple.

**The 55P03 decision, which no other task owns.** `is_transient_sqlstate`
(`error.rs:18-24`) matches `08*`, `57P01`, `57P02`, `57P03`, `53300`, `40001`,
`40P01`. **`55P03 lock_not_available` is absent**, so a statement that hits
`LOCK_TIMEOUT` falls to `Other` → `Internal` → non-retryable, and
`is_retryable_batch_error` (`:757`) will not retry it.

That is pre-existing, but Task 2 made it visible by electing the ingest
`ON CONFLICT` path as `LOCK_TIMEOUT`'s worked example — so an inherently
transient wait is now *documented* as failing into a non-retryable bucket.
You own the retry path, so you own this call. **Either add `55P03` to the
transient set, or write down why `Internal` is the intended answer.** Silence
is the one outcome that is not acceptable, because the next reader will assume
the omission was considered.

- [ ] **Step 7: Handle the in-batch collision**

Two invalidations of one target in one `create_batch` call. Verify what
happens: if the batch is one multi-row `INSERT`, the index rejects the whole
statement rather than one row, which is wrong — the SPI wants exactly one
accepted and the other rejected, with per-record outcomes aligned to input
order.

If that is what happens, the fix is `plan_batch` (`:693`) detecting a
duplicate `invalidates` target within the batch and pre-rejecting all but the
first, **in addition to** the index, which still covers the cross-call case.
Document that the in-batch check is not the enforcement — the index is — so
nobody later deletes the index as redundant.

Write a test for the in-batch case naming its mutation.

- [ ] **Step 8: Run, verify, commit**

```bash
cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --no-fail-fast -E 'test(record_store)' 2>&1 | tail -30
```

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/record_store.rs \
        gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/record_store_tests.rs
git commit -s -m "feat(timescaledb-plugin)!: persist entries under the 5-tuple identity

Moves the dedup authority from the 4-tuple to the 5-tuple DESIGN 3.7 requires,
assigns acceptance_sequence monotonically per (tenant_id, gts_type_id) inside
the insert transaction, and enforces at-most-one-invalidation with a partial
unique index rather than a read followed by a write — the SPI requires the
check be atomic with the entry it admits, and says why a gateway-side pre-read
cannot substitute.

BREAKING CHANGE: the dedup identity now includes the covered period, so an
entry that differs only in window_start is a distinct entry rather than a
retry."
```

---

## Task 10: `get_usage_record` intersects the compiled scope

The SPI signature grew a parameter and it is not cosmetic. Slice 3 retired the
gateway's in-process per-record attribution check, so **"exists but not yours
reads as `NotFound`" now rests entirely on the plugin intersecting the filter
it is handed.** Slice 6 wrote the contract check that proves it
(`SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH`).

The SPI:

> `scope` is the caller's compiled PDP scope … The point lookup carries no
> caller-supplied filter of its own, so `scope` is the *whole* filter the row
> must satisfy — a row whose attribution tuple falls outside it MUST NOT be
> returned; the plugin reports `UsageRecordNotFound` exactly as it would for an
> `id` that does not exist at all.

And the obligation that pulls the other way:

> A withdrawn pair MUST be returned **as persisted** … a plugin MUST NOT
> withhold a withdrawn entry from it as a kindness.

**Files:**
- Modify: `src/domain/ports.rs`, `src/infra/storage/record_store.rs`, `src/infra/storage/record_store_tests.rs`

- [ ] **Step 1: Change the port signature**

```rust
    async fn get(
        &self,
        id: Uuid,
        scope: &ast::Expr,
    ) -> Result<UsageRecord, UsageCollectorPluginError>;
```

- [ ] **Step 2: Write the failing test**

```rust
#[test]
fn the_point_lookup_renders_the_scope_as_its_whole_where_clause() {
    let mut ctx = SqlCtx::new(2);
    let scope = parse_scope("tenant_id eq 11111111-1111-1111-1111-111111111111");

    let sql = build_get_sql(&scope, &mut ctx).expect("a scope must render");

    assert!(
        sql.contains("WHERE id = $1 AND ("),
        "the point lookup carries no caller filter, so the compiled scope is \
         the whole filter the row must satisfy; a lookup that selects on id \
         alone is an existence oracle. got: {sql}"
    );
}
```

**The mutation:** drop the scope conjunct from `build_get_sql`, leaving
`WHERE id = $1`. That single edit is exactly the defect slice 3 created the
obligation to prevent, and it must make this red.

- [ ] **Step 3: Implement**

`get` is at `:982` and builds `SELECT {RECORD_COLUMNS} FROM usage_records WHERE
id = $1`. It must now conjoin the translated scope. `translate.rs` already has
the filter-AST-to-SQL machinery `list` uses; reuse it — a second translator is
two implementations of one security boundary.

A row outside the scope returns `UsageCollectorPluginError::UsageRecordNotFound
{ id }`, identical to a missing row. Do not add a distinguishing log line at a
level a caller could observe through timing or volume; the point is that the
two cases are indistinguishable.

- [ ] **Step 4: Do not filter withdrawn entries here**

The ledger obligation is explicit and points the opposite way from the fold's.
Add a comment at the query saying no `invalidates` predicate belongs here and
why — a reader who has just written Task 8's exclusion clause will otherwise
add one, and it would destroy the audit trail the append-only model exists to
keep.

- [ ] **Step 5: Update the adapter**

```rust
    async fn get_usage_record(
        &self,
        id: Uuid,
        scope: &ast::Expr,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        self.record.get(id, scope).await
    }
```

- [ ] **Step 6: Run and commit**

```bash
cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --no-fail-fast -E 'test(record_store)' 2>&1 | tail -20
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/
git commit -s -m "feat(timescaledb-plugin)!: intersect the compiled scope on the point lookup

Slice 3 retired the gateway's in-process per-record attribution check, so
'exists but not yours reads as NotFound' now rests entirely on the plugin
intersecting the filter it is handed. The point lookup carries no caller
filter, so the scope is the whole WHERE clause beyond the id.

A withdrawn entry is still returned as persisted: that is a ledger path, and
the exclusion belongs to the fold.

BREAKING CHANGE: RecordStore::get takes the compiled scope."
```

---

## Task 11: `list_usage_records` — time range, order, cursor

**Files:**
- Modify: `src/domain/ports.rs`, `src/infra/storage/record_store.rs`, `src/infra/storage/record_store_tests.rs`

### The three obligations

**Selection reads the period end alone.** `from <= window_end < to`
(`cpt-cf-usage-collector-adr-window-end-selection`). Not overlap, not
containment — those make adjacent ranges double count or drop entries. **No
selection predicate reads `window_start`.** DIVERGENCES §F notes that a backend
selecting on `window_start` fails both `window-end-selection` **and**
`quantity-round-trip`, because the latter's read-back range is
`[window_end, window_end + 1s)` while each fixture's `window_start` sits an hour
earlier. **If both go red, diagnose the period rule, not the decimals.**

**`query.order` MUST be honoured** — it is the keyset the continuation is built
from. `time_range` is a typed parameter and never appears in `query.filter`.

**`query.filter_hash` is guaranteed on this method** and MUST be carried into
`next_cursor.f` verbatim. This is the obligation with no compiler backstop: a
plugin that drops it recompiles clean and paginates exactly once.

- [ ] **Step 1: Change the port signature**

```rust
    async fn list(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError>;
```

- [ ] **Step 2: Write the failing tests**

```rust
#[test]
fn selection_reads_the_period_end_alone() {
    let mut ctx = SqlCtx::new(1);
    let sql = build_time_range_clause(sample_range(), &mut ctx);

    assert!(
        sql.contains("window_end >= $") && sql.contains("window_end < $"),
        "selection is `from <= window_end < to` on every path: it needs no case \
         for a point event and must not match by overlap or containment. got: {sql}"
    );
    assert!(
        !sql.contains("window_start"),
        "no selection predicate reads window_start; a backend that selects on \
         it fails window-end-selection AND quantity-round-trip (DIVERGENCES F). \
         got: {sql}"
    );
}

#[test]
fn the_minted_cursor_carries_the_gateways_filter_hash_verbatim() {
    let hash = "e3b0c44298fc1c14";
    let token = mint_next_cursor(&canonical_order(), &sample_row_keys(), Some(hash))
        .expect("a page ending mid-range mints a cursor");

    let decoded = toolkit_odata::CursorV1::decode(&token).expect("round-trips");
    assert_eq!(
        decoded.f.as_deref(),
        Some(hash),
        "the gateway recomputes this from the follow-up request and refuses a \
         token carrying a different one, or none, with FilterMismatch. This is \
         the one SPI requirement with no compiler backstop: a plugin that drops \
         it recompiles clean and paginates exactly once."
    );
}
```

**The mutations:** change `>=` to `>` on the lower bound, or add a
`window_start` conjunct (test 1 red); pass `None` for the filter hash at the
mint site (test 2 red).

Test 2's mutation is the important one. Name it out loud in the task report.

- [ ] **Step 3: Implement**

Rewrite `list` (`:1028`). The shape:

```sql
SELECT {RECORD_COLUMNS} FROM usage_records r
WHERE r.gts_type_id = $1
  AND r.window_end >= $2 AND r.window_end < $3
  [AND <translated $filter>]
  [AND <metadata filter clauses>]
  [AND <keyset predicate from the cursor>]
ORDER BY <render_order_by(query.order)>
LIMIT <clamped limit + 1>
```

No withdrawal exclusion. This is a ledger path.

Mint `next_cursor` from the last in-page row's key values in `query.order`
field order, passing `query.filter_hash` through verbatim. `encode_next_cursor`
already takes the parameter — the defect this task guards against is passing
`None` at the call site, so read the call, not the signature.

An empty `query.order` or an absent `query.filter_hash` is a **gateway breach**,
not a case to paper over. Fail loudly.

- [ ] **Step 4: Run, prove the mutations, commit**

```bash
cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --no-fail-fast -E 'test(record_store)' 2>&1 | tail -30
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/
git commit -s -m "feat(timescaledb-plugin)!: page the ledger on the covered-period end

Selection is from <= window_end < to on every path, reading the period end
alone: no case for a point event, and never overlap or containment. Honours
query.order as the keyset it is handed and carries query.filter_hash into
next_cursor.f verbatim — the one SPI requirement with no compiler backstop.

Withdrawn entries are returned as persisted; this is a ledger path.

BREAKING CHANGE: RecordStore::list takes the meter and time range as typed
parameters."
```

---

## Task 12: `query_aggregated_usage_records`

**Files:**
- Modify: `src/domain/ports.rs`, `src/infra/storage/record_store.rs`

- [ ] **Step 1: Change the port signature**

```rust
    async fn aggregate(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError>;
```

- [ ] **Step 1b: Two things Task 2's review handed to this task**

**A live `status = 'active'` clause now has zero test coverage.** `aggregate`
still applies it unconditionally (`record_store.rs:1222`, documented at
`:1172-1182`), and the retired time model's `status` column still exists in the
migration and in `mapper.rs`. Task 2 deleted `pg_aggregate_excludes_inactive`,
which was the only test exercising it — correctly, because `deactivate` was the
only writer of `inactive` and the SDK never let a caller set it, so the test
could no longer be set up. But that leaves the clause live and unguarded.

**This task is where it goes.** The withdrawal-exclusion clause from Task 8
replaces it outright: `status` is not a column in the Task 3 schema, so
`status = 'active'` must be gone from the assembled SQL. Grep the built query
in a test and assert `status` does not appear in it.

**`UsageRecordStatus` survives in five files, not the two an earlier note
claimed** — `mapper.rs`, `mapper_tests.rs`, `record_store_tests.rs`,
`tests/common/mod.rs`, `tests/records_ingest_integration_pg.rs`. Task 5 removes
it from the mapper; the test files are Task 15's. Neither count is this task's
to fix, but do not be surprised by the remainder.

- [ ] **Step 2: Rewrite `aggregate`**

At `:1204`. The shape:

```sql
SELECT <dimension exprs…>, <fold expr>
FROM usage_records r
WHERE r.gts_type_id = $1
  AND r.window_end >= $2 AND r.window_end < $3
  AND <withdrawal_exclusion_clause()>
  [AND <translated $filter>]
  [AND <metadata filter clauses>]
[GROUP BY 1, 2, …]
[LIMIT MAX_AGGREGATION_BUCKETS + 1]
```

The table **must** be aliased `r` — `withdrawal_exclusion_clause` binds to it.
Grep the assembled SQL in a test rather than assuming.

- [ ] **Step 3: Get the no-grouping case right**

The noop plugin's doc records the trap precisely: an empty `buckets` vector is
**not** the shape a conforming plugin answers with. The no-grouping case is a
**single bucket carrying an empty `key`**, whose value is absent for every fold
but `COUNT`.

`COUNT` over an empty selection is `Some(0)`, not `None` — "counting an empty
selection is zero rather than absent, the same split `SELECT COUNT(*)` makes
against `SELECT MIN(v)` over no rows". Postgres already does this; the risk is
a hand-written empty-result short circuit that does not.

Write a test for the empty-selection `COUNT` case naming its mutation.

- [ ] **Step 4: `MUST NOT` read `filter_hash` here**

The SPI is explicit: this method paginates nothing, mints no cursor, and the
gateway assigns it no fingerprint. **"An aggregate implementation MUST NOT read
the slot."** Confirm the implementation does not, and say so in a comment.

- [ ] **Step 5: Update the adapter, run, commit**

```rust
    async fn query_aggregated_usage_records(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        self.record
            .aggregate(gts_type_id, time_range, fold, query, metadata_filter, group_by)
            .await
    }
```

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/
git commit -s -m "feat(timescaledb-plugin)!: fold over the typed meter, range and group_by

The fold, meter, time range and group_by all arrive as typed parameters;
declarations never reach the SPI. Applies the two withdrawal-exclusion
obligations and answers the no-grouping case as a single bucket with an empty
key rather than an empty bucket list.

BREAKING CHANGE: RecordStore::aggregate takes AggregationFold and group_by in
place of AggregationSpec."
```

---

## Task 13: Compile — metrics, config, gear, lib

The first task whose success criterion is `cargo check` passing.

**Files:**
- Modify: `src/infra/metrics.rs`, `src/infra/metrics_tests.rs`, `src/config.rs`, `src/gear.rs`, `src/lib.rs`, `src/gear_tests.rs`

- [ ] **Step 1: Bring the metric inventory to DESIGN §3.11.5**

`src/infra/metrics.rs` is 486 lines and carries catalog-labelled metrics and
correction-era operation labels. **DESIGN §3.11.5 "Operational Metric
Inventory" is at lines 1767-1824** — read it and reconcile.

DIVERGENCES entries 4, 9, 13 and 14 all concern metric labels and **none of
them is yours**. Entry 4 (`uc_query_requests_total` lists a label that cannot
fire), entry 9 (`uc_ingestion_records_total`'s label row), entry 13
(`uc_pdp_duration_seconds`) and entry 14 (`docs/features/usage-emission.md`)
are gear-side or doc-side. Change only what this plugin emits.

- [ ] **Step 2: Fix the remaining compile errors**

```bash
cd gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
cargo check -p cf-gears-timescaledb-usage-collector-plugin --all-targets 2>&1 | tail -60
```

Work to zero. The expected remainder after Tasks 1-12 is imports, the gear
wiring, and `src/lib.rs`'s module doc (which describes a `domain` layer holding
"the SPI adapter and store port traits" — plural, and there is one now).

- [ ] **Step 3: `cargo check` must pass**

```bash
cargo check -p cf-gears-timescaledb-usage-collector-plugin --all-targets
```

Expected: exits 0. **This is the first time since slice 4.**

- [ ] **Step 4: Clippy**

```bash
cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings
```

Expected: exits 0. `clippy::pedantic` is deny at workspace level. Watch for
`clippy::non_ascii_literal` — no em dashes inside string literals.

- [ ] **Step 5: Unit tests**

```bash
cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --no-fail-fast 2>&1 | tail -10
```

Report `N passed / M skipped` unfiltered. A count under an `-E` filter is a
lower bound and is not the number to report.

- [ ] **Step 6: Commit**

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/
git commit -s -m "feat(timescaledb-plugin): compile against the reshaped SPI

Brings the metric inventory, config, gear wiring and crate docs to the model
the preceding commits established. The crate compiles for the first time since
slice 4 removed UsageRecordStatus and corrects_id from the SDK."
```

---

## Task 14: The acceptance criterion — run the contract suite

**Files:**
- Create: `tests/contract_conformance_pg.rs`
- Modify: `Cargo.toml`

### Read this before relying on a green run

`contract.rs`'s module header and DIVERGENCES §F both say a green run is **not**
the same as being a conforming plugin. Specifically:

- **Five of DESIGN §3.3's seven checks are written.** `feed-snapshot-and-replay`
  is blocked on the feed (out of scope, entry 18); `latest-tie-break` is blocked
  because `UsageRecord` carries no `acceptance_sequence` (entry 19, and the
  owner's decision keeps it that way). `run_all` also runs
  `SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH`, which DESIGN states as an obligation
  without tabulating.
- **Nothing in the suite exercises the SPI's keyset obligations.** The reference
  backend serves the canonical order, ignores `query.order`, and mints no
  `next_cursor` — and a real plugin owes all three. Task 11's cursor-fingerprint
  test and Task 15's pg tests are what cover it here.
- **Two checks are coupled and the coupling points the wrong way.** A backend
  selecting on `window_start` fails both `window-end-selection` and
  `quantity-round-trip`. Both red means diagnose the period rule.

**When reporting coverage, report `IMPLEMENTED_CHECKS`, `UNWRITTEN_CHECKS`,
`BLOCKED_CHECKS` and `ADDITIONAL_CHECKS` together.** `IMPLEMENTED_CHECKS` alone
under-reports what `run_all` ran.

- [ ] **Step 1: Add the dev-dependency**

In `Cargo.toml` under `[dev-dependencies]`:

```toml
cf-gears-usage-collector-sdk = { workspace = true, features = ["contract"] }
```

The crate already depends on `usage-collector-sdk` under that workspace alias;
confirm whether the alias or the package name is the right key here by matching
how another crate in the workspace enables a feature on an aliased dependency.

- [ ] **Step 2: Write the test**

`tests/contract_conformance_pg.rs`:

```rust
#![cfg(feature = "postgres")]

//! The DESIGN §3.3 plugin contract suite, run against a live TimescaleDB
//! backend.
//!
//! This is the acceptance criterion for the port. It is **not** a conformance
//! certificate: the suite writes five of DESIGN's seven checks plus one
//! DESIGN states without tabulating, and nothing in it exercises the SPI's
//! keyset obligations. See `contract.rs`'s module header and DIVERGENCES §F.

use usage_collector_sdk::contract;

mod common;

#[tokio::test]
async fn the_timescale_backend_conforms() {
    let (_container, backend) = common::start_backend().await;

    let violations = contract::run_all(&backend).await;

    assert!(
        violations.is_empty(),
        "plugin contract violations ({} of DESIGN §3.3's seven checks are \
         written; {:?} are blocked): {violations:#?}",
        contract::IMPLEMENTED_CHECKS.len(),
        contract::BLOCKED_CHECKS,
    );
}
```

Confirm the exported constant names and shapes against
`usage-collector-sdk/src/contract.rs` before writing — `BLOCKED_CHECKS` carries
each name with its blocker, so its element type may not be a bare `&str`.

- [ ] **Step 3: `common::start_backend`**

`tests/common/mod.rs` already starts a TimescaleDB container via
`testcontainers` for the existing pg suites. Extend it with a helper returning
a `StorageAdapter` wired to a migrated pool, rather than writing a second
container harness.

The suite "writes entries and never removes them, so a backend under test
starts each run from whatever state the previous one left". A fresh container
per run makes that moot; if the harness reuses one, read the fixtures' keying
assumptions before relying on repeated runs.

- [ ] **Step 4: Run it**

```bash
cd gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features postgres \
  --no-fail-fast -E 'test(the_timescale_backend_conforms)' 2>&1 | tail -40
```

Needs a reachable Docker daemon. Expected: PASS.

**If it fails, the violation list names each check.** Do not patch the check;
fix the backend. If a violation looks like a defect in the suite rather than in
the backend, stop and report it — that would be a twentieth DIVERGENCES entry,
and this plan's Task 18 is where it lands.

- [ ] **Step 5: Commit**

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/tests/ \
        gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/Cargo.toml
git commit -s -m "test(timescaledb-plugin): run the DESIGN 3.3 contract suite

The acceptance criterion for the port. Five of DESIGN's seven checks are
written plus one DESIGN states without tabulating; the assertion message
reports the blocked ones alongside the violations, because a green run over
five checks must not read as a run over seven."
```

---

## Task 15: Rewrite the Postgres integration suites

Four suites survive Task 1 and every one was written against the old model.

**Files:**
- Rewrite: `tests/records_ingest_integration_pg.rs` (702), `tests/records_query_integration_pg.rs` (919), `tests/cleanup_integration_pg.rs` (239), `tests/id_uniqueness_integration_pg.rs` (93), `tests/schema_integration_pg.rs` (75), `tests/common/mod.rs` (346)

- [ ] **Step 1: Inventory what each suite asserts, before changing any of it**

For each file, list the questions it asks. Then mark each: **still a live
question** (repoint it), or **a question the model no longer has** (delete it).
Produce that list in the task report. "Deleted the failing test" and "deleted
the test whose question no longer exists" look identical in a diff, and only
the second is legitimate.

**Two items Task 1 handed forward and this task must close:**

- **`tests/common/mod.rs`'s `setup_with_type(_gts, _fields)` ignores both
  parameters.** Task 1 kept the signature deliberately, to avoid churning 17
  call sites this task rewrites anyway, and documented that it did. A helper
  taking two ignored arguments is a trap for anyone who assumes it still
  registers a type. **It must not survive this task** — either give it a
  signature matching what it does, or delete it.
- **`pg_insert_with_unregistered_gts_id_is_usage_type_not_found` must be
  deleted, not repointed** (see Task 3 Step 2b). It passes for the wrong
  reason once the catalog table is gone.

- [ ] **Step 2: `schema_integration_pg.rs`**

Asserts the DDL. Rewrite against Task 3's schema: the hypertable partitions on
`window_end`, the dedup UNIQUE spans the 5-tuple, the partial unique index on
`(invalidates, window_end)` exists, `entry_type` is a stored generated column,
and `usage_type_catalog` **does not exist**.

That last one is a real assertion, not a formality — it is what catches a stale
database surviving a migration change.

- [ ] **Step 3: `id_uniqueness_integration_pg.rs`**

The id is a UUIDv5 over the 5-tuple. Assert that two entries differing only in
`window_start` get different ids, and that `entry_type` is **not** an input to
the derivation (`DESIGN.md:63`) — an invalidation and its target differ in
`invalidates` and `idempotency_key`, and the latter is what separates their ids.

- [ ] **Step 4: `records_ingest_integration_pg.rs`**

The behavioural home for Task 9. Cover:

- an exact-equality retry under the same idempotency key is absorbed and
  returns the previously persisted row
- a divergent same-key write is a fail-closed `IdempotencyConflict`
- `acceptance_sequence` is strictly monotonic per `(tenant_id, gts_type_id)`,
  and two different scopes do **not** share a sequence
- a second invalidation of one target is `AlreadyInvalidated` — **including
  when both arrive in one `create_batch`**, which is the case the SPI singles
  out
- per-record outcomes in a batch are aligned with input order

The concurrency case is worth a real test: two concurrent `create_usage_record`
calls invalidating one target, exactly one accepted. That is what the atomicity
obligation is for, and a sequential test cannot see it.

- [ ] **Step 4b: Two tests assert the rule Task 8 inverted — delete, do not repoint**

`tests/records_query_integration_pg.rs` carries two tests that encode the
*retired* compensation semantics, found during Task 2:

- `pg_aggregate_sum_nets_compensation` (`:336`)
- `pg_aggregate_count_excludes_active_compensation` (`:383`)

Both set `compensation.corrects_id = Some(original_id)` (`:348`, `:398`), and
`corrects_id` no longer exists anywhere in the SDK (`grep -rn 'corrects_id'`
over `usage-collector-sdk/src/` returns nothing). So neither compiles.

**They must be deleted rather than ported, and the reason is semantic, not
mechanical.** Their subject is the rule Task 8 *inverted*: the old model had
`SUM` net across signed compensation rows and `COUNT` filter
`corrects_id IS NULL`. An invalidation echoes the quantity it withdraws rather
than negating it, so netting now double-counts, and both halves of a withdrawn
pair are excluded under every fold. A test "ported" by swapping `corrects_id`
for `invalidates` would assert the opposite of the current contract while
looking like a faithful translation. That is the most dangerous shape available
here.

The replacement coverage is Task 8's `every_fold_excludes_both_halves_of_a_withdrawn_pair`
plus the behavioural cases in Step 5 below. Write the verdict as "deleted
because the rule it asserted was replaced by its opposite", and name the
replacement.

**The module doc at `:6-7` still advertises `SUM nets compensation`.** Task 2
dropped its `active-only` sibling but deliberately left this one, because the
fold rewrite is yours. Remove it in the same pass.

- [ ] **Step 5: `records_query_integration_pg.rs`**

The behavioural home for Tasks 10, 11 and 12. Cover:

- `from <= window_end < to` at both boundaries, and a point event
  (`window_start == window_end`) needing no special case
- an entry whose `window_start` is inside the range but whose `window_end` is
  outside is **not** selected
- `$filter` on each of the published eight resolves and discriminates —
  especially `origin` and `entry_type`, the two entry 16 could not serve
- `$orderby` on each of `KEYSET_SAFE_RECORD_FIELDS`
- a page boundary falling **between** an invalidation and its target, which the
  SPI names explicitly: the pair shares a `window_end` but not an `id`
- a `next_cursor` round-trips and its `f` equals the `filter_hash` passed in
- the point lookup returns `NotFound` for a row outside the compiled scope,
  and the **same** error for an id that does not exist
- both halves of a withdrawn pair are returned by `list` and `get`, and
  contribute nothing to any fold
- an orphan invalidation whose target was purged still contributes nothing

- [ ] **Step 6: `cleanup_integration_pg.rs`**

Retention. The policy now measures from `window_end` (Task 3 Step 3). Assert
the retention horizon is measured from the covered period, which is what
`cpt-cf-usage-collector-fr-idempotency` requires.

- [ ] **Step 7: Run the whole pg suite**

```bash
cd gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features postgres --no-fail-fast 2>&1 | tail -20
```

Report the unfiltered `N passed / M skipped`.

- [ ] **Step 8: Commit**

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/tests/
git commit -s -m "test(timescaledb-plugin): rewrite the Postgres suites for the current model

Covers what the contract suite does not: the keyset obligations, the
concurrent at-most-one-invalidation case, per-scope acceptance_sequence
monotonicity, and a page boundary falling between an invalidation and its
target."
```

---

## Task 16: Rewire the plugin into the build

Substantially the revert of `8225d8ebd` `build(usage-collector): unwire the
timescaledb plugin` — **minus the CI half, which is Task 17**, because turning
CI back on is a decision rather than a revert.

```bash
git show --stat 8225d8ebd
```

Measured: 9 files, `.github/workflows/ci.yml` (3 lines),
`.github/workflows/e2e.yml` (12), `Cargo.lock` (37), `Cargo.toml` (1),
`Makefile` (12), `apps/cf-gears-example-server/Cargo.toml` (4),
`apps/cf-gears-example-server/src/registered_gears.rs` (3),
`testing/e2e/suites/usage_collector/config.yaml` (16),
`testing/e2e/suites/usage_collector/e2e.yaml` (7).

**Files:**
- Modify: `Cargo.toml`, `Cargo.lock`, `Makefile`, `apps/cf-gears-example-server/Cargo.toml`, `apps/cf-gears-example-server/src/registered_gears.rs`, `testing/e2e/suites/usage_collector/config.yaml`

- [ ] **Step 1: Confirm the workspace member line, and add the dependency alias**

**The `members` line was already added in Task 0** — it had to be, because the
crate cannot be compiled in any form without it. Confirm it is still there:

```bash
grep -n 'timescaledb-usage-collector-plugin' Cargo.toml
```

Then check whether `8225d8ebd` also removed a workspace dependency alias
alongside the other two (lines 350 and 365 as measured), and restore it if so:

```bash
git show 8225d8ebd -- Cargo.toml
```

- [ ] **Step 2: Restore the Makefile target**

`git show 8225d8ebd -- Makefile` shows what was removed. Restore
`test-usage-collector-pg`, adjusting for anything that has changed in the
Makefile since.

- [ ] **Step 3: Restore the example server registration**

`apps/cf-gears-example-server/Cargo.toml` and `src/registered_gears.rs`.

- [ ] **Step 4: Restore the e2e suite config**

`testing/e2e/suites/usage_collector/config.yaml` — the 16 removed lines
configure the TimescaleDB plugin DSN with the port placeholder `conftest.py`
substitutes.

Leave `e2e.yaml` for Task 17; its comment is about CI.

- [ ] **Step 5: `cargo check --workspace` for real**

```bash
cd /Users/binarycode/code/virtuozzo/gears-rust
cargo check --workspace --all-targets
```

This is the first run that includes the plugin. `Cargo.lock` updates itself.

- [ ] **Step 6: Full verification bar**

```bash
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk \
  -p cf-gears-noop-usage-collector-plugin \
  -p cf-gears-timescaledb-usage-collector-plugin --no-fail-fast
cargo nextest run -p cf-gears-usage-collector-sdk --features contract --no-fail-fast
cargo clippy --workspace --all-targets --all-features
cargo +nightly fmt
cargo doc --no-deps -p cf-gears-usage-collector-sdk -p cf-gears-usage-collector
```

Against baseline: **716 + the plugin's own tests, 0 skipped**; contract
**166 / 0**; doc warnings **35** (host) and **0** (SDK), neither grown. Read the
`generated N warnings` line.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock Makefile apps/cf-gears-example-server \
        testing/e2e/suites/usage_collector/config.yaml
git commit -s -m "build(usage-collector): rewire the timescaledb plugin

Reverses the build half of 8225d8ebd. The plugin is a workspace member again,
the example server registers it, the nextest pg target is back, and the e2e
suite has its plugin config. The CI steps are restored separately."
```

---

## Task 17: Un-defer CI and bring the e2e python current

Per the owner's decision, **both** removed CI steps come back. The type-plane
rewrite that justified deferring them ends with this slice.

**Files:**
- Modify: `.github/workflows/ci.yml`, `.github/workflows/e2e.yml`, `testing/e2e/suites/usage_collector/e2e.yaml`, `testing/e2e/suites/usage_collector/test_integration_seams.py`, and whatever Step 1 turns up

- [ ] **Step 1: Read the e2e python before assuming the revert is clean**

```bash
cd /Users/binarycode/code/virtuozzo/gears-rust/testing/e2e/suites/usage_collector
grep -rn -i 'usage_type\|corrects\|status\|created_at\|catalog\|old model\|still on the' *.py
```

`test_integration_seams.py:58` carries a comment saying the plugin "is still on
the" old model. Read the whole comment and the test under it. Others may name
retired routes — slice 2 removed two usage-type routes and slice 5 added
`/records/backfill`.

Produce the same live-question / dead-question inventory Task 15 Step 1 asks
for, and report per-item verdicts.

- [ ] **Step 2: Restore the CI steps**

```bash
git show 8225d8ebd -- .github/workflows/ci.yml .github/workflows/e2e.yml
```

Two steps were removed: **"Test timescaledb usage-collector plugin (pg
integration)"** in `ci.yml` and **"Run usage-collector E2E tests (TimescaleDB
container)"** in `e2e.yml`. Restore both, adjusting for drift in the
surrounding workflow.

**Delete the comments that replaced them.** `e2e.yml:100-102` says the
usage-collector suite "is not run here at all during the type-plane rewrite —
see testing/e2e/suites/usage_collector/e2e.yaml". Leaving that comment beside a
restored step is exactly the "claim outliving the code" defect.

- [ ] **Step 3: Rewrite the `e2e.yaml` header**

It currently opens:

> NOT RUN IN CI during the type-plane rewrite: the step that invoked this suite
> has been removed from .github/workflows/e2e.yml. … `make e2e-usage-collector`
> still runs it locally and is expected to fail until the TimescaleDB plugin is
> ported.

Every clause is now false. **Replace the block, do not append to it.**

- [ ] **Step 4: Run the suite locally**

```bash
cd /Users/binarycode/code/virtuozzo/gears-rust
make e2e-usage-collector
```

Needs Docker — the suite starts its own TimescaleDB container. Expected: pass.
If it does not, the suite is telling you something the unit and contract tests
could not; fix the plugin, not the suite, unless the inventory in Step 1 says
the assertion is a dead question.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/ci.yml .github/workflows/e2e.yml \
        testing/e2e/suites/usage_collector/
git commit -s -m "ci(usage-collector): run the timescaledb plugin and E2E suites again

Restores the two steps 8225d8ebd removed and deletes the comments that stood
in for them, which said the suite is not run during the type-plane rewrite.
That rewrite ends with this slice. Brings the e2e python to the current model.

CI regains a Docker dependency for the usage-collector lanes."
```

---

## Task 18: Regenerate `api.json`, settle the label, update `DIVERGENCES.md`

Last, because `api.json` is a generated artifact that conflicts noisily.

**Files:**
- Modify: `docs/api/api.json`, `DIVERGENCES.md`

- [ ] **Step 1: Re-verify §A before acting on it**

DIVERGENCES §A records what the regeneration will contain, and slice 6
corrected it once already.

**Measured at this slice's start:** `git diff --stat main -- docs/api/api.json`
is **empty** — the file is byte-identical to `main`. `api.json` carries
`QueryAggregatedUsageRecordsRequest` **twice** and `AggregationRequest`
**zero** times. §A's three claims — adds `/records/backfill`, removes two
usage-type routes, renames a published component — check out.

Re-run those three commands before trusting these numbers; Tasks 1-17 have
landed since.

- [ ] **Step 2: Regenerate**

```bash
cd /Users/binarycode/code/virtuozzo/gears-rust
make openapi
git diff --stat docs/api/api.json
```

- [ ] **Step 3: Read the diff against §A**

Confirm it adds `/records/backfill`, removes the two usage-type routes, and
renames `QueryAggregatedUsageRecordsRequest` to `AggregationRequest`. **If the
diff contains anything §A does not predict, stop and report it** rather than
committing a generated file whose contents you have not accounted for.

- [ ] **Step 4: Settle the `breaking-api-acknowledged` label**

The slice-6 breaking changes plus this slice's. Enumerate:

```bash
git log --oneline 9f64cf22f..HEAD | grep '!'
```

Slice 6 contributed three. This plan adds a `!` to Tasks 1, 2, 3, 8, 9, 10, 11
and 12. Every one is a real wire or SPI break, so the label is owed.

The label goes on the PR, which does not exist yet — the gateway work and this
port merge together. **Report the enumerated list so whoever opens the PR
applies the label**; do not open the PR as part of this task unless asked.

- [ ] **Step 5: Update `DIVERGENCES.md`**

Measured at slice start: **19 numbered entries** (`grep -c '^## [0-9]'`) and
**15 load-bearing** markers (`grep -c '^\*\*Load-bearing'`). **Note the
handoff's "14" was stale** — the entry-8 split added one. Re-run both before
editing.

Changes owed:

- **Entry 16 is resolved.** Task 6 implemented its proposed resolution
  verbatim. Mark it resolved; do not delete it.
- **Entry 10 is unchanged.** `accepted_at` and `acceptance_sequence` are still
  absent from the SDK model, by the owner's decision. The plugin now assigns a
  sequence in its own table, which is DESIGN §3.7's obligation and **does not**
  close entry 10's — that is about the SDK model and the published response
  shape. **Do not mark it resolved, and do not reword it to imply progress.**
  Add one sentence recording where the sequence now lives, and why that leaves
  the entry open.
- **Entry 19 is unchanged.** `latest-tie-break` stays blocked for the same
  reason. Worth adding: this backend implements the declared tie-break exactly
  while the reference backend cannot, so the two now differ on a rule no check
  asserts — which sharpens the entry rather than resolving it.
- **§A is discharged** by Steps 2-3.
- **§G may need a new note** if Task 8 Step 6 found the absent-dimension
  disagreement. This backend collects NULL groups where the reference backend
  drops them, and §G says a spec owner decides. Record it there.
- **Any twentieth entry** this port turned up.

- [ ] **Step 6: Full verification bar, one last time**

```bash
cargo check --workspace --all-targets
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk \
  -p cf-gears-noop-usage-collector-plugin \
  -p cf-gears-timescaledb-usage-collector-plugin --no-fail-fast
cargo nextest run -p cf-gears-usage-collector-sdk --features contract --no-fail-fast
cargo clippy --workspace --all-targets --all-features
cargo +nightly fmt
cargo doc --no-deps -p cf-gears-usage-collector-sdk -p cf-gears-usage-collector
```

0 skipped. Doc warnings 35 / 0, neither grown.

- [ ] **Step 7: Commit**

```bash
git add docs/api/api.json DIVERGENCES.md
git commit -s -m "docs(usage-collector): regenerate api.json and record what the port closed

api.json was byte-identical to main and the contract workflow fails on any
diff. The regeneration adds /records/backfill, removes the two usage-type
routes, and renames QueryAggregatedUsageRecordsRequest to AggregationRequest.

DIVERGENCES entry 16 is resolved by this slice. Entries 10 and 19 are not:
acceptance_sequence now exists in the plugin's own table, which is DESIGN
3.7's obligation, and neither it nor accepted_at reached the SDK model or the
published response shape, so the LATEST fold still has nothing to read and
latest-tie-break stays blocked."
```

---

## Self-review — run this before declaring the plan done

1. **Scope coverage.** Every item in SLICE7.md's "What the port actually is"
   has a task: the 229 stale references (Tasks 1, 2, 4, 5, 6, 8, 9), the SPI
   signature move (Tasks 10, 11, 12), `catalog_store` (Task 1), the migrations
   (Task 3), entry 16 (Task 6), the rewiring commit (Tasks 16, 17), the
   api.json debt (Task 18).

2. **Out of scope, and no task touches them:** the usage feed and
   reconciliation, `CursorBeyondRetention` (entry 18), gear-level workload
   isolation (entry 12), `AggregationDimension` growing `Origin` (entry 15),
   regenerating `DECOMPOSITION.md` or `docs/features/*` (entries 5, 14), any
   crate version bump. Task 8 Step 5 says explicitly not to add an `Origin`
   dimension arm.

3. **Type consistency.** `MeterTypeId` (not `UsageTypeGtsId`),
   `AggregationFold` (not `AggregationOp`/`AggregationSpec`), `gts_type_id`
   (not `gts_id`), `window_start`/`window_end` (not `created_at`),
   `invalidates`/`reason_code` (not `corrects_id`/`status`), `RecordOrigin`,
   `Invalidation`, `TimeRange`. Used consistently across Tasks 4-13.

4. **Every named test carries its mutation.** Tasks 5, 6, 7, 8, 9, 10, 11, 12
   each name the one-token edit that makes each new test red. A test whose
   mutation you cannot name does not go in.
