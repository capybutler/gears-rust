# TimescaleDB Usage Collector Plugin

TimescaleDB storage-backend plugin that implements the Usage Collector `UsageCollectorPluginV1` SPI. It is the durable system of record for usage records: the Usage Collector gateway gear discovers it via the types registry and dispatches all persistence to it. The plugin owns nothing of the host's domain logic — it is pure persistence over a TimescaleDB (PostgreSQL) database.

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

- **Deduplication** — the `usage_records` hypertable's own `UNIQUE (tenant_id, gts_type_id, idempotency_key, window_start, window_end)` (`usage_records_dedup_uniq`, `migrations/0001_init.sql`) is the dedup authority, enforced via `INSERT … ON CONFLICT … DO NOTHING RETURNING`. A returned row is a fresh insert; on conflict the existing row is read and resolved as a silent absorb (canonical-equal) or an `IdempotencyConflict`. `ON CONFLICT DO NOTHING` serializes concurrent same-key inserts (the loser blocks on the in-progress tuple until the winner commits). The dedup index rides the hypertable's chunk lifecycle — no separate dedup table, no cleanup job. **Note:** the dedup identity is the gear's DESIGN §3.7 5-tuple verbatim, not a divergence from it — one stable per-meter `idempotency_key` therefore covers many covered periods, and a replay of that key over a *different* `(window_start, window_end)` is a distinct entry by design rather than an `IdempotencyConflict`.
- **Retention** — a declarative TimescaleDB retention policy is registered at init from `retention_period_secs`; TimescaleDB drops chunks wholly older than the window. No application-side deletion path.
- **Invalidation** — there is no mutation path. A withdrawal is an ordinary appended entry carrying `invalidates` (the entry it withdraws) and `reason_code`; the withdrawn entry is never rewritten. At most one accepted withdrawal per target is enforced by the partial unique index `usage_records_one_invalidation_uniq` over `(invalidates, window_end)` — see `DIVERGENCES.md` entry 21 for why that guarantee is conditional on the Ingestion Gateway and entry 22 for how a cross-call collision fails a `create_batch`.

## SPI conformance

The crate implements `usage_collector_sdk::UsageCollectorPluginV1` (via `StorageAdapter` over the record store). The trait impl makes SPI **signature** drift a build error — that is all compile time buys. **Behavioural** conformance is what `tests/contract_conformance_pg.rs` runs against a live container: the DESIGN §3.3 suite via `usage_collector_sdk::contract::run_all`. A green run covers **six** checks — the five of DESIGN's seven in `contract::IMPLEMENTED_CHECKS`, plus the one in `contract::ADDITIONAL_CHECKS` that DESIGN obliges without tabulating. It is **not** a conformance certificate: the remaining two DESIGN checks are in `contract::BLOCKED_CHECKS`, which names what unblocks each, and nothing in the suite exercises the SPI's keyset obligations. Read the counts off those constants, not off this sentence.

## Running integration tests

The real-DB suites are gated behind the `postgres` feature and require Docker plus the `timescale/timescaledb` image (pulled on demand via `testcontainers`):

```sh
cargo test -p cf-gears-timescaledb-usage-collector-plugin --features postgres
```

Without the feature, only the unit tests run (no Docker needed).

## Design

See [`DESIGN.md`](docs/DESIGN.md) for the full architecture, sequences, schema, and constraint catalog.
