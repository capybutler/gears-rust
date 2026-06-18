# TimescaleDB Usage Collector Storage Plugin — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Team execution conventions (VHP):** Drive execution **group-by-group**, not task-by-task. A **Group** (A, B, C…) is the unit of review — the controller (main session) reviews the whole group at its end, in the warm session. Code-generation subagents run on **Opus**. Subagents gather facts and write code; they **never run cold builds** — `cargo build`/`test`/`clippy`/`dylint` run in the main session with the warm cache. **Heavy gates** (`dylint --all`, `cargo shear --expand`, `cargo clippy --workspace`) run **once at each Phase end**, not per task.

**Goal:** Implement the TimescaleDB storage backend plugin (`cf-gears-timescaledb-usage-collector-plugin`) that realizes the `UsageCollectorPluginV1` SPI on PostgreSQL + TimescaleDB, per `DESIGN.md` (VHP-1142).

**Architecture:** A ToolKit plugin crate following the canonical DDD-light layout (mirrors `oidc-authn-plugin`): `gear.rs` does the `#[toolkit::gear]` registration handshake; `domain/` holds the SPI **adapter** (implements `UsageCollectorPluginV1`) plus two **port traits** (`RecordStore`, `CatalogStore`) and stays infrastructure-free; `infra/storage/` holds the concrete `sqlx`-backed Postgres implementations of those ports, the connection pool, row entities/mappers, SQL error classification, and the injection-safe OData→SQL translator; `infra/metrics.rs` holds the OTel instruments. Persistence is **direct `sqlx`** (runtime queries) against a `usage_records` hypertable and a `usage_type_catalog` table.

**Tech Stack:** Rust, `sqlx` 0.8 (postgres, runtime-tokio, tls-rustls-aws-lc-rs), TimescaleDB (hypertable + retention policy), `toolkit` gear macro + `ClientHub`, `types-registry-sdk`, `usage-collector-sdk`, `toolkit-odata` (filter AST + `CursorV1` + `Page`), `toolkit-macros` (`domain_model`, `ExpandVars`), `rust_decimal`, `time`, `uuid`, `opentelemetry` 0.31, `testcontainers` 0.27.

---

## Conventions used throughout

- **Crate name:** `cf-gears-timescaledb-usage-collector-plugin`; **lib name:** `timescaledb_usage_collector_plugin`.
- **Crate root:** `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/`. All `Create:`/`Modify:` paths below are relative to that root unless they start with `Cargo.toml` (workspace root) or `gears/…`.
- **Build one crate:** `cargo build -p cf-gears-timescaledb-usage-collector-plugin`
- **Unit tests (no Docker):** `cargo test -p cf-gears-timescaledb-usage-collector-plugin`
- **Integration tests (Docker + TimescaleDB image):** `cargo test -p cf-gears-timescaledb-usage-collector-plugin --features postgres`
- **Integration test gating (repo convention):** files end with `_integration_pg.rs`, begin with `#![cfg(feature = "postgres")]`, use a `tests/common/mod.rs` harness, name test fns `pg_<scenario>_<outcome>`, and use `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`. `testcontainers` is an unconditional dev-dep (the `#![cfg]` gates compilation).
- **Unit test gating (repo convention):** sibling `*_tests.rs` file linked via `#[cfg(test)] #[cfg_attr(coverage_nightly, coverage(off))] #[path = "<file>_tests.rs"] mod <name>;` with `use super::*;`.
- **Commit cadence:** one commit per task (TDD). Branch `usage-collector/timescaledb-plugin` (checked out). Reference `VHP-1142` in commit subjects.

### SPI surface to implement (verbatim, `usage-collector-sdk/src/plugin_api.rs`)

```rust
#[async_trait]
pub trait UsageCollectorPluginV1: Send + Sync + 'static {
    async fn create_usage_record(&self, record: UsageRecord) -> Result<UsageRecord, UsageCollectorPluginError>;
    async fn create_usage_records(&self, records: Vec<UsageRecord>)
        -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>;
    async fn get_usage_record(&self, uuid: Uuid) -> Result<UsageRecord, UsageCollectorPluginError>;
    async fn query_aggregated_usage_records(&self, gts_id: UsageTypeGtsId, query: &ODataQuery,
        metadata_filter: &[MetadataFilter], aggregation: AggregationSpec)
        -> Result<AggregationResult, UsageCollectorPluginError>;
    async fn list_usage_records(&self, gts_id: UsageTypeGtsId, query: &ODataQuery,
        metadata_filter: &[MetadataFilter]) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError>;
    async fn deactivate_usage_record(&self, id: Uuid) -> Result<(), UsageCollectorPluginError>;
    async fn create_usage_type(&self, usage_type: UsageType) -> Result<UsageType, UsageCollectorPluginError>;
    async fn get_usage_type(&self, gts_id: UsageTypeGtsId) -> Result<UsageType, UsageCollectorPluginError>;
    async fn list_usage_types(&self, query: &ODataQuery) -> Result<ODataPage<UsageType>, UsageCollectorPluginError>;
    async fn delete_usage_type(&self, gts_id: UsageTypeGtsId) -> Result<(), UsageCollectorPluginError>;
}
```

Error vocabulary (`UsageCollectorPluginError`): `Transient { detail, retry_after_seconds }`, `UsageTypeNotFound { gts_id }`, `UsageTypeAlreadyExists { gts_id }`, `UsageTypeReferenced { gts_id, sample_ref_count }`, `IdempotencyConflict { idempotency_key, existing_uuid }`, `UsageRecordNotFound { id }`, `UsageRecordAlreadyInactive { id }`, `Internal(String)`. (noop uses `UsageCollectorPluginError::internal("...")`.)

`UsageRecord` fields: `uuid: Uuid`, `gts_id: UsageTypeGtsId`, `tenant_id: Uuid`, `resource_ref: ResourceRef` (`.resource_id()->&str`, `.resource_type()->&str`), `subject_ref: Option<SubjectRef>` (`.subject_id()->&str`, `.subject_type()->Option<&str>`), `metadata: BTreeMap<MetadataKey, String>`, `value: Decimal`, `idempotency_key: IdempotencyKey` (`.as_str()`), `corrects_id: Option<Uuid>`, `status: UsageRecordStatus` (default `Active`), `created_at: time::OffsetDateTime`. (`ingested_at` is DB-only, server-set.)

`UsageType` fields: `gts_id: UsageTypeGtsId`, `kind: UsageKind` (`Counter`/`Gauge`, serde lowercase), `metadata_fields: BTreeSet<MetadataKey>`.

---

## File structure (locked before tasks)

```
gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/
├── Cargo.toml                          # [features] postgres = []
├── migrations/
│   └── 0001_init.sql                   # extension, catalog, records hypertable, unique, FK, indexes
├── src/
│   ├── lib.rs                          # mod config/domain/gear/infra; re-export gear
│   ├── gear.rs                         # #[toolkit::gear] struct + Gear::init   ⟶ cpt component-module
│   ├── config.rs / config_tests.rs     # TimescaleDbPluginConfig + Default + ExpandVars + validate()
│   ├── domain.rs                       # pub mod adapter; pub mod ports;
│   ├── domain/
│   │   ├── adapter.rs                  # StorageAdapter: impl UsageCollectorPluginV1 ⟶ cpt component-adapter
│   │   └── ports.rs                    # RecordStore + CatalogStore traits (infra-free)
│   ├── infra.rs                        # pub mod storage; (pub mod metrics; added P5)
│   └── infra/
│       ├── storage.rs                  # pub mod pool/entity/mapper/error/query/record_store/catalog_store
│       ├── storage/
│       │   ├── pool.rs                 # build_pool, apply_retention_policy, MIGRATOR ⟶ cpt component-migrations
│       │   ├── error.rs / error_tests.rs   # sqlx::Error -> UsageCollectorPluginError
│       │   ├── entity.rs               # sqlx FromRow rows (UsageRecordRow, UsageTypeRow)
│       │   ├── mapper.rs / mapper_tests.rs # row -> SDK model; status/kind helpers
│       │   ├── record_store.rs         # PgRecordStore: impl ports::RecordStore ⟶ cpt component-record-store
│       │   ├── catalog_store.rs        # PgCatalogStore: impl ports::CatalogStore ⟶ cpt component-catalog-store
│       │   ├── query.rs                # pub mod translate/bind/keyset/aggregate
│       │   └── query/
│       │       ├── translate.rs / translate_tests.rs  # FilterNode<F> -> parameterized WHERE; column allowlist
│       │       ├── bind.rs             # SqlBind enum + ODataValue->SqlBind + bind onto query
│       │       ├── keyset.rs           # ORDER BY + keyset WHERE + cursor encode/decode
│       │       └── aggregate.rs        # AggregationSpec -> SQL (P4)
│       └── metrics.rs                  # OTel instruments + pool gauges (P5)
└── tests/
    ├── common/mod.rs                   # testcontainers TimescaleDB harness (#![cfg(feature="postgres")])
    ├── schema_integration_pg.rs        # P1 gate: migrate + retention + schema assertions
    ├── catalog_integration_pg.rs       # P2/P4: catalog CRUD + FK-referenced delete + list
    ├── records_ingest_integration_pg.rs# P3: dedup/compensation/deactivation
    └── records_query_integration_pg.rs # P4: keyset pagination + aggregation correctness
```

**Layering rules (enforced by DE0309 lint + `#[domain_model]`):** nothing under `domain/` may name a `sqlx`/`sea_orm` type; the `StorageAdapter` holds `Arc<dyn RecordStore>` + `Arc<dyn CatalogStore>` (allowed trait objects). All `sqlx` lives under `infra/`. Error classification and metrics live in `infra/` (DB-coupled). Migrations are `.sql` at crate root (sqlx idiom; SeaORM `.rs` migrations N/A). No `<name>-sdk` crate (consumes `usage-collector-sdk`) and no `api/rest/` (the plugin has no REST surface).

---

# Phase 1 — Skeleton, schema, contracts, wiring

**Exit criteria:** crate compiles, the `StorageAdapter` + `gear.rs` register exactly like the noop plugin, and `schema_integration_pg` migrates + applies retention against a real TimescaleDB container. All store methods are stubs returning `Internal`; the adapter delegation and gear wiring are **final** after this phase.

## Group A — Crate scaffold + config

### Task A1: Create the crate and register it in the workspace

**Files:**
- Create: `Cargo.toml` (crate)
- Create: `src/lib.rs`
- Modify: `Cargo.toml` (workspace root: `members` after the noop entry; add `bigdecimal`/`chrono` workspace deps if absent)

- [ ] **Step 1: Write the crate `Cargo.toml`**

```toml
[package]
name = "cf-gears-timescaledb-usage-collector-plugin"
description = "TimescaleDB Usage Collector storage backend plugin"
version = "0.1.0"
edition.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
rust-version.workspace = true
keywords = ["constructorfabric", "cf-gears"]
metadata.docs.rs.all-features = true

[lib]
name = "timescaledb_usage_collector_plugin"

[lints]
workspace = true

[features]
# Gates the TimescaleDB-backed real-DB integration tests (Docker required).
postgres = []

[dependencies]
usage-collector-sdk = { workspace = true }
types-registry-sdk = { workspace = true }

toolkit = { workspace = true }
toolkit-macros = { workspace = true }
toolkit-odata = { workspace = true }

async-trait = { workspace = true }

sqlx = { workspace = true, features = ["postgres", "uuid", "time", "rust_decimal", "json", "migrate"] }

# ODataValue carries these types; converted to time/rust_decimal at the bind layer.
bigdecimal = { workspace = true }
chrono = { workspace = true }

rust_decimal = { workspace = true }
time = { workspace = true }
uuid = { workspace = true }

serde = { workspace = true }
serde_json = { workspace = true }

opentelemetry = { workspace = true }
tracing = { workspace = true }

anyhow = { workspace = true }
inventory = { workspace = true }

[dev-dependencies]
tokio = { workspace = true }
# Unconditional: [dev-dependencies] can't be optional; usage is #![cfg(feature="postgres")]-gated.
testcontainers = { workspace = true }
```

- [ ] **Step 2: Confirm/add `bigdecimal` + `chrono` workspace deps** — Run `grep -nE '^(bigdecimal|chrono) ' Cargo.toml`. If absent, add to `[workspace.dependencies]` mirroring the versions in `libs/toolkit-odata/Cargo.toml` so they unify.

- [ ] **Step 3: Write `src/lib.rs`** (modules added as built; declare the skeleton now)

```rust
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

//! TimescaleDB storage backend plugin for the Usage Collector storage Plugin SPI.

pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use gear::TimescaleDbUsageCollectorPlugin;
```

> Until later tasks create `domain`/`gear`/`infra`, comment those `mod`/`pub use` lines out, or create empty module files. Keep `lib.rs` compiling at every step. Simplest: create empty `src/domain.rs`, `src/infra.rs`, and defer `mod gear`/the re-export to Task D-gear; uncomment as each lands.

- [ ] **Step 4: Add the workspace member** — in root `Cargo.toml`, right after the noop line:
```toml
    "gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin",
```

- [ ] **Step 5:** Run `cargo build -p cf-gears-timescaledb-usage-collector-plugin` → success.
- [ ] **Step 6: Commit** `feat(uc-timescaledb): scaffold plugin crate (VHP-1142)`

### Task A2: Config struct

**Files:** Create `src/config.rs`, `src/config_tests.rs`; Modify `src/lib.rs` (`pub mod config;`).

- [ ] **Step 1: Write `src/config_tests.rs`**

```rust
use super::*;

#[test]
fn config_defaults_are_applied() {
    let cfg: TimescaleDbPluginConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(cfg.vendor, "cyberfabric");
    assert_eq!(cfg.priority, 10);
    assert_eq!(cfg.pool_size_min, 2);
    assert_eq!(cfg.pool_size_max, 16);
    assert_eq!(cfg.connection_timeout_secs, 10);
    assert_eq!(cfg.retention_period_secs, 365 * 86_400);
    assert!(cfg.database_url.is_empty());
}

#[test]
fn validate_rejects_empty_database_url() {
    let cfg: TimescaleDbPluginConfig = serde_json::from_str("{}").unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn validate_rejects_min_gt_max_pool() {
    let json = r#"{ "database_url": "postgres://x", "pool_size_min": 20, "pool_size_max": 4 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn validate_accepts_well_formed_config() {
    let json = r#"{ "database_url": "postgres://u:p@h/db?sslmode=require" }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.validate().is_ok());
}

#[test]
fn config_rejects_unknown_fields() {
    let json = r#"{ "database_url": "postgres://x", "nope": true }"#;
    assert!(serde_json::from_str::<TimescaleDbPluginConfig>(json).is_err());
}
```

- [ ] **Step 2:** Run `cargo test -p cf-gears-timescaledb-usage-collector-plugin config` → FAIL.
- [ ] **Step 3: Implement `src/config.rs`**

```rust
use serde::Deserialize;

/// Configuration for the TimescaleDB Usage Collector storage backend.
/// Durations are whole seconds (repo convention).
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct TimescaleDbPluginConfig {
    /// Postgres DSN; TLS required (use `sslmode=require`).
    pub database_url: String,
    /// Connection-pool lower bound.
    pub pool_size_min: u32,
    /// Connection-pool upper bound.
    pub pool_size_max: u32,
    /// Acquire timeout in seconds.
    pub connection_timeout_secs: u64,
    /// `usage_records` retention window in seconds; chunks wholly older are dropped.
    pub retention_period_secs: u64,
    /// Vendor name for GTS instance registration.
    pub vendor: String,
    /// Plugin priority (lower = higher priority).
    pub priority: i16,
}

impl Default for TimescaleDbPluginConfig {
    fn default() -> Self {
        Self {
            database_url: String::new(),
            pool_size_min: 2,
            pool_size_max: 16,
            connection_timeout_secs: 10,
            retention_period_secs: 365 * 86_400, // 365 days
            vendor: "cyberfabric".to_owned(),
            priority: 10,
        }
    }
}

impl TimescaleDbPluginConfig {
    /// Validate invariants not expressible in the type.
    ///
    /// # Errors
    /// Returns an error string for an empty DSN, inconsistent pool bounds, or a zero retention window.
    pub fn validate(&self) -> Result<(), String> {
        if self.database_url.trim().is_empty() {
            return Err("database_url must not be empty".to_owned());
        }
        if self.pool_size_max == 0 || self.pool_size_min > self.pool_size_max {
            return Err(format!("invalid pool bounds: min={} max={}", self.pool_size_min, self.pool_size_max));
        }
        if self.retention_period_secs == 0 {
            return Err("retention_period_secs must be > 0".to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "config_tests.rs"]
mod config_tests;
```

- [ ] **Step 4:** Run `cargo test -p cf-gears-timescaledb-usage-collector-plugin config` → PASS (5).
- [ ] **Step 5: Commit** `feat(uc-timescaledb): config struct + validation (VHP-1142)`

### Group A review gate (controller)
- [ ] Config matches `DESIGN.md` §3.5 (defaults 2/16, 10s, 365d, priority 10).
- [ ] Member registered; `bigdecimal`/`chrono` unified with `toolkit-odata`; crate compiles.

---

## Group B — Schema migration

### Task B1: Initial schema migration

**Files:** Create `migrations/0001_init.sql`.

> Dedup UNIQUE includes the partition column (`created_at`) — TimescaleDB forbids a hypertable unique index without it; same-key/different-`created_at` is caught by the app guard in Phase 3. FK is hypertable→normal-table (allowed). `resource_id`/`subject_id` are `text` (SDK `String`). Retention is **not** here — it's config-driven, applied at init (Task C2).

- [ ] **Step 1: Write `migrations/0001_init.sql`**

```sql
-- TimescaleDB Usage Collector storage backend — base schema.
CREATE EXTENSION IF NOT EXISTS timescaledb;

CREATE TABLE IF NOT EXISTS usage_type_catalog (
    gts_id          text PRIMARY KEY,
    kind            text NOT NULL CHECK (kind IN ('counter', 'gauge')),
    metadata_fields text[] NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS usage_records (
    uuid            uuid        NOT NULL,
    tenant_id       uuid        NOT NULL,
    gts_id          text        NOT NULL,
    value           numeric     NOT NULL,
    created_at      timestamptz NOT NULL,
    resource_id     text        NOT NULL,
    resource_type   text        NOT NULL,
    subject_id      text,
    subject_type    text,
    idempotency_key text        NOT NULL,
    corrects_id     uuid,
    status          text        NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'inactive')),
    metadata        jsonb       NOT NULL DEFAULT '{}'::jsonb,
    ingested_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (uuid, created_at),
    CONSTRAINT usage_records_dedup_uniq
        UNIQUE (tenant_id, gts_id, idempotency_key, created_at),
    CONSTRAINT usage_records_gts_id_fk
        FOREIGN KEY (gts_id) REFERENCES usage_type_catalog (gts_id) ON DELETE RESTRICT
);

SELECT create_hypertable('usage_records', 'created_at', if_not_exists => TRUE);

CREATE INDEX IF NOT EXISTS usage_records_tenant_gts_time_idx
    ON usage_records (tenant_id, gts_id, created_at DESC);
CREATE INDEX IF NOT EXISTS usage_records_tenant_time_idx
    ON usage_records (tenant_id, created_at DESC);
CREATE INDEX IF NOT EXISTS usage_records_corrects_id_idx
    ON usage_records (corrects_id) WHERE corrects_id IS NOT NULL;
```

- [ ] **Step 2:** Validated by `schema_integration_pg` (Task D-smoke), not locally. Smoke-grep: `grep -c CREATE migrations/0001_init.sql` → 5.
- [ ] **Step 3: Commit** `feat(uc-timescaledb): base schema migration (VHP-1142)`

### Group B review gate (controller)
- [ ] Matches `DESIGN.md` §3.7 (columns, PK, dedup UNIQUE incl. `created_at`, FK RESTRICT, indexes, hypertable). Retention deliberately absent.

---

## Group C — Domain contract + infra foundation

### Task C1: Port traits (`domain/ports.rs`)

**Files:** Create `src/domain.rs`, `src/domain/ports.rs`; Modify `src/lib.rs` (`pub mod domain;`).

- [ ] **Step 1: `src/domain.rs`**

```rust
pub mod adapter;
pub mod ports;
```

> (`adapter` lands in Task C5; until then comment that line or create an empty `domain/adapter.rs`.)

- [ ] **Step 2: Implement `src/domain/ports.rs`** — the domain↔infra contract; infra-free, returns SDK types.

```rust
use async_trait::async_trait;
use toolkit_odata::{ODataQuery, Page as ODataPage};
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationResult, AggregationSpec, MetadataFilter, UsageCollectorPluginError, UsageRecord,
    UsageType, UsageTypeGtsId,
};

/// Persistence + query operations on `usage_records`. Implemented by infra.
#[async_trait]
pub trait RecordStore: Send + Sync + 'static {
    async fn create(&self, record: UsageRecord) -> Result<UsageRecord, UsageCollectorPluginError>;
    async fn create_batch(&self, records: Vec<UsageRecord>)
        -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>;
    async fn get(&self, uuid: Uuid) -> Result<UsageRecord, UsageCollectorPluginError>;
    async fn list(&self, gts_id: UsageTypeGtsId, query: &ODataQuery, metadata_filter: &[MetadataFilter])
        -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError>;
    async fn aggregate(&self, gts_id: UsageTypeGtsId, query: &ODataQuery,
        metadata_filter: &[MetadataFilter], spec: AggregationSpec)
        -> Result<AggregationResult, UsageCollectorPluginError>;
    async fn deactivate(&self, id: Uuid) -> Result<(), UsageCollectorPluginError>;
}

/// Catalog operations on `usage_type_catalog`. Implemented by infra.
#[async_trait]
pub trait CatalogStore: Send + Sync + 'static {
    async fn create(&self, usage_type: UsageType) -> Result<UsageType, UsageCollectorPluginError>;
    async fn get(&self, gts_id: UsageTypeGtsId) -> Result<UsageType, UsageCollectorPluginError>;
    async fn list(&self, query: &ODataQuery) -> Result<ODataPage<UsageType>, UsageCollectorPluginError>;
    async fn delete(&self, gts_id: UsageTypeGtsId) -> Result<(), UsageCollectorPluginError>;
}
```

- [ ] **Step 3:** `cargo build -p …` → success. **Step 4: Commit** `feat(uc-timescaledb): domain port traits (VHP-1142)`

### Task C2: Infra pool + retention + migrator

**Files:** Create `src/infra.rs`, `src/infra/storage.rs`, `src/infra/storage/pool.rs`; Modify `src/lib.rs` (`pub mod infra;`).

- [ ] **Step 1: `src/infra.rs`** → `pub mod storage;`  **and** `src/infra/storage.rs` →
```rust
pub mod catalog_store;
pub mod entity;
pub mod error;
pub mod mapper;
pub mod pool;
pub mod query;
pub mod record_store;
```
> Create empty stubs for the not-yet-written submodules so `storage.rs` compiles; each is filled by its task.

- [ ] **Step 2: Implement `src/infra/storage/pool.rs`**

```rust
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use crate::config::TimescaleDbPluginConfig;

/// Embedded schema migrations (`migrations/` at crate root).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Build the connection pool. TLS is governed by the DSN (`sslmode=require`).
///
/// # Errors
/// Returns `sqlx::Error` if the pool cannot connect within the timeout.
pub async fn build_pool(cfg: &TimescaleDbPluginConfig) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .min_connections(cfg.pool_size_min)
        .max_connections(cfg.pool_size_max)
        .acquire_timeout(Duration::from_secs(cfg.connection_timeout_secs))
        .connect(&cfg.database_url)
        .await
}

/// Idempotently register the config-driven retention policy. Runs after migrations.
///
/// # Errors
/// Returns `sqlx::Error` if the policy statement fails.
pub async fn apply_retention_policy(pool: &PgPool, retention_secs: u64) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT add_retention_policy('usage_records', drop_after => make_interval(secs => $1), if_not_exists => TRUE)")
        .bind(retention_secs as f64)
        .execute(pool)
        .await?;
    Ok(())
}
```

> **Confirm during implementation:** `make_interval(secs => $1)` arg type and the `add_retention_policy(..., drop_after => ...)` signature for the pinned TimescaleDB version; `if_not_exists => TRUE` makes re-runs no-ops.

- [ ] **Step 3:** `cargo build -p …` → success. **Step 4: Commit** `feat(uc-timescaledb): pool builder + retention + migrator (VHP-1142)`

### Task C3: SQL error classification

**Files:** Create `src/infra/storage/error.rs`, `src/infra/storage/error_tests.rs`.

- [ ] **Step 1: Write `error_tests.rs`**

```rust
use super::*;

#[test]
fn unique_violation_on_dedup_is_dedup_conflict() {
    assert_eq!(classify_db("23505", Some("usage_records_dedup_uniq")), DbErrorClass::DedupUniqueViolation);
}
#[test]
fn unique_violation_on_catalog_pk_is_type_exists() {
    assert_eq!(classify_db("23505", Some("usage_type_catalog_pkey")), DbErrorClass::CatalogUniqueViolation);
}
#[test]
fn fk_violation_is_type_referenced() {
    assert_eq!(classify_db("23503", Some("usage_records_gts_id_fk")), DbErrorClass::ForeignKeyViolation);
}
#[test]
fn connection_class_is_transient() {
    assert_eq!(classify_db("08006", None), DbErrorClass::Transient);
    assert_eq!(classify_db("57P03", None), DbErrorClass::Transient);
}
#[test]
fn unknown_code_is_other() {
    assert_eq!(classify_db("42601", None), DbErrorClass::Other);
}
```

- [ ] **Step 2:** Run `cargo test -p … error` → FAIL.
- [ ] **Step 3: Implement `src/infra/storage/error.rs`**

```rust
use usage_collector_sdk::UsageCollectorPluginError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbErrorClass {
    DedupUniqueViolation,
    CatalogUniqueViolation,
    ForeignKeyViolation,
    Transient,
    Other,
}

fn is_transient_sqlstate(code: &str) -> bool {
    code.starts_with("08") || matches!(code, "57P01" | "57P02" | "57P03" | "53300" | "40001" | "40P01")
}

#[must_use]
pub fn classify_db(code: &str, constraint: Option<&str>) -> DbErrorClass {
    match code {
        "23505" => match constraint {
            Some("usage_records_dedup_uniq") => DbErrorClass::DedupUniqueViolation,
            _ => DbErrorClass::CatalogUniqueViolation,
        },
        "23503" => DbErrorClass::ForeignKeyViolation,
        c if is_transient_sqlstate(c) => DbErrorClass::Transient,
        _ => DbErrorClass::Other,
    }
}

/// `(sqlstate, constraint)` if `err` is a DB error.
#[must_use]
pub fn db_code_and_constraint(err: &sqlx::Error) -> Option<(String, Option<String>)> {
    if let sqlx::Error::Database(db) = err {
        return Some((db.code()?.into_owned(), db.constraint().map(ToOwned::to_owned)));
    }
    None
}

/// Catch-all mapping for non-classified sqlx errors (transient vs internal).
#[must_use]
pub fn map_sqlx_err(err: &sqlx::Error) -> UsageCollectorPluginError {
    if let sqlx::Error::Database(db) = err {
        if classify_db(db.code().as_deref().unwrap_or(""), db.constraint()) == DbErrorClass::Transient {
            return UsageCollectorPluginError::Transient { detail: "transient database error".to_owned(), retry_after_seconds: None };
        }
    }
    if matches!(err, sqlx::Error::PoolTimedOut | sqlx::Error::Io(_) | sqlx::Error::PoolClosed) {
        return UsageCollectorPluginError::Transient { detail: "database unavailable".to_owned(), retry_after_seconds: None };
    }
    UsageCollectorPluginError::Internal(format!("database error: {err}"))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "error_tests.rs"]
mod error_tests;
```

> **Confirm:** `UsageCollectorPluginError::{Transient,Internal}` construction; `sqlx::error::DatabaseError::constraint()` exists in sqlx 0.8.

- [ ] **Step 4:** Run `cargo test -p … error` → PASS (5). **Step 5: Commit** `feat(uc-timescaledb): sqlx error classification (VHP-1142)`

### Group C review gate (controller)
- [ ] Ports return SDK types only (no sqlx); classifier covers the constraint names in `0001_init.sql`; crate compiles.

---

## Group D — Stub stores + adapter + gear wiring + smoke test

### Task D1: Stub Postgres stores

**Files:** Create `src/infra/storage/record_store.rs`, `src/infra/storage/catalog_store.rs`.

- [ ] **Step 1: Implement stub `PgRecordStore` + `PgCatalogStore`** — hold the pool, impl the ports, every method returns `Internal("<op> not implemented")`. (Bodies filled in Phases 2–4.)

```rust
// record_store.rs
use async_trait::async_trait;
use sqlx::PgPool;
use toolkit_odata::{ODataQuery, Page as ODataPage};
use uuid::Uuid;
use usage_collector_sdk::{
    AggregationResult, AggregationSpec, MetadataFilter, UsageCollectorPluginError, UsageRecord,
    UsageTypeGtsId,
};
use crate::domain::ports::RecordStore;

#[derive(Debug, Clone)]
pub struct PgRecordStore {
    pool: PgPool,
}
impl PgRecordStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self { Self { pool } }
}
fn ni(op: &str) -> UsageCollectorPluginError { UsageCollectorPluginError::Internal(format!("{op} not implemented")) }

#[async_trait]
impl RecordStore for PgRecordStore {
    async fn create(&self, _r: UsageRecord) -> Result<UsageRecord, UsageCollectorPluginError> { Err(ni("record.create")) }
    async fn create_batch(&self, _r: Vec<UsageRecord>)
        -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError> { Err(ni("record.create_batch")) }
    async fn get(&self, _u: Uuid) -> Result<UsageRecord, UsageCollectorPluginError> { Err(ni("record.get")) }
    async fn list(&self, _g: UsageTypeGtsId, _q: &ODataQuery, _m: &[MetadataFilter])
        -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> { Err(ni("record.list")) }
    async fn aggregate(&self, _g: UsageTypeGtsId, _q: &ODataQuery, _m: &[MetadataFilter], _s: AggregationSpec)
        -> Result<AggregationResult, UsageCollectorPluginError> { Err(ni("record.aggregate")) }
    async fn deactivate(&self, _id: Uuid) -> Result<(), UsageCollectorPluginError> { Err(ni("record.deactivate")) }
}
```

(Analogous `PgCatalogStore` with the 4 catalog methods, holding `pool`.)

- [ ] **Step 2:** `cargo build -p …` → success. **Step 3: Commit** `feat(uc-timescaledb): stub Postgres stores (VHP-1142)`

### Task D2: SPI adapter (domain)

**Files:** Create `src/domain/adapter.rs`; ensure `src/domain.rs` declares `pub mod adapter;`.

- [ ] **Step 1: Implement `StorageAdapter`** — `#[domain_model]`, holds the two ports, delegates every SPI method. This is **final** (no further edits in later phases).

```rust
use std::sync::Arc;

use async_trait::async_trait;
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page as ODataPage};
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationResult, AggregationSpec, MetadataFilter, UsageCollectorPluginError,
    UsageCollectorPluginV1, UsageRecord, UsageType, UsageTypeGtsId,
};

use crate::domain::ports::{CatalogStore, RecordStore};

/// The single implementation of `UsageCollectorPluginV1`. Delegates record ops
/// to the [`RecordStore`] port and catalog ops to the [`CatalogStore`] port.
#[domain_model]
pub struct StorageAdapter {
    record: Arc<dyn RecordStore>,
    catalog: Arc<dyn CatalogStore>,
}

impl StorageAdapter {
    #[must_use]
    pub fn new(record: Arc<dyn RecordStore>, catalog: Arc<dyn CatalogStore>) -> Self {
        Self { record, catalog }
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for StorageAdapter {
    async fn create_usage_record(&self, record: UsageRecord) -> Result<UsageRecord, UsageCollectorPluginError> {
        self.record.create(record).await
    }
    async fn create_usage_records(&self, records: Vec<UsageRecord>)
        -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError> {
        self.record.create_batch(records).await
    }
    async fn get_usage_record(&self, uuid: Uuid) -> Result<UsageRecord, UsageCollectorPluginError> {
        self.record.get(uuid).await
    }
    async fn query_aggregated_usage_records(&self, gts_id: UsageTypeGtsId, query: &ODataQuery,
        metadata_filter: &[MetadataFilter], aggregation: AggregationSpec)
        -> Result<AggregationResult, UsageCollectorPluginError> {
        self.record.aggregate(gts_id, query, metadata_filter, aggregation).await
    }
    async fn list_usage_records(&self, gts_id: UsageTypeGtsId, query: &ODataQuery,
        metadata_filter: &[MetadataFilter]) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
        self.record.list(gts_id, query, metadata_filter).await
    }
    async fn deactivate_usage_record(&self, id: Uuid) -> Result<(), UsageCollectorPluginError> {
        self.record.deactivate(id).await
    }
    async fn create_usage_type(&self, usage_type: UsageType) -> Result<UsageType, UsageCollectorPluginError> {
        self.catalog.create(usage_type).await
    }
    async fn get_usage_type(&self, gts_id: UsageTypeGtsId) -> Result<UsageType, UsageCollectorPluginError> {
        self.catalog.get(gts_id).await
    }
    async fn list_usage_types(&self, query: &ODataQuery) -> Result<ODataPage<UsageType>, UsageCollectorPluginError> {
        self.catalog.list(query).await
    }
    async fn delete_usage_type(&self, gts_id: UsageTypeGtsId) -> Result<(), UsageCollectorPluginError> {
        self.catalog.delete(gts_id).await
    }
}
```

> **Confirm:** `#[domain_model]` accepts `Arc<dyn RecordStore>` fields (own-domain trait objects are allowed). If the macro objects to the async-trait impl on the same type, fall back to placing the adapter in `infra/` — but per `oidc-authn-plugin` the SPI-impl-in-domain pattern holds.

- [ ] **Step 2:** `cargo build -p …` → success. **Step 3: Commit** `feat(uc-timescaledb): SPI storage adapter delegating to ports (VHP-1142)`

### Task D3: Gear init wiring

**Files:** Create `src/gear.rs`; Modify `src/lib.rs` (`pub mod gear; pub use gear::TimescaleDbUsageCollectorPlugin;`).

- [ ] **Step 1: Implement `src/gear.rs`** (mirrors noop wiring + pool/migrate/retention; constructs the stores + adapter).

```rust
use std::sync::Arc;

use async_trait::async_trait;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use toolkit::Gear;
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};
use usage_collector_sdk::{UsageCollectorPluginSpecV1, UsageCollectorPluginV1};

use crate::config::TimescaleDbPluginConfig;
use crate::domain::adapter::StorageAdapter;
use crate::domain::ports::{CatalogStore, RecordStore};
use crate::infra::storage::catalog_store::PgCatalogStore;
use crate::infra::storage::pool::{apply_retention_policy, build_pool, MIGRATOR};
use crate::infra::storage::record_store::PgRecordStore;

/// TimescaleDB Usage Collector storage backend plugin module.
#[toolkit::gear(
    name = "timescaledb-usage-collector-plugin",
    deps = ["types-registry"]
)]
#[derive(Default)]
pub struct TimescaleDbUsageCollectorPlugin;

#[async_trait]
impl Gear for TimescaleDbUsageCollectorPlugin {
    // @cpt-flow:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: TimescaleDbPluginConfig = ctx.config_expanded_or_default()?;
        cfg.validate().map_err(|e| anyhow::anyhow!("invalid timescaledb plugin config: {e}"))?;

        let pool = build_pool(&cfg).await?;
        MIGRATOR.run(&pool).await?;
        apply_retention_policy(&pool, cfg.retention_period_secs).await?;

        let (instance_id, instance_json) = PluginV1::<UsageCollectorPluginSpecV1>::build_registration(
            "cf.core._.timescaledb_usage_collector.v1",
            cfg.vendor.clone(),
            cfg.priority,
        )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        let record: Arc<dyn RecordStore> = Arc::new(PgRecordStore::new(pool.clone()));
        let catalog: Arc<dyn CatalogStore> = Arc::new(PgCatalogStore::new(pool.clone()));
        let adapter = StorageAdapter::new(record, catalog);

        ctx.client_hub().register_scoped::<dyn UsageCollectorPluginV1>(
            ClientScope::gts_id(&instance_id),
            Arc::new(adapter) as Arc<dyn UsageCollectorPluginV1>,
        );

        info!(instance_id = %instance_id, vendor = %cfg.vendor, priority = cfg.priority,
            "Registered TimescaleDB usage-collector plugin instance");
        Ok(())
    }
}
```

> **Confirm:** the `unique_id` convention (mirror noop's `cf.core._.noop_usage_collector.v1` → `…timescaledb_usage_collector.v1`); `ctx.config_expanded_or_default()`; `sqlx::migrate!("./migrations")` resolves at crate root.

- [ ] **Step 2:** `cargo build -p …` → success. **Step 3: Commit** `feat(uc-timescaledb): gear init wiring (pool/migrate/retention/GTS) (VHP-1142)`

### Task D4: TimescaleDB testcontainer harness + smoke test

**Files:** Create `tests/common/mod.rs`, `tests/schema_integration_pg.rs`.

- [ ] **Step 1: `tests/common/mod.rs`**

```rust
#![cfg(feature = "postgres")]
//! Shared TimescaleDB testcontainer harness. Requires Docker.

use std::time::Duration;

use sqlx::PgPool;
use testcontainers::core::WaitFor;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

use timescaledb_usage_collector_plugin::config::TimescaleDbPluginConfig;
use timescaledb_usage_collector_plugin::infra::storage::pool::{apply_retention_policy, build_pool, MIGRATOR};

pub struct TsHarness {
    pub pool: PgPool,
    _container: ContainerAsync<GenericImage>,
}

pub async fn bring_up() -> anyhow::Result<TsHarness> {
    let image = GenericImage::new("timescale/timescaledb", "2.17.2-pg16")
        .with_wait_for(WaitFor::message_on_stderr("database system is ready to accept connections"))
        .with_env_var("POSTGRES_USER", "user")
        .with_env_var("POSTGRES_PASSWORD", "pass")
        .with_env_var("POSTGRES_DB", "app");
    let container = image.start().await?;
    let port = container.get_host_port_ipv4(5432).await?;

    let cfg = TimescaleDbPluginConfig {
        database_url: format!("postgres://user:pass@127.0.0.1:{port}/app"),
        ..Default::default()
    };

    // The image logs "ready" once during init before a restart; retry connect briefly.
    let mut pool = None;
    let mut last = None;
    for _ in 0..20 {
        match build_pool(&cfg).await {
            Ok(p) => { pool = Some(p); break; }
            Err(e) => { last = Some(e); tokio::time::sleep(Duration::from_millis(500)).await; }
        }
    }
    let pool = pool.ok_or_else(|| anyhow::anyhow!("pool connect failed: {last:?}"))?;

    MIGRATOR.run(&pool).await?;
    apply_retention_policy(&pool, cfg.retention_period_secs).await?;
    Ok(TsHarness { pool, _container: container })
}
```

> **Confirm:** testcontainers 0.27 API (`GenericImage::new`, `with_wait_for`, `with_env_var`, `start`, `get_host_port_ipv4(5432)` — bare `u16`, matching `account-management/tests/common/mod.rs:1347`); pin a real published tag (e.g. `2.17.2-pg16`).

- [ ] **Step 2: `tests/schema_integration_pg.rs`**

```rust
#![cfg(feature = "postgres")]
mod common;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_migrations_create_hypertable_and_retention() {
    let h = common::bring_up().await.expect("timescaledb container (Docker required)");

    let ht: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM timescaledb_information.hypertables WHERE hypertable_name = 'usage_records'",
    ).fetch_one(&h.pool).await.expect("hypertable query");
    assert_eq!(ht, 1, "usage_records must be a hypertable");

    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM timescaledb_information.jobs \
         WHERE proc_name = 'policy_retention' AND hypertable_name = 'usage_records'",
    ).fetch_one(&h.pool).await.expect("jobs query");
    assert!(jobs >= 1, "retention policy must be registered");

    sqlx::query("SELECT gts_id, kind, metadata_fields FROM usage_type_catalog LIMIT 0")
        .fetch_all(&h.pool).await.expect("usage_type_catalog must exist");
}
```

> **Confirm:** `timescaledb_information` view/column names for the pinned image version.

- [ ] **Step 3:** Run `cargo test -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test schema_integration_pg` (Docker up) → PASS. If Docker is unavailable, run at the phase gate.
- [ ] **Step 4: Commit** `test(uc-timescaledb): testcontainer harness + schema smoke test (VHP-1142)`

### Group D review gate (controller)
- [ ] Init order: pool → migrate → retention → register; adapter + gear are final (later phases touch only infra store bodies).
- [ ] `config`/`infra::storage::pool` are `pub` for test reuse.

## ✅ Phase 1 end gate (run once)
- [ ] `cargo test -p cf-gears-timescaledb-usage-collector-plugin` (unit) passes.
- [ ] `cargo test -p … --features postgres` passes (Docker up).
- [ ] `cargo clippy --workspace --all-targets` clean.
- [ ] `cargo dylint --all` clean (incl. DE0309 — `StorageAdapter` has `#[domain_model]`, no sqlx in `domain/`).
- [ ] `cargo shear --expand` reports no unused deps.

---

# Phase 2 — Query-translation foundation + Catalog store

**Exit criteria:** the injection-safe OData→SQL translator + keyset/cursor helpers + row mapping are unit-tested; `PgCatalogStore` create/get/delete implemented and pass `catalog_integration_pg`. (`list` shares the paginator built in Phase 4 — flagged.)

## Group E — Injection-safe OData→SQL translation + row mapping

### Task E1: Confirm toolkit-odata public API (research, no code)

- [ ] **Step 1:** Read `libs/toolkit-odata/src/{filter,lib,page}.rs`. Record (as a doc comment atop `query/translate.rs`): the converter name/visibility/error (`convert_expr_to_filter_node::<F>`), `FilterNode<F>` variants, `FilterOp` variants, `FilterField` methods (`name`/`kind`/`from_name`/`FIELDS`), the macro-generated variant names for `UsageRecordFilterField`/`UsageTypeFilterField` and whether `.name()` returns the snake column name, `ODataValue` variants, `ODataOrderBy`/`OrderKey`/`SortDir`, `CursorV1` fields + `encode`/`decode`, and `Page::new` + `PageInfo`. Also confirm `UsageTypeGtsId` ↔ `&str` (construct from string; read as `&str` for binding), `ResourceRef::new`/`SubjectRef::new`, `MetadataKey::new`. No commit.

### Task E2: Value binding (`query/bind.rs`)

**Files:** Create `src/infra/storage/query.rs` (module file), `src/infra/storage/query/bind.rs`, `src/infra/storage/query/translate.rs`, `src/infra/storage/query/translate_tests.rs`.

- [ ] **Step 1: `query.rs`** → `pub mod aggregate; pub mod bind; pub mod keyset; pub mod translate;` (+ empty stubs for `keyset`/`aggregate`).
- [ ] **Step 2: Write failing tests** in `translate_tests.rs` for column allowlist + value conversion:

```rust
use super::*;
use bigdecimal::BigDecimal;
use std::str::FromStr;

#[test]
fn record_field_columns_are_allowlisted() {
    assert_eq!(record_column("created_at"), Some("created_at"));
    assert_eq!(record_column("tenant_id"), Some("tenant_id"));
    assert_eq!(record_column("status"), Some("status"));
    assert_eq!(record_column("definitely_not_a_column"), None);
}
#[test]
fn number_converts_to_decimal_bind() {
    let v = crate::infra::storage::query::bind::odata_value_to_bind(
        &ODataValue::Number(BigDecimal::from_str("42.5").unwrap())).unwrap();
    assert!(matches!(v, SqlBind::Decimal(d) if d.to_string() == "42.5"));
}
#[test]
fn datetime_converts_to_offsetdatetime_bind() {
    use chrono::TimeZone;
    let dt = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
    assert!(matches!(
        crate::infra::storage::query::bind::odata_value_to_bind(&ODataValue::DateTime(dt)).unwrap(),
        SqlBind::DateTime(_)));
}
```

- [ ] **Step 3:** Run `cargo test -p … query` → FAIL.
- [ ] **Step 4: Implement `query/bind.rs`** (`SqlBind` enum + `odata_value_to_bind` chrono→time / bigdecimal→Decimal conversion + `bind_one`).

```rust
use bigdecimal::BigDecimal;
use rust_decimal::Decimal;
use std::str::FromStr;
use time::OffsetDateTime;
use uuid::Uuid;

use toolkit_odata::filter::ODataValue; // confirm path in E1

#[derive(Debug, Clone)]
pub enum SqlBind { Uuid(Uuid), Str(String), Decimal(Decimal), DateTime(OffsetDateTime), Bool(bool) }

/// Convert an OData AST value (chrono/bigdecimal-typed) to a storage-typed bind.
///
/// # Errors
/// Errors on `Null`/`Date`/`Time` (not used by usage-record filters) or out-of-range numerics.
pub fn odata_value_to_bind(v: &ODataValue) -> Result<SqlBind, String> {
    match v {
        ODataValue::Uuid(u) => Ok(SqlBind::Uuid(*u)),
        ODataValue::String(s) => Ok(SqlBind::Str(s.clone())),
        ODataValue::Bool(b) => Ok(SqlBind::Bool(*b)),
        ODataValue::Number(n) => Decimal::from_str(&n.to_string())
            .map(SqlBind::Decimal).map_err(|e| format!("numeric out of range: {e}")),
        ODataValue::DateTime(dt) => {
            let nanos = dt.timestamp_nanos_opt().ok_or("datetime out of range")?;
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(nanos))
                .map(SqlBind::DateTime).map_err(|e| format!("datetime conversion: {e}"))
        }
        ODataValue::Null => Err("null filter value unsupported".to_owned()),
        ODataValue::Date(_) | ODataValue::Time(_) => Err("date/time-only filter values unsupported".to_owned()),
    }
}

#[must_use]
pub fn bind_one<'q>(
    q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    v: &'q SqlBind,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    match v {
        SqlBind::Uuid(u) => q.bind(u),
        SqlBind::Str(s) => q.bind(s),
        SqlBind::Decimal(d) => q.bind(d),
        SqlBind::DateTime(t) => q.bind(t),
        SqlBind::Bool(b) => q.bind(b),
    }
}
```

- [ ] **Step 5: Implement the allowlists in `query/translate.rs`** (`record_column`, `usage_type_column` — closed `match` on `FilterField::name()`), plus `pub use` of bind items so tests resolve `SqlBind`/`ODataValue`. Add the `#[path="translate_tests.rs"] mod` hook.

```rust
#[must_use]
pub fn record_column(field_name: &str) -> Option<&'static str> {
    match field_name {
        "uuid" => Some("uuid"), "created_at" => Some("created_at"), "tenant_id" => Some("tenant_id"),
        "resource_id" => Some("resource_id"), "resource_type" => Some("resource_type"),
        "subject_id" => Some("subject_id"), "subject_type" => Some("subject_type"),
        "corrects_id" => Some("corrects_id"), "status" => Some("status"),
        _ => None,
    }
}
#[must_use]
pub fn usage_type_column(field_name: &str) -> Option<&'static str> {
    match field_name { "gts_id" => Some("gts_id"), "kind" => Some("kind"), _ => None }
}
```

- [ ] **Step 6:** Run `cargo test -p … query` → PASS. **Step 7: Commit** `feat(uc-timescaledb): column allowlist + odata value binding (VHP-1142)`

### Task E3: Filter AST → WHERE fragment

**Files:** Modify `query/translate.rs`, `query/translate_tests.rs`.

- [ ] **Step 1: Failing tests** — `Binary` eq → `status = $1` + one bind; `Composite And` → `(tenant_id = $1 AND created_at >= $2)`; an unmapped field → `Err`. (Helpers `rec_field(name)`/`uuid_val()`/`dt_val()` built via `from_name`/`ODataValue`.)
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** `SqlCtx { next, binds: Vec<SqlBind> }`, `op_sql(FilterOp)`, and `translate_record_filter<F: FilterField>(&FilterNode<F>, &mut SqlCtx) -> Result<String, String>` walking Binary/InList/Composite/Not — identifiers via `record_column` (reject unknown), values via `odata_value_to_bind` → `$N`. Add a parallel `translate_usage_type_filter` using `usage_type_column`. `Contains/StartsWith/EndsWith` → `Err` (SPI fields are exact-match; LIKE out of scope).

```rust
use toolkit_odata::filter::{FilterField, FilterNode, FilterOp};

pub struct SqlCtx { next: usize, pub binds: Vec<SqlBind> }
impl SqlCtx {
    #[must_use] pub fn new(start: usize) -> Self { Self { next: start, binds: Vec::new() } }
    fn push(&mut self, b: SqlBind) -> usize { let n = self.next; self.next += 1; self.binds.push(b); n }
}
fn op_sql(op: FilterOp) -> Result<&'static str, String> {
    match op { FilterOp::Eq=>Ok("="),FilterOp::Ne=>Ok("<>"),FilterOp::Gt=>Ok(">"),
        FilterOp::Ge=>Ok(">="),FilterOp::Lt=>Ok("<"),FilterOp::Le=>Ok("<="),
        other=>Err(format!("unsupported operator: {other:?}")) }
}
pub fn translate_record_filter<F: FilterField>(node: &FilterNode<F>, ctx: &mut SqlCtx) -> Result<String, String> {
    match node {
        FilterNode::Binary { field, op, value } => {
            let col = record_column(field.name()).ok_or_else(|| format!("field not allowlisted: {}", field.name()))?;
            let n = ctx.push(odata_value_to_bind(value)?);
            Ok(format!("{col} {} ${n}", op_sql(*op)?))
        }
        FilterNode::InList { field, values } => {
            let col = record_column(field.name()).ok_or_else(|| format!("field not allowlisted: {}", field.name()))?;
            let ph = values.iter().map(|v| Ok(format!("${}", ctx.push(odata_value_to_bind(v)?))))
                .collect::<Result<Vec<_>, String>>()?;
            Ok(format!("{col} IN ({})", ph.join(", ")))
        }
        FilterNode::Composite { op, children } => {
            let j = match op { FilterOp::And=>" AND ", FilterOp::Or=>" OR ", o=>return Err(format!("invalid composite: {o:?}")) };
            let parts = children.iter().map(|c| translate_record_filter(c, ctx)).collect::<Result<Vec<_>, _>>()?;
            Ok(format!("({})", parts.join(j)))
        }
        FilterNode::Not(inner) => Ok(format!("NOT ({})", translate_record_filter(inner, ctx)?)),
    }
}
```

- [ ] **Step 4:** Run → PASS. **Step 5: Commit** `feat(uc-timescaledb): odata filter -> parameterized WHERE (VHP-1142)`

### Task E4: Order-by + keyset + cursor helpers

**Files:** Implement `query/keyset.rs`; add tests to `translate_tests.rs`.

- [ ] **Step 1: Failing tests** — `render_order_by` over allowlisted columns (`created_at ASC, uuid ASC`); reject unknown column; `keyset_predicate` ascending → `(created_at, uuid) > ($1, $2)` + 2 binds.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** in `keyset.rs`: `render_order_by(order, col_fn) -> Result<String,String>`; `keyset_predicate(order_pairs, cursor_keys, col_fn, ctx) -> Result<String,String>` (tuple comparison `(c1,c2,…) > / <` for all-asc/all-desc; the gateway default `(created_at, uuid)` asc is the common path; mixed-direction falls back to lexicographic OR-form); `cursor_key_to_bind(column, raw) -> SqlBind` (parse `created_at`→`OffsetDateTime`, `uuid`→`Uuid`, else `Str`); `encode_next_cursor(order, last_row_keys, filter_hash)` → `CursorV1{..}.encode()`; `decode_cursor(token)` over `CursorV1::decode` + `filter_hash` consistency check (mismatch → caller maps to `Internal`).
- [ ] **Step 4:** Run → PASS. **Step 5: Commit** `feat(uc-timescaledb): order-by + keyset + cursor helpers (VHP-1142)`

### Task E5: Row entities + mappers

**Files:** Create `src/infra/storage/entity.rs`, `src/infra/storage/mapper.rs`, `src/infra/storage/mapper_tests.rs`.

- [ ] **Step 1: `entity.rs`** — `#[derive(sqlx::FromRow)]` `UsageRecordRow` (all `usage_records` columns; `value: Decimal`, `created_at: OffsetDateTime`, `metadata: serde_json::Value`, nullable `subject_*`/`corrects_id`) and `UsageTypeRow { gts_id: String, kind: String, metadata_fields: Vec<String> }`.
- [ ] **Step 2: `mapper.rs`** — `record_row_to_model(UsageRecordRow) -> Result<UsageRecord, UsageCollectorPluginError>`, `type_row_to_model(UsageTypeRow) -> Result<UsageType, _>`, and pure helpers `parse_status`/`parse_kind`/`kind_to_sql` + `gts_id_str(&UsageTypeGtsId)->&str` / `gts_id_from_str(&str)->UsageTypeGtsId` (per E1) + `metadata_jsonb_to_map` / `metadata_map_to_jsonb`.
- [ ] **Step 3: `mapper_tests.rs`** — pure round-trip tests for `parse_status`/`parse_kind`/`kind_to_sql` and metadata jsonb↔map. Run → PASS.
- [ ] **Step 4: Commit** `feat(uc-timescaledb): row entities + SDK mappers (VHP-1142)`

### Group E review gate (controller)
- [ ] No identifier ever interpolated outside the closed allowlist; every value bound. E1 findings recorded; no invented APIs. Mappers have no `todo!`.

---

## Group F — Catalog store

### Task F1: `PgCatalogStore` create / get

**Files:** Modify `src/infra/storage/catalog_store.rs`; Create `tests/catalog_integration_pg.rs`; add catalog fixtures to `tests/common/mod.rs`.

- [ ] **Step 1: Failing tests** in `catalog_integration_pg.rs`: `pg_create_then_get_roundtrips`; `pg_create_duplicate_is_already_exists`; `pg_get_missing_is_not_found`. Add `fixture_usage_type(gts, kind, &[fields])` + `fixture_gts_id(gts)` to `common`.
- [ ] **Step 2:** Run `--features postgres --test catalog_integration_pg` → FAIL.
- [ ] **Step 3: Implement** `PgCatalogStore::create` (INSERT; on `db_code_and_constraint` → `CatalogUniqueViolation` → `UsageTypeAlreadyExists{gts_id}`; else `map_sqlx_err`) and `get` (`query_as::<_,UsageTypeRow>` + `fetch_optional` → `type_row_to_model` or `UsageTypeNotFound`). Bind `metadata_fields` as `&[String]`, `kind` via `kind_to_sql`.
- [ ] **Step 4:** Run → PASS. **Step 5: Commit** `feat(uc-timescaledb): catalog create/get (VHP-1142)`

### Task F2: `PgCatalogStore` delete (FK→Referenced)

**Files:** Modify `catalog_store.rs`, `catalog_integration_pg.rs`; add a raw `insert_usage_record_row` helper to `common` (to create an FK child).

- [ ] **Step 1: Failing tests** — `pg_delete_unreferenced_succeeds`; `pg_delete_referenced_is_usage_type_referenced` (count ≥ 1); `pg_delete_missing_is_not_found`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** `delete`: `DELETE … WHERE gts_id=$1`; `rows_affected()==0` → `UsageTypeNotFound`; on FK violation → `SELECT count(*) FROM usage_records WHERE gts_id=$1` → `UsageTypeReferenced{gts_id, sample_ref_count: count.max(1) as u64}`; else `map_sqlx_err`.
- [ ] **Step 4:** Run → PASS. **Step 5: Commit** `feat(uc-timescaledb): catalog delete with FK->Referenced (VHP-1142)`

> `PgCatalogStore::list` is implemented in Phase 4 Task I1 (shares the records paginator). It stays the stub `Internal` until then; `list_usage_types` via the adapter therefore returns `Internal` at the end of Phase 2 — acceptable (list isn't a Phase-2 exit criterion).

### Group F review gate (controller)
- [ ] Errors match the design: collision→AlreadyExists, FK→Referenced (count≥1), missing delete→NotFound. Adapter delegation already final from Phase 1.

## ✅ Phase 2 end gate (run once)
- [ ] Unit + `--features postgres` (`schema_*`, `catalog_*` create/get/delete) pass.
- [ ] `clippy --workspace`, `dylint --all`, `shear --expand` clean.

---

# Phase 3 — Record ingest, point read, deactivation

**Exit criteria:** `PgRecordStore` create/batch/get/deactivate implemented; pass `records_ingest_integration_pg`.

## Group G — Insert + dedup + compensation

### Task G1: Single insert with dedup + app guard

**Files:** Modify `src/infra/storage/record_store.rs`; Create `tests/records_ingest_integration_pg.rs`.

> **Algorithm** (§3.6 ingest-dedup + the UNIQUE(…,created_at)+guard decision):
> 1. App-guard lookup: `SELECT <cols> FROM usage_records WHERE tenant_id=$1 AND gts_id=$2 AND idempotency_key=$3 LIMIT 1`. If present → canonical-equal → **absorb** (return stored); else → `IdempotencyConflict{idempotency_key, existing_uuid}`.
> 2. Else `INSERT … ON CONFLICT (tenant_id, gts_id, idempotency_key, created_at) DO NOTHING RETURNING <cols>`. Row → inserted. No row (lost the exact-tuple race) → re-run step 1.
> "Canonical fields": `(uuid, value, created_at, resource_id, resource_type, subject_id, subject_type, corrects_id, metadata)`.

- [ ] **Step 1: Failing tests** — insert new → Active; exact retry → same stored row (absorb); same key + different `value` → `IdempotencyConflict` (orig uuid); compensation (negative value + `corrects_id`) persists with `corrects_id`.
- [ ] **Step 2:** Run `--features postgres --test records_ingest_integration_pg` → FAIL.
- [ ] **Step 3: Implement** `canonical_equal(&UsageRecordRow, &UsageRecord) -> bool` + `PgRecordStore::create` per the algorithm; build metadata jsonb via `metadata_map_to_jsonb`; classify via `db_code_and_constraint`/`map_sqlx_err`.
- [ ] **Step 4:** Run → PASS. **Step 5: Commit** `feat(uc-timescaledb): single insert with dedup + app guard (VHP-1142)`

### Task G2: Batch insert (per-record outcomes, input order)

**Files:** Modify `record_store.rs`, `records_ingest_integration_pg.rs`.

- [ ] **Step 1: Failing tests** — batch of 3 with a conflicting #2 → `[Ok, Err(IdempotencyConflict), Ok]`; empty batch → `Internal`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** `create_batch`: empty → `Err(Internal)`; else iterate `create` per row, collecting `Vec<Result<…>>` in input order (a conflict on one MUST NOT fail others). Note in code: optimized multi-row INSERT deferred (correctness + ordering first, per the design's batch semantics).
- [ ] **Step 4:** Run → PASS. **Step 5: Commit** `feat(uc-timescaledb): batch insert with per-row outcomes (VHP-1142)`

### Group G review gate (controller)
- [ ] Absorb vs conflict matches canonical comparison; `existing_uuid` is the stored uuid; batch preserves order + isolates conflicts.

## Group H — Point read + deactivation cascade

### Task H1: `get`

**Files:** Modify `record_store.rs`, `records_ingest_integration_pg.rs`.

- [ ] **Step 1: Failing test** — insert then `get` returns it; unknown uuid → `UsageRecordNotFound`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** `get`: `SELECT <cols> FROM usage_records WHERE uuid=$1` (`fetch_optional`) → `record_row_to_model` or `UsageRecordNotFound`.
- [ ] **Step 4:** Run → PASS. **Step 5: Commit** `feat(uc-timescaledb): get_usage_record (VHP-1142)`

### Task H2: Deactivation cascade (depth-1, atomic)

**Files:** Modify `record_store.rs`, `records_ingest_integration_pg.rs`.

> **Algorithm** (§3.6 deactivate-cascade): in one transaction — (1) `SELECT status FROM usage_records WHERE uuid=$1 FOR UPDATE` → none → `UsageRecordNotFound`; `inactive` → `UsageRecordAlreadyInactive`. (2) `UPDATE usage_records SET status='inactive' WHERE uuid=$1 OR (corrects_id=$1 AND status='active')`. (3) commit. One-way; no other field mutated.

- [ ] **Step 1: Failing tests** — active target + one active compensation flips both; missing → `UsageRecordNotFound`; already-inactive → `UsageRecordAlreadyInactive`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** `deactivate` using `self.pool.begin()`/`tx.commit()`; `query_scalar` for the `FOR UPDATE` status read; `execute(&mut *tx)` for the update.
- [ ] **Step 4:** Run → PASS. **Step 5: Commit** `feat(uc-timescaledb): depth-1 deactivation cascade (VHP-1142)`

### Group H review gate (controller)
- [ ] Single transaction; exactly target + depth-1 active compensations flip; one-way.

## ✅ Phase 3 end gate (run once)
- [ ] Unit + integration pass. `clippy --workspace`, `dylint --all`, `shear --expand` clean.

---

# Phase 4 — Record queries (raw list + aggregation)

**Exit criteria:** `PgRecordStore` list + aggregate implemented; `PgCatalogStore::list` finished (shared paginator); pass `records_query_integration_pg` + the catalog list test.

## Group I — Keyset raw list

### Task I1: Shared paginator + `list` (records + catalog)

**Files:** Modify `record_store.rs`, `catalog_store.rs`, `query/keyset.rs`; Create `tests/records_query_integration_pg.rs`; add a catalog-list test to `catalog_integration_pg.rs`.

> §3.6 list-keyset: honor `query.order` (non-empty), `query.limit` (default 100; documented), `query.cursor`. Records SQL: `SELECT <cols> FROM usage_records WHERE gts_id=$1 [AND <filter>] [AND <metadata>] [AND <keyset>] ORDER BY <order> LIMIT n+1`. Metadata predicates from `metadata_filter`: `metadata ->> $key = ANY($values)` (key + values bound). Fetch n+1, trim, encode next cursor from last in-page row.

- [ ] **Step 1: Failing tests** — page with `limit=2` returns 2 + `next_cursor`; following cursor → next page, no overlap/gap; order `(created_at, uuid)`; `metadata_filter` narrows; catalog list paginates by `gts_id`.
- [ ] **Step 2:** Run `--features postgres` (both test files) → FAIL.
- [ ] **Step 3: Implement** a private `paginate` (in `keyset.rs` or a `record_store` helper) assembling SQL from `translate_record_filter` + `render_order_by` + `keyset_predicate` + metadata predicates, binding via `SqlCtx` (+`bind_one`), fetching `limit+1`, trimming, mapping rows, building `toolkit_odata::Page`. Implement `PgRecordStore::list`. Then implement `PgCatalogStore::list` with the same pattern (single key `gts_id`, fixed `ORDER BY gts_id ASC`, `usage_type_column` allowlist).
- [ ] **Step 4:** Run both → PASS. **Step 5: Commit** `feat(uc-timescaledb): keyset list for records + catalog (VHP-1142)`

### Group I review gate (controller)
- [ ] No overlap/gap across pages; `filter_hash` mismatch → `Internal`; metadata predicates fully bound; catalog `list` stub replaced (no `Internal` left for list).

## Group J — Pushed-down aggregation

### Task J1: Aggregation translation + execution

**Files:** Implement `query/aggregate.rs`; Modify `record_store.rs`, `records_query_integration_pg.rs`.

> §3.6 aggregated query. `SELECT <group cols…>, <AGG>(value) FROM usage_records WHERE gts_id=$1 AND status='active' [AND <filter>] [AND <metadata>] GROUP BY <group cols…>`. AGG: Sum→`SUM(value)`, Count→`COUNT(*)`, Min/Max/Avg→`MIN/MAX/AVG(value)`. Dimension→column: `TenantId→tenant_id`, `ResourceId→resource_id`, `ResourceType→resource_type`, `SubjectId→subject_id`, `SubjectType→subject_type`, `Metadata(key)→metadata ->> $key` (key bound). Each row → `AggregationBucket{ key: Vec<String> (group order), value: Option<Decimal> }`. NULL subject excluded from subject grouping (per SDK doc).

- [ ] **Step 1: Failing tests** — SUM over active rows (compensation nets out); COUNT; GROUP BY `resource_id` → bucket per resource; GROUP BY `metadata('region')`; inactive rows excluded from SUM.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** `aggregate_dimension_sql(dim, ctx) -> (select_expr, group_expr)` (metadata binds its key), `build_aggregation_sql(...)`, and `PgRecordStore::aggregate` reading group keys as text + agg value as `Option<Decimal>`.
- [ ] **Step 4:** Run → PASS. **Step 5: Commit** `feat(uc-timescaledb): pushed-down aggregation + group-by (VHP-1142)`

### Group J review gate (controller)
- [ ] `status='active'` always applied; dimensions allowlisted; metadata group-by key bound; AVG scale acceptable.

## ✅ Phase 4 end gate (run once)
- [ ] Full unit + integration suite passes. `clippy --workspace`, `dylint --all`, `shear --expand` clean. **No `todo!`/`unimplemented!`/`Internal("… not implemented")` stubs remain** in `src/`.

---

# Phase 5 — Observability + hardening

**Exit criteria:** full OTel metric inventory per `DESIGN.md` §Observability; traceability markers; docs; final conformance.

## Group K — Metrics

### Task K1: Metrics module

**Files:** Create `src/infra/metrics.rs`; Modify `src/infra.rs` (`pub mod metrics;`).

- [ ] **Step 1: Implement `Metrics`** — build the meter exactly like `libs/toolkit-http/src/layers/metrics.rs` (lines 118–120): `let scope = opentelemetry::InstrumentationScope::builder("uc.timescaledb").build(); let meter = opentelemetry::global::meter_with_scope(scope);`. **NB:** `meter_with_scope` takes an `InstrumentationScope`, **not** a `&str` — passing a bare string does not compile. Instruments (every name carries the `uc.timescaledb.` prefix): histograms `uc.timescaledb.insert.duration{mode}`, `uc.timescaledb.query.duration{query_kind}`, `uc.timescaledb.deactivate.duration`, `uc.timescaledb.pool.acquire.duration`, `uc.timescaledb.batch.rows`; counters `uc.timescaledb.dedup.absorbed.count`, `uc.timescaledb.backend.error.count{class}`, `uc.timescaledb.idempotency.conflict.count`, `uc.timescaledb.usage_type.referenced.count`, `uc.timescaledb.migration.failure.count`, `uc.timescaledb.compensation.count`, `uc.timescaledb.query.requests{query_kind}`, `uc.timescaledb.tls.handshake.failure.count`; **observable** gauges `uc.timescaledb.pool.connections.active`, `uc.timescaledb.pool.connections.idle`; **synchronous** gauges `uc.timescaledb.usage_type_catalog.size`, `uc.timescaledb.ready`. Dot-separated, `uc.timescaledb.`-prefixed names (matches the oidc-authn-plugin convention — `authn.*`); `with_unit`; histogram boundaries bracket §1.2 p95 budgets.
- [ ] **Step 2: Gauges.** Only the **two pool gauges are observable** — give **each** its own `with_callback` closure (0.31 has no batch-observe API; one callback per gauge) that clones the `PgPool` and reads `pool.size()`/`pool.num_idle()` (both synchronous in-memory reads; `active = size − num_idle`). The callback MUST stay non-blocking and non-async — do **not** query the DB inside it. `usage_type_catalog.size` and `ready` are **synchronous** gauges set imperatively (NOT observable): `usage_type_catalog.size` refreshed after catalog create/delete (Task K2), `ready` set to 1 after pool+migration. Store the returned `ObservableGauge` handle(s) in the `Metrics` struct so the registered callback isn't dropped. **Step 3:** A test that `Metrics::new()` constructs without panicking. **Step 4: Commit** `feat(uc-timescaledb): OTel metric inventory (VHP-1142)`

> **Verified against `opentelemetry-0.31.0` source:** use the per-instrument builder `meter.u64_observable_gauge("uc.timescaledb.pool.connections.active").with_callback(|obs| obs.observe(v, &[])).build()`. `with_callback` takes `Fn(&dyn AsyncInstrument<u64>) + Send + Sync + 'static` (`metrics/instruments/mod.rs:296`); the observer method is `observe(measurement, &[KeyValue])` (`:24`). **There is no `Meter::register_callback` / batch-observe API in 0.31** — register one callback per gauge; do not try to observe both gauges from a single closure. No existing gear uses observable gauges (every repo gauge is a synchronous `i64_gauge`/`f64_gauge`), so there is no in-repo template to copy.

### Task K2: Instrument the stores + gear

**Files:** Modify `record_store.rs`, `catalog_store.rs`, `gear.rs` (inject `Arc<Metrics>`).

- [ ] **Step 1:** Thread `Arc<Metrics>` into `PgRecordStore`/`PgCatalogStore` (constructed in `gear.rs`); record insert/query/deactivate durations + `query.requests`; increment `dedup.absorbed`, `compensation` (on `corrects_id`), `idempotency.conflict`, `usage_type.referenced`, `backend.error{class}` at the classification sites; refresh `usage_type_catalog.size` after catalog create/delete; set `migration.failure`/`ready` in `gear.rs`. (Domain `StorageAdapter` stays metric-free — recording happens in infra.)
- [ ] **Step 2:** `cargo build` + full `--features postgres` suite (behavior unchanged) → PASS. **Step 3: Commit** `feat(uc-timescaledb): instrument stores with metrics (VHP-1142)`

### Group K review gate (controller)
- [ ] Every §Observability metric emitted; labels bounded (no unbounded ids as labels); `ready` distinct from the gear's structural readiness; metrics live in `infra/` (no opentelemetry in `domain/`).

## Group L — Traceability, docs, conformance

### Task L1: Traceability markers

**Files:** Modify source to add `@cpt-*` markers (mirror noop's `@cpt-flow`/`@cpt-begin`/`@cpt-end` style) tying code to design IDs: `cpt-cf-uc-plugin-component-module` (gear.rs), `-adapter` (domain/adapter.rs), `-record-store` (record_store.rs), `-catalog-store` (catalog_store.rs), `-migrations` (pool.rs/migrations), `-interface-spi` (ports.rs), the dedup/cascade/aggregation/list seq IDs at their methods, `-metric-inventory` (metrics.rs), and the constraint IDs (`-constraint-injection-safe-translation` on query/translate.rs).

- [ ] **Step 1:** Add markers. **Step 2:** Validate via the cypilot traceability tooling (read `.cypilot`; run analyze/validate read-only). **Step 3: Commit** `docs(uc-timescaledb): code<->design traceability markers (VHP-1142)`

### Task L2: Crate README

**Files:** Create `README.md`.

- [ ] **Step 1:** Short README: purpose, config keys (match `config.rs`), how to run integration tests (Docker + `--features postgres` + TimescaleDB image), dedup/retention semantics, pointer to `DESIGN.md`, compile-time SPI conformance note. **Step 2: Commit** `docs(uc-timescaledb): crate README (VHP-1142)`

### Group L review gate (controller)
- [ ] Markers resolve to real design IDs; README config keys match `config.rs`.

## ✅ Phase 5 end gate (run once — final)
- [ ] `cargo test -p cf-gears-timescaledb-usage-collector-plugin` and `--features postgres` all pass.
- [ ] `cargo clippy --workspace --all-targets` clean.
- [ ] `cargo dylint --all` clean (DE0309 incl.).
- [ ] `cargo shear --expand` clean.
- [ ] No `todo!`/`unimplemented!`/dead stubs in `src/`.
- [ ] Plugin registers + resolves end-to-end (host binds by vendor/priority) — verify via the usage-collector gear's selection path if an end-to-end harness exists; else confirm registration via the `schema_integration_pg` path.

---

## Spec-coverage self-check (writer)

- **Pluggable storage / SPI conformance** → D2 adapter (all 10) + L2 note. ✓
- **Ingestion + idempotency + IdempotencyConflict** → G1. ✓
- **Batch ingestion (input order, isolated conflicts)** → G2. ✓
- **Compensation (signed value + corrects_id)** → G1 + K2. ✓
- **Event deactivation (depth-1 atomic, one-way)** → H2. ✓
- **Raw query (keyset)** → I1. ✓
- **Aggregated query (pushed-down + group-by incl. metadata)** → J1. ✓
- **Usage-type catalog (create/get/list/delete + FK Referenced)** → F1, F2, I1(list). ✓
- **Retention-bounded dedup / Data retention** → B1 (UNIQUE incl. created_at) + C2/D3 (config-driven retention). ✓
- **In-DB referential integrity (ON DELETE RESTRICT)** → B1 FK + F2 classification. ✓
- **Injection-safe translation (allowlist + binds)** → Group E. ✓
- **Vendor isolation (no host dep)** → Cargo.toml deps (sdk + types-registry-sdk + toolkit* only). ✓
- **Observability (full inventory, bounded labels, plugin-local ready)** → Group K. ✓
- **Testing (testcontainers; compile-time conformance)** → D4 harness + per-phase `*_integration_pg` suites. ✓
- **TimescaleDB hypertable + indexes** → B1. ✓
- **Canonical layout (DDD-light: gear/domain/ports/infra-storage)** → file structure + Groups C/D/E. ✓

**Sequencing note (not a gap):** `PgCatalogStore::list` (F2) is implemented in I1 (shares the paginator); the adapter delegation for it is final from Phase 1, so only the infra body is deferred.
