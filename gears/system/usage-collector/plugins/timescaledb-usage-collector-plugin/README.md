# TimescaleDB Usage Collector Plugin

TimescaleDB storage-backend plugin that implements the Usage Collector `UsageCollectorPluginV1` SPI. It is the durable system of record for usage records: the Usage Collector gateway gear discovers it via the types registry and dispatches all persistence to it. The plugin owns nothing of the host's domain logic — it is pure persistence over a TimescaleDB (PostgreSQL) database.

## Configuration

Config maps to `TimescaleDbPluginConfig` (`src/config.rs`). Durations are whole seconds (repo convention).

| Key | Default | Description |
| --- | --- | --- |
| `database_url` | _(required)_ | Postgres DSN; TLS required (use `sslmode=require`). |
| `pool_size_min` | `2` | Connection-pool lower bound. |
| `pool_size_max` | `16` | Connection-pool upper bound (at least 2). The retention sweep holds one extra detached connection while it runs; budget Postgres `max_connections` for `pool_size_max + 1` per replica. |
| `connection_timeout_secs` | `10` | Connection acquire timeout (seconds). |
| `statement_timeout_secs` | `30` | Per-statement timeout on every request-path connection (seconds). |
| `chunk_time_interval_secs` | `604800` (7d) | Time width of new ledger chunks; a multiple of 3600; applies to chunks created afterwards. |
| `type_key_slice_width` | `1` | How many type keys share one chunk slice; applies to chunks created afterwards. See [Retention](#retention). |
| `retention_sweep_interval_secs` | `3600` (1h) | Seconds between retention sweeps. |
| `rollup_materialization_lag_secs` | `7200` (2h) | Buckets newer than this are answered from the ledger rather than materialised. Multiple of 3600, at least 3600, below `rollup_live_window_secs`. See [Aggregate path](#aggregate-path). |
| `rollup_live_window_secs` | `259200` (3d) | Reach of the frequent refresh policy. Size it to the gateway's live past tolerance (48h by default). Multiple of 3600. |
| `rollup_refresh_interval_secs` | `120` | Seconds between live refresh-policy runs. |
| `rollup_history_refresh_interval_secs` | `3600` (1h) | Seconds between history refresh-policy runs (backfill, old invalidations). |
| `vendor` | `cyberfabric` | Vendor name for GTS instance registration. |
| `priority` | `10` | Plugin priority (lower = higher precedence). |

```toml
[gears.timescaledb-usage-collector-plugin.config]
database_url = "postgres://user:pass@host:5432/usage?sslmode=require"
pool_size_min = 2
pool_size_max = 16
connection_timeout_secs = 10
statement_timeout_secs = 30
chunk_time_interval_secs = 604800
type_key_slice_width = 1
retention_sweep_interval_secs = 3600
rollup_materialization_lag_secs = 7200
rollup_live_window_secs = 259200
rollup_refresh_interval_secs = 120
rollup_history_refresh_interval_secs = 3600
vendor = "cyberfabric"
priority = 10
```

## Storage semantics

- **Deduplication** — the `usage_records` hypertable's own `UNIQUE (tenant_id, gts_type_id, idempotency_key, window_start, window_end, type_key)` (`usage_records_dedup_uniq`, `migrations/0001_init.sql`) is the dedup authority, enforced via `INSERT … ON CONFLICT … DO NOTHING RETURNING`. A returned row is a fresh insert; on conflict the existing row is read and resolved as a silent absorb (canonical-equal) or an `IdempotencyConflict`. `ON CONFLICT DO NOTHING` serializes concurrent same-key inserts (the loser blocks on the in-progress tuple until the winner commits). The dedup index rides the hypertable's chunk lifecycle — no separate dedup table, no cleanup job. `type_key` is there because a hypertable UNIQUE must carry every partition column; a type's key never changes, so the dedup identity is still the 5-tuple. **Note:** the dedup identity is the gear's DESIGN §3.7 5-tuple verbatim, not a divergence from it — one stable per-meter `idempotency_key` therefore covers many covered periods, and a replay of that key over a *different* `(window_start, window_end)` is a distinct entry by design rather than an `IdempotencyConflict`.
- **Retention** — per GTS type, applied by the plugin's retention sweep; see [Retention](#retention).
- **Invalidation** — there is no mutation path. A withdrawal is an ordinary appended entry carrying `invalidates` (the entry it withdraws) and `reason_code`; the withdrawn entry is never rewritten. At most one accepted withdrawal per target is enforced by the partial unique index `usage_records_one_invalidation_uniq` over `(invalidates, window_end, type_key)` — see `DIVERGENCES.md` entry 21 for why that guarantee is conditional on the Ingestion Gateway and entry 22 for how a cross-call collision fails a `create_batch`.

## Retention

This section is the plugin's deployment-guide statement of the retention it enforces per GTS type (the gear's `DESIGN.md` §3.10 item 5).

- **What is enforced.** Each type's **current** declared `retention` trait, read from `types-registry`, measured from the end of the covered period (`window_end`). The value must be a fixed-length ISO 8601 duration (`P125D`, `PT36H`); years and months are rejected.
- **How.** `usage_records` is partitioned on `window_end` and on a per-type integer `type_key` (`usage_type_key`). A background retention sweep runs every `retention_sweep_interval_secs` and drops a chunk once every type in its key range has passed its retention. There is no table-wide TimescaleDB retention policy; startup removes one if an earlier build left it.
- **Amendments.** A changed retention applies to entries already stored, in both directions, from the next sweep.
- **Over-retention.** A chunk is dropped whole, so an entry can be held up to one `chunk_time_interval_secs` past its retention. With `type_key_slice_width` above 1, types share chunks and a shared chunk is held to the longest retention among them. Both are permitted: a purge may run later than the horizon, never earlier. A narrower race runs the other way: with `type_key_slice_width` above 1, a type that first receives a key after a sweep has already loaded the shared slice's chunks can write into a chunk that same sweep judged expired and drops, without the new type's retention having been considered — keep `type_key_slice_width` at 1 if that matters.
- **Unresolvable retention keeps data.** If the registry is unreachable, a type is not registered, or its `retention` is missing or invalid, the chunks holding it are kept and `uc_timescaledb_retention_chunks_kept_unresolved_total` grows under the matching `reason`. That counter is the signal to act on. The first sweep runs as soon as the plugin starts, before the registry may be ready, so a short burst of kept-unresolved chunks right after a restart is expected, not a regression.
- **Chunk count.** Roughly *(types ÷ `type_key_slice_width`)* × *(weeks retained at a 7-day interval)*, reported by `uc_timescaledb_chunks`. Keep it below 10 000: before it gets there, raise `type_key_slice_width` or `chunk_time_interval_secs`. Both apply to chunks created afterwards, so no data migration is needed. `uc_timescaledb_chunks` is set by whichever replica's sweep last held the retention lock; read the most recent value, not a max across replicas. Lookups by entry id (`get_usage_record`, used on every invalidation) carry no time or type predicate, so each one plans over every chunk and its cost grows with the chunk count — it can also briefly queue behind a chunk drop's lock. Measure by-id lookup latency before approaching the ceiling.
- **The floor is the deployer's.** A meter a charging consumer reads must declare at least the retention floor (backfill window plus one replay horizon, 125 days at the launch defaults; `cpt-cf-usage-collector-fr-billing-retention-floor`). The plugin cannot tell which meters those are and does not check it.

Sweep metrics: `uc_timescaledb_retention_sweeps_total{outcome}`, `uc_timescaledb_retention_sweep_duration_seconds`, `uc_timescaledb_retention_chunks_dropped_total`, `uc_timescaledb_retention_chunks_kept_unresolved_total{reason}`, `uc_timescaledb_retention_drop_failures_total`, `uc_timescaledb_chunks`.

## SPI conformance

The crate implements `usage_collector_sdk::UsageCollectorPluginV1` (via `StorageAdapter` over the record store). The trait impl makes SPI **signature** drift a build error — that is all compile time buys. **Behavioural** conformance is what `tests/contract_conformance_pg.rs` runs against a live container: the DESIGN §3.3 suite via `usage_collector_sdk::contract::run_all`. A green run covers **six** checks — the five of DESIGN's seven in `contract::IMPLEMENTED_CHECKS`, plus the one in `contract::ADDITIONAL_CHECKS` that DESIGN obliges without tabulating. It is **not** a conformance certificate: the remaining two DESIGN checks are in `contract::BLOCKED_CHECKS`, which names what unblocks each, and nothing in the suite exercises the SPI's keyset obligations. Read the counts off those constants, not off this sentence.

## Running integration tests

The real-DB suites are gated behind the `postgres` feature and require Docker plus the `timescale/timescaledb` image (pulled on demand via `testcontainers`):

```sh
cargo test -p cf-gears-timescaledb-usage-collector-plugin --features postgres
```

Without the feature, only the unit tests run (no Docker needed).

## Design

See [`DESIGN.md`](docs/DESIGN.md) for the full architecture, sequences, schema, and constraint catalog — but see `DIVERGENCES.md` entry 23 first: it still describes the pre-port model throughout, its schema included. `migrations/0001_init.sql` is the authority for the shipped schema, and it says so at its head.
