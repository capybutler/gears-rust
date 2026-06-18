# TimescaleDB Usage Collector Plugin

TimescaleDB storage-backend plugin that implements the Usage Collector `UsageCollectorPluginV1` SPI. It is the durable system of record for usage records and the usage-type catalog: the Usage Collector gateway gear discovers it via the types registry and dispatches all persistence to it. The plugin owns nothing of the host's domain logic — it is pure persistence over a TimescaleDB (PostgreSQL) database.

## Configuration

Config maps to `TimescaleDbPluginConfig` (`src/config.rs`). Durations are whole seconds (repo convention).

| Key | Default | Description |
| --- | --- | --- |
| `database_url` | _(required)_ | Postgres DSN; TLS required (use `sslmode=require`). |
| `pool_size_min` | `2` | Connection-pool lower bound. |
| `pool_size_max` | `16` | Connection-pool upper bound. |
| `connection_timeout_secs` | `10` | Connection acquire timeout (seconds). |
| `retention_period_secs` | `31536000` (365d) | `usage_records` retention window; chunks wholly older are dropped. |
| `vendor` | `cyberfabric` | Vendor name for GTS instance registration. |
| `priority` | `10` | Plugin priority (lower = higher precedence). |

```toml
[gears.timescaledb-usage-collector-plugin.config]
database_url = "postgres://user:pass@host:5432/usage?sslmode=require"
pool_size_min = 2
pool_size_max = 16
connection_timeout_secs = 10
retention_period_secs = 31536000
vendor = "cyberfabric"
priority = 10
```

## Storage semantics

- **Deduplication** — a dedicated `usage_dedup` table (normal table, `PRIMARY KEY (tenant_id, gts_id, idempotency_key)`) is the atomic 3-tuple uniqueness authority. Ingest runs in one transaction: it claims the dedup slot (`INSERT … ON CONFLICT DO NOTHING`) and inserts the record only if it won the slot; a concurrent same-key submission blocks on the slot's row lock and then resolves as a silent absorb (canonical-equal) or an `IdempotencyConflict`. This serializes the same-key/different-`created_at` case the hypertable's `UNIQUE (…, created_at)` constraint structurally cannot. A TimescaleDB cleanup job (`prune_usage_dedup`) reclaims a dedup row once its record's chunk has been dropped, keeping the table in step with retention.
- **Retention** — a declarative TimescaleDB retention policy is registered at init from `retention_period_secs`; TimescaleDB drops chunks wholly older than the window. No application-side deletion path.
- **Deactivation** — `deactivate` flips the target record and its depth-1 active compensations (`corrects_id` pointing at it) to `inactive` in a single transaction. The transition is one-way and the cascade is bounded to a single level.
- **Referential integrity** — `usage_records.gts_id` references `usage_type_catalog` with `ON DELETE RESTRICT`; deleting a referenced usage type fails and is mapped to `UsageTypeReferenced` rather than orphaning records.

## SPI conformance

The crate implements `usage_collector_sdk::UsageCollectorPluginV1` (via `StorageAdapter` over the record and catalog stores). Conformance is enforced at compile time: the adapter satisfies the trait, so a drift between the SPI and this backend is a build error.

## Running integration tests

The real-DB suites are gated behind the `postgres` feature and require Docker plus the `timescale/timescaledb` image (pulled on demand via `testcontainers`):

```sh
cargo test -p cf-gears-timescaledb-usage-collector-plugin --features postgres
```

Without the feature, only the unit tests run (no Docker needed).

## Design

See [`DESIGN.md`](../../docs/timescaledb-usage-collector-storage-plugin/DESIGN.md) for the full architecture, sequences, schema, and constraint catalog.
