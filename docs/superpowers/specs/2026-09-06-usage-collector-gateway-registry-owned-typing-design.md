# Usage Collector gateway — align the code with the reworked metering model

**Date**: 2026-09-06
**Status**: Approved
**Scope**: `gears/system/usage-collector/usage-collector-sdk`,
`gears/system/usage-collector/usage-collector`,
`gears/system/usage-collector/plugins/noop-usage-collector-plugin`,
root `Cargo.toml`, `Makefile`, `testing/e2e/suites/usage_collector`

## 1. Why

Commit `67493dbd2` reworked the metering model in `docs/DESIGN.md`, the 15
ADRs, `docs/PRD.md`, `docs/usage-collector-v1.yaml`, and the two JSON schemas
under `docs/schemas/`. The code still implements the previous model. This spec
covers the code change that closes that gap for the type plane and the entry
model.

Four changes drive the work:

1. Type declarations move to `types-registry`. This gear holds no catalog.
2. A GTS type schema describes a meter. Its `x-gts-traits` carry the
   declaration attributes.
3. The aggregation fold is a declaration property. The aggregate request
   carries no fold parameter.
4. An entry covers a period. Corrections are appended invalidation entries,
   not status flips.

The usage feed is out of scope by decision. See [§7](#7-out-of-scope).

### Documents that are current, and documents that are stale

`DESIGN.md`, the ADR set, `PRD.md`, `usage-collector-v1.yaml`, and
`docs/schemas/*.json` carry the reworked model. They are the source of truth
for this work.

`docs/DECOMPOSITION.md` and all five files under `docs/features/` were **not**
reworked. Commit `67493dbd2` only removed an emoji from DECOMPOSITION's
headings. Both still describe the usage-type catalog, event deactivation, and
compensation. Do not implement from them. Refreshing them is separate work.

## 2. Current state

The SDK and the gear implement the previous model:

- A `UsageType` catalog with `create`, `get`, `list`, and `delete` on the SDK
  trait, the REST surface, and the Plugin SPI.
- `UsageKind` (`counter` / `gauge`), which gates which aggregation operation
  and which correction a caller may use.
- `AggregationSpec { op, group_by }`, so the caller picks the fold per request.
- Records carrying `created_at`, `status: active | inactive`, and
  `corrects_id`.
- `deactivate_usage_record`, a one-way `active -> inactive` flip that cascades
  depth-1 to referencing compensation rows.

Rate limiting (`cpt-cf-usage-collector-fr-rate-limiting`) is not implemented
today. No quota code and no `ResourceExhausted` construction exists. This is a
pre-existing gap, unrelated to typing.

## 3. Target state

### 3.1 Removed

Removed from every surface — SDK trait, REST, Plugin SPI, domain service, and
their tests:

`UsageType`, `UsageTypeQuery`, `UsageTypeFilterField`, `UsageTypeGtsId`,
`UsageKind`, `UsageRecordStatus`, `AggregationOp`, `AggregationSpec`,
`USAGE_TYPE_RESOURCE`, `create_usage_type`, `get_usage_type`,
`list_usage_types`, `delete_usage_type`, `deactivate_usage_record`, and the
record fields `corrects_id`, `status`, and `created_at`.

`AggregationOp::Avg` disappears with the rest. The declared fold set is
`SUM`, `COUNT`, `MAX`, `MIN`, `LATEST`, so `AVG` is not a fold this gear
serves and `LATEST` is new.

### 3.2 `MeterTypeId`

`MeterTypeId` replaces `UsageTypeGtsId`. The old newtype wrapped
`gts::GtsInstanceId`. A meter is a *type*, not an instance, so the new one
wraps `gts::GtsTypeId`.

Validation, from `docs/schemas/usage_record.v1.schema.json`:

- Matches `^gts\.cf\.core\.uc\.usage_record\.v1~[^\x00-\x1F\x7F~]+~$` — the
  base type plus exactly one derivation segment, terminated by `~`.
- Carries no ASCII control character. The entry identifier derivation
  concatenates this value under a `0x1F` separator, so a control character
  would break injectivity (ADR-0007).
- At most 512 characters.

### 3.3 Type Resolver

New module: `usage-collector/src/domain/type_resolver.rs`.

It resolves a `MeterTypeId` to a declaration through the `TypesRegistryClient`
already registered on `ClientHub`. The domain service uses that client for
plugin selection today, so no new dependency is introduced.

```rust
pub struct ResolvedDeclaration {
    pub gts_type_id: MeterTypeId,
    pub aggregation_fold: AggregationFold,
    pub canonical_unit: String,
    pub metadata_schema: Arc<CompiledMetadataSchema>,
    pub nominal_sampling_interval: Option<String>,
}
```

Resolution path:

1. `TypesRegistryClient::get_type_schema(type_id)` returns a `GtsTypeSchema`.
2. `GtsTypeSchema::effective_traits()` yields the merged `x-gts-traits`:
   `aggregation_fold`, `canonical_unit`, `retention`, and the optional
   `nominal_sampling_interval`.
3. `GtsTypeSchema::effective_properties()["metadata"]` yields the metadata
   subschema. It is compiled once per declaration with the `jsonschema` crate
   and held on the cached `Arc<ResolvedDeclaration>`.

`retention` is read but not exposed to the write or read paths. Per DESIGN
§3.3 the storage plugin reads retention from `types-registry` itself.

`nominal_sampling_interval` is exposed and never acted on, per DESIGN §3.1.

An entry whose declaration binds no `canonical_unit` is rejected.

**Cache posture.** TTL-based refresh, capacity-bounded, with single-flight
population so a cold key does not stampede the registry. On a registry error
the resolver serves a stale cached entry. It fails closed only when nothing is
cached for the key. This satisfies the DESIGN requirement that a registry
outage degrades the introduction of new types rather than the ingestion of
existing ones.

The TTL is load-bearing rather than a convenience. The types-registry rewrite
(its ADR-0005) states that caches may no longer treat a major-only GTS
identifier as immutable forever. A no-expiry cache would violate that once the
rewrite promotes its v2 routes onto v1.

### 3.4 SDK trait

Six methods, matching DESIGN §3.3 without `read_usage_feed`:

```rust
create_usage_record(ctx, CreateUsageRecord) -> UsageRecord
create_usage_records(ctx, Vec<CreateUsageRecord>) -> Vec<Result<UsageRecord, _>>
backfill_usage_records(ctx, Vec<CreateUsageRecord>) -> Vec<Result<UsageRecord, _>>
get_usage_record(ctx, Uuid) -> UsageRecord
query_aggregated_usage_records(ctx, MeterTypeId, TimeRange, &ODataQuery,
                               &[MetadataFilter], &[AggregationDimension])
    -> AggregationResult
list_usage_records(ctx, MeterTypeId, TimeRange, &ODataQuery,
                   &[MetadataFilter]) -> ODataPage<UsageRecord>
```

The aggregate method carries no fold parameter. A caller cannot select one.

### 3.5 Plugin SPI

Five methods, matching DESIGN §3.3 without `read_feed_page` and
`get_reconciliation_metadata`. Two shapes change beyond the entry model:

- `get_usage_record(id, scope: &ast::Expr)` gains the compiled PDP scope, so a
  row outside the caller's scope is not returned. The gateway produces the
  expression through the existing `authz::scope_to_odata_filter`.
- `query_aggregated_usage_records(gts_type_id, time_range, fold, query,
  metadata_filter, group_by)` takes the fold as a parameter. Declarations
  never reach the SPI.

### 3.6 Entry shape

`UsageRecord` carries:

`id`, `tenant_id`, `gts_type_id: MeterTypeId`, `resource_ref`, `subject_ref`,
`metadata`, `quantity: Decimal`, `window_start`, `window_end`,
`idempotency_key`, `accepted_at`, `acceptance_sequence: u64`,
`origin: RecordOrigin`, `invalidates: Option<Uuid>`,
`reason_code: Option<String>`.

`entry_type` is a **method**, not a stored field. It is derived from
`invalidates.is_some()` and serialized read-only. Deriving it is what stops a
submitted marker from disagreeing with the payload it marks.

`CreateUsageRecord` carries the caller-supplied fields only. It drops `id`,
`entry_type`, `origin`, `accepted_at`, and `acceptance_sequence`.

`acceptance_sequence` stays on the record although the feed is out of scope.
The `LATEST` tie-break reads it, and the plugin assigns it.

### 3.7 Identity derivation

`derive_usage_record_id` moves from the 4-tuple
`(tenant_id, gts_id, idempotency_key, created_at)` to the 5-tuple
`(tenant_id, gts_type_id, idempotency_key, window_start, window_end)`.

The namespace constant `56313026-863b-4de8-b32b-1f96b67306ed` does not change.
ADR-0007 fixes it permanently, and the value already in `id.rs` matches it.

The canonical pre-image follows ADR-0007 exactly:

| Input | Canonical form |
| --- | --- |
| `tenant_id` | Lowercase hyphenated 36-character RFC 4122 form, as UTF-8 |
| `gts_type_id` | The wire string, byte-exact UTF-8, terminator `~` included |
| `idempotency_key` | The wire string, byte-exact UTF-8 |
| `window_start` | `YYYY-MM-DDTHH:MM:SS.ffffffZ`, as UTF-8 |
| `window_end` | `YYYY-MM-DDTHH:MM:SS.ffffffZ`, as UTF-8 |

Separator is `0x1F` between the five inputs. The namespace enters the digest
as its 16 raw bytes, never as text.

Two preconditions are **validation errors, not truncation**:

- A window bound carrying finer than microsecond precision is rejected.
  Truncating would make a read-back entry derive an identifier different from
  the one it carries, which breaks offline reproduction.
- A second value of `60` is rejected, so no leap second enters the digest.

`created_at_micros` is replaced by a canonical 27-character timestamp
formatter shared by the derivation and its tests.

### 3.8 Ingestion pipeline

One choke point serves REST and SDK, live and backfill, single and batch:

1. Structural attribution validation — tenant, resource, optional subject,
   `gts_type_id`.
2. Idempotency key present.
3. Period validation — `window_start <= window_end`, UTC normalization,
   rejection of an offset-less timestamp, the microsecond ceiling, the leap
   second rule, and the path's time bounds.
4. Quantity range and precision.
5. PDP authorization, fanned out per distinct attribution tuple.
6. Type resolution, fanned out per distinct `gts_type_id`.
7. Metadata against the compiled declaration schema, and the size cap.
8. Invalidation rules.
9. Derive `id`. Stamp `accepted_at` and `origin`.
10. Dispatch to the active plugin.

The existing bounded fan-out structure carries over at concurrency 8. The
catalog fan-out becomes the resolver fan-out. The `corrects_id` L1-lookup
fan-out becomes the invalidation-target fan-out.

### 3.9 Invalidation rules

Invalidation replaces compensation and deactivation completely.

- **Both-or-neither.** `invalidates` and `reason_code` appear together or not
  at all.
- **Target resolves**, is itself a record (carries no `invalidates`), and is
  not already invalidated.
- **Faithful copy.** Every caller-supplied field equals the target's, except
  the three permitted departures: the entry's own idempotency key,
  `invalidates`, and `reason_code`. For `subject_ref`, presence against
  absence is a mismatch. The rejection names the field that differs.
- **Echo, not compensation.** The quantity restates what is withdrawn. It is
  never negated.
- **At most one** invalidation per record. The plugin enforces this atomically
  against the store. The gateway pre-checks.

The copied period is bounded by the path's period validation, not by a rule of
its own. Withdrawing a record older than the live past tolerance goes to the
backfill route.

### 3.10 Configuration

`[usage_collector]` gains:

| Key | Default | Purpose |
| --- | --- | --- |
| `live_future_tolerance` | 5 min | Live path upper bound on `window_end` |
| `live_past_tolerance` | 48 h | Live path lower bound on `window_end` |
| `backfill_window` | 90 d | Beyond it, backfill needs elevated authorization |
| `metadata_size_cap` | 8 KiB | Cap on the serialized metadata map. Matches the value `domain/validation.rs` already hard-codes, so replacing the constant with the config value changes no behaviour. |
| `type_cache_ttl` | 5 min | Type Resolver refresh interval |
| `type_cache_capacity` | 10 000 | Type Resolver entry ceiling |

The past-tolerance rejection names `POST /usage-collector/v1/records/backfill`
in its message, matching the OpenAPI description.

### 3.11 Query path

- `TimeRange` is a typed parameter. It is never a `$filter` conjunct.
- Selection is `from <= window_end < to` on every path, whatever the length of
  the period. No path matches by overlap or containment, and no path reads
  `window_start` to select.
- Filter and `group_by` surface: the eight fixed fields — `tenant_id`,
  `resource_id`, `resource_type`, `subject_id`, `subject_type`, `entry_type`,
  `origin`, `invalidates` — plus the queried type's declared metadata keys,
  recomputed per request from the resolved declaration.
- `gts_type_id` is reserved. A predicate touching it is rejected, not silently
  honored. Both window bounds are non-filterable.
- Order admissibility: mandatory fields only, one sort direction, and rejected
  when supplied alongside a cursor. A row-value keyset over a nullable column
  is unsound, so an order naming `subject_id`, `subject_type`, `invalidates`,
  or a metadata key is rejected rather than silently dropping those rows. The
  gateway appends `(window_end, id)` in the caller's direction before dispatch.
- Raw reads return withdrawn pairs as persisted. The aggregate path excludes
  both entries of a withdrawn pair.

### 3.12 Authorization

The PDP posture does not change: an inline `access_scope_with` call per
operation, then a post-permit check of the operation's attribution against the
returned scope.

Changes are confined to the vocabulary. The usage-type resource and its
actions are removed. The backfill path carries its own action so that a
submission beyond the configured backfill window can require elevated
authorization.

### 3.13 Errors

`ValidationReason` loses `GaugeCompensationRejected` and `OpNotAllowedForKind`
with `UsageKind`, and the usage-type reasons with the catalog.
`MissingTimeWindow` goes too: the time range is a typed mandatory parameter
rather than a `$filter` conjunct, so its absence is a required-parameter
violation on the REST surface and unrepresentable on the SDK trait.

It gains `FUTURE_WINDOW`, `PAST_WINDOW`, the faithful-copy reasons, and
`CURSOR_BEYOND_RETENTION` for the lifted plugin variant.

The gear does **not** define `INVALID_CURSOR`, `FILTER_MISMATCH`, or
`ORDER_WITH_CURSOR`. Those are owned upstream by `toolkit_odata` — declared on
its cursor error enum and mapped to `Problem` field violations in its
`problem_mapping.rs`. The gear surfaces them by propagating that error, and
duplicating them here would create a second place the same code can be read
and disagree.

`ConflictReason` loses `UsageTypeReferenced`, `AlreadyInactive`,
`CorrectsIdTargetsCompensation`, `CorrectsIdWrongScope`, and
`CorrectsIdInactive`. It keeps `IdempotencyConflict` and gains
`AlreadyInvalidated`.

`UsageCollectorPluginError` narrows to the six variants of DESIGN §3.3:
`Transient`, `Internal`, `IdempotencyConflict`, `AlreadyInvalidated`,
`UsageRecordNotFound`, and `CursorBeyondRetention`.

## 4. TimescaleDB plugin

The plugin is unwired from the build and left untouched on disk. Removed:

- the workspace member line in the root `Cargo.toml`
- the `Makefile` nextest target and the server feature-exclude entry
- the wiring in `testing/e2e/suites/usage_collector/config.yaml` and `e2e.yaml`

The crate stops building. Nothing references a crate that no longer compiles.
Re-wiring later is a revert of one commit.

The noop plugin is updated in lockstep with the SPI. It is the only remaining
implementation.

## 5. Implementation slices

Each slice cuts through the SDK, the SPI, the noop plugin, and the gear
together, and each ends with the workspace compiling and its tests passing.
The alternative — one crate at a time — leaves the tree red from the first
commit to the last, with no runnable test in between.

| # | Slice | Content |
| --- | --- | --- |
| 1 | Unwire TimescaleDB | [§4](#4-timescaledb-plugin). No code change elsewhere. |
| 2 | Type plane | Catalog removed. `MeterTypeId`, Type Resolver, declared fold, declaration-driven metadata and unit validation. |
| 3 | Time model | `created_at` becomes `[window_start, window_end)`. New identity derivation. Period-end selection. `TimeRange` parameter. |
| 4 | Correction model | `status` / `corrects_id` / `deactivate` become `entry_type` / `invalidates` / `reason_code` with the invalidation rules. |
| 5 | Origin and backfill | `RecordOrigin`, the backfill route, its window and elevated-authorization rule. |
| 6 | Errors and contract gate | Reason vocabularies. Plugin error taxonomy. OpenAPI drift gate re-enabled. |

Slices 3 and 4 both touch the ingestion path, so they are ordered rather than
parallel.

## 6. Testing

Test-driven per slice. Beyond porting the existing suites:

- **Type Resolver**: cache miss populates, hit serves, registry error serves a
  stale entry, registry error with nothing cached fails closed, a definite
  not-found fails closed, single-flight population under concurrent misses.
- **Identity derivation**: golden vectors pinning the namespace constant and
  the canonical pre-image; a `0x1F` byte in the idempotency key; sub-microsecond
  precision rejected; a `60` second rejected; a point event over equal bounds.
- **Invalidation**: one rejection test per faithful-copy field, including
  `subject_ref` presence against absence; invalidation of an invalidation
  rejected; both-or-neither in each direction.
- **Period-end selection**: boundary tests at `from` and `to`, an entry wider
  than the range selected by neither side, a point event needing no special
  case.
- **Query surface**: `gts_type_id` predicate rejected, window predicate
  rejected, nullable-column order rejected, order alongside cursor rejected.

The plugin contract suite of DESIGN §3.3 is built in `usage-collector-sdk`
behind a `contract` feature and validated against a purpose-written
`InMemoryReferencePlugin` — **not** against the noop plugin, which persists
nothing and so fails every behavioural check by construction rather than by
defect. Two of DESIGN's seven checks cannot be written against the SPI this
gear declares and are named in `BLOCKED_CHECKS` with what unblocks each:
`feed-snapshot-and-replay`, which needs a feed method the SPI does not declare,
and `latest-tie-break`, which needs an `acceptance_sequence` the record does
not carry.

### Traceability annotations

The code carries `@cpt-begin:` and `@cpt-dod:` markers naming feature IDs the
reworked DESIGN deleted — the event-deactivation state machine, the
compensation algorithms, the usage-type-lifecycle DoDs. An annotation whose
target ID no longer exists in DESIGN or PRD is removed. An annotation that
still resolves is kept. No new IDs are invented here: DECOMPOSITION and the
feature files are stale, and re-deriving them is separate work.

### OpenAPI drift gate

The six drift tests in
`usage-collector/src/api/rest/routes/openapi_contract_tests.rs` are `#[ignore]`d
today because the documents ran ahead of the code. They are re-enabled against
an explicit implemented-operation set, with `/feed` and `/reconciliation` named
in a not-yet-implemented list. Drift is then caught on every operation that
ships, and the two deferred paths are recorded in code rather than behind a
blanket ignore.

## 7. Out of scope

Stated so that no reader assumes an omission:

- **The usage feed** — `read_usage_feed`, `read_feed_page`, `FeedPage`,
  `FeedSubscription`, `FeedKeyset`, the watermark, and `GET /feed`.
- **Reconciliation** — `get_reconciliation_metadata` and `GET /reconciliation`.
- **The declaration mirror** of ADR-0015. It needs a durable table, and the
  gear owns no database today. The ADR calls the mirror temporary, and the
  types-registry rewrite already implements persistent storage on its internal
  v2 routes, which retires the mirror's premise. Flagged for a separate
  decision.
- **Ingestion quotas** — `cpt-cf-usage-collector-fr-rate-limiting`. Not
  implemented before this work either.
- **The TimescaleDB plugin port** to the new SPI.
- **Refreshing** `docs/DECOMPOSITION.md` and `docs/features/*.md`.

## 8. Risks

- **The types-registry rewrite.** Its ADR-0001 has domain gears persist an
  opaque Registry Reference UUID rather than the GTS identifier. This gear puts
  `gts_type_id` in the dedup 5-tuple and the deterministic identifier, so the
  two decisions collide. The collision lands when that gear promotes its v2
  routes onto v1, not now. The TTL cache of [§3.3](#33-type-resolver) is the
  part of the posture that survives either outcome.
- **Declaration immutability.** DESIGN §3.1 treats fold, unit, and metadata
  surface as immutable for a type's life, which is what makes read-time
  resolution safe. The types-registry ADR-0004 says a major-only identifier
  names a mutable entity. Serving stale on error is bounded by the TTL, which
  limits the exposure but does not remove it.
- **Test volume.** Roughly 15,000 of the gear's 28,000 lines are tests keyed to
  the old model. A large share is deleted rather than ported, and the slices
  are sized on that assumption.
