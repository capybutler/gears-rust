# Usage Collector — Record and ingestion wire shape (slice A)

**Date**: 2026-09-15
**Status**: Approved in brainstorming, pending spec review
**Scope**: `gears/system/usage-collector` (`usage-collector-sdk`, `usage-collector`,
`plugins/timescaledb-usage-collector-plugin`, `plugins/noop-usage-collector-plugin`),
`testing/e2e/suites/usage_collector`, `docs/api/api.json`

## 1. Why

The spec was reworked after the "new metering model" implementation
(`dd2eb7a60`). `SPEC-DIFF.md` at the repo root lists where the code now
disagrees with the spec. That work is split into seven slices (A–G), each with
its own spec, plan, and execution. This is slice A: the record shape and the
ingestion wire shape, which almost every later item touches.

### Sources of truth

- `gears/system/usage-collector/docs/`: `DESIGN.md`, `PRD.md`,
  `usage-collector-v1.yaml`, `ADR/*`, `schemas/*`.
- `SPEC-DIFF.md` **Decisions** table. Where a decision disagrees with the docs,
  the decision wins. The decided spec amendments are applied to the docs
  separately, not in this slice.
- Stale, not a basis: `DECOMPOSITION.md`, `docs/features/*`.

## 2. Scope

**In scope (SPEC-DIFF items):**

| Item | Change |
| --- | --- |
| 1.1 | Wire and Rust field `value` → `quantity` |
| 1.2 | Server-stamped `accepted_at`, persisted and returned |
| 1.3 | Idempotency key rules for invalidations and the `inv:` prefix |
| 1.4 | Quantity range and precision enforced, no silent rounding |
| 1.7 | Batch entries decoded one by one; cap checked before any entry |
| 1.8 | Attribution string limits, counted in characters |
| 4.1 | Batch cap is operator configuration |
| 11 | Stale code docs touched by the above |

**Out of scope, owned by later slices:**

- 4.3 (PDP before validation) → slice C, together with removing the `BACKFILL`
  action (3.1), so the attribution tuple key changes once.
- 1.5, 1.6 (aggregate body, default page size) → slice D.
- 2.5 (dedup compares caller-supplied fields only, `origin` excluded) → slice B.
- Per-entry ingestion metric labels and handler-level rejection counting
  (10.4) → slice G.
- Contract checks added or removed → slice E.

## 3. SDK (`usage-collector-sdk`)

### 3.1 `UsageQuantity`

```rust
pub struct UsageQuantity(Decimal); // private field

impl FromStr for UsageQuantity { /* pattern + ≤ 28 significant digits */ }
impl TryFrom<&str> for UsageQuantity { /* delegates to FromStr */ }
impl Display for UsageQuantity { /* exact, preserves scale */ }
impl UsageQuantity {
    pub fn as_decimal(&self) -> Decimal;
}
// serde: always a JSON string, deserialized through FromStr
```

- Accepted text matches `^-?(?:0|[1-9][0-9]{0,27})(?:\.[0-9]{1,28})?$`
  (`usage-collector-v1.yaml` `UsageQuantity`) **and** has at most 28
  significant digits, leading zeros excluded (ADR-0013).
- Anything else is `InvalidArgument` on field `quantity`, reason
  `QUANTITY_OUT_OF_RANGE`. This includes `1e3`, `+1`, `_` separators, leading
  zeros, more than 28 fraction digits and 29 significant digits. Nothing is
  rounded.
- Parsing is exact: the `Decimal` keeps the submitted scale, so `42.500`
  displays as `42.500`.
- **Negative zero is rejected** (`-0`, `-0.00`, …) with
  `QUANTITY_OUT_OF_RANGE`. `rust_decimal` renders it as `0`, and Postgres
  `numeric` has no negative zero, so it cannot round-trip digit for digit. This
  was decided on 2026-09-15. It is a small tightening of the published
  pattern, noted for the spec amendments. Every other accepted form was
  checked against `rust_decimal` 1.41 and round-trips exactly, including the
  28-digit corners and trailing zeros.
- `PartialEq` on `UsageQuantity` is derived, so it compares numerically
  (`42.5 == 42.500`). The faithful-copy check keeps today's numeric
  comparison. Textual comparison (decision S-B7) is slice B's change to 2.5.
- `AggregationBucket.value` (`BigDecimal`) is a different type and is not
  renamed.

### 3.2 Record model

- `UsageRecord.value` and `CreateUsageRecord.value` become
  `quantity: UsageQuantity`. The four private wire shadows
  (`UsageRecordWire`, `UsageRecordWireRef`, `CreateUsageRecordWire`,
  `CreateUsageRecordWireRef`) rename the field too.
- `UsageRecord` gains `accepted_at: OffsetDateTime`, serialized RFC 3339. It is
  not a field of `CreateUsageRecord`. The create shadow's
  `deny_unknown_fields` rejects a caller-supplied `accepted_at`.
- `try_into_usage_record(self, origin: RecordOrigin, accepted_at: OffsetDateTime)`
  takes the stamp from the gateway.
- Fix the `UsageRecord.value` doc that ties the sign to counter or gauge
  semantics. The sign is never constrained (DESIGN §3.1).

### 3.3 Idempotency keys

```rust
pub struct CreateUsageRecord {
    pub idempotency_key: Option<IdempotencyKey>,
    pub invalidation: Option<Invalidation>,
    pub quantity: UsageQuantity,
    // ...
}

impl IdempotencyKey {
    pub fn new(s: impl Into<String>) -> Result<Self, UsageCollectorError>;       // caller key
    pub fn for_invalidation(target: Uuid) -> Self;                               // "inv:" + lowercase hyphenated
    pub fn from_stored(s: impl Into<String>) -> Result<Self, UsageCollectorError>; // either form
}
```

- `new` keeps its checks (non-empty, ≤ 256, no control characters) and adds:
  a key starting with `inv:` is rejected with `RESERVED_KEY_PREFIX` on
  `idempotency_key`.
- `from_stored` applies the same checks except the prefix rule. Plugin mappers
  and `UsageRecord` deserialization use it, because stored invalidations carry
  `inv:` keys.
- `CreateUsageRecord` deserialization uses `new`.
- `UsageRecord.idempotency_key` stays non-optional.
- `try_into_usage_record` enforces key/invalidation exclusivity before
  deriving the id:
  - An invalidation with a key is rejected with `KEY_ON_INVALIDATION` on
    `idempotency_key`.
  - A record without a key is rejected with `VALIDATION` on
    `idempotency_key`.
  - An invalidation gets the key `IdempotencyKey::for_invalidation(target)`.
    The id is then derived by the unchanged `derive_usage_record_id`.
- Update the `derive_usage_record_id` doc: an invalidation's id is now a
  function of its target.

### 3.4 String limits (1.8)

All limits count characters, not bytes.

| Value | Max | Violation field |
| --- | --- | --- |
| `ResourceRef.resource_id` | 256 | `resource_ref.resource_id` |
| `ResourceRef.resource_type` | 256 | `resource_ref.resource_type` |
| `SubjectRef.subject_id` | 256 | `subject_ref.subject_id` |
| `SubjectRef.subject_type` | 256 | `subject_ref.subject_type` |
| `IdempotencyKey` | 256 | `idempotency_key` |
| `ReasonCode` | 128 | `reason_code` |

`ReasonCode` keeps rejecting control characters. The spec does not list that
rule. It is kept as a known divergence and not changed here.

### 3.5 Reasons

`reason.rs` gains `QUANTITY_OUT_OF_RANGE`, `RESERVED_KEY_PREFIX` and
`KEY_ON_INVALIDATION`: a const, a `ValidationReason` variant, and the
`from_wire`/`as_wire` arms each, plus `reason_tests.rs` coverage. They ride
`field_violations[].reason` (decision S-E2).

### 3.6 Golden vectors

`id_tests.rs` gains ADR-0007's missing required tests:

- a golden vector for an `inv:` key;
- an invalidation differs from its target only through the key;
- an `inv:` measurement key is rejected before derivation.

## 4. Gateway (`usage-collector`)

### 4.1 Batch cap as configuration (4.1)

- `UsageCollectorConfig.max_batch_records: usize`. Its default is a domain
  constant, 100. `validate()` bails on 0 with the existing
  `[usage_collector].<key> must be greater than 0` message style.
- Remove `pub const MAX_BATCH_RECORDS`.
  - `Service::new_with_metrics` takes the cap.
  - `Service::max_batch_records()` exposes it.
  - `module.rs` passes `cfg.max_batch_records`.
  - `ServiceFixture` gains `with_max_batch_records`.
- `uc_ingestion_batch_size` boundaries: the entries of `[1, 2, 5, 10, 20, 50, 100]`
  below the cap, then the cap itself. `UcMetricsMeter::new` and
  `build_default_adapter` take the cap. Update the callers in `module.rs`,
  `test_support::local_metrics` and `metrics_tests.rs`.
- Fix the `service.rs` doc claiming the YAML has `maxItems`.

### 4.2 Batch decoding (1.7)

- `CreateUsageRecordsRequest { records: Vec<serde_json::Value> }` keeps
  `deny_unknown_fields`. A body that is not JSON, or has no `records` array,
  is still a whole-request 400 from the `Json` extractor.
- `dispatch_usage_record_batch` receives the cap from `Service` and, in order:
  1. Checks `records.len()`. Zero or over-cap is a whole-request 400,
     `invalid_batch_size`, on `records`, before any entry is decoded.
  2. Decodes each entry with `serde_json::from_value::<CreateUsageRecordRequest>`.
     A failure becomes `Rejected { index, error }`: a 400 Problem with reason
     `VALIDATION`, whose field is the serde-reported field when available
     (unknown or missing field) and `records` otherwise.
  3. Converts decoded entries with `record_request_into_domain`, as today.
- `CreateUsageRecordRequest` gets `quantity: String` and
  `idempotency_key: Option<String>`. `record_request_into_domain` builds
  `UsageQuantity` and `IdempotencyKey::new` (when present) and maps errors as
  today. Key/invalidation exclusivity is left to the SDK
  (`try_into_usage_record`), so REST and in-process callers share one rule.
- `UsageRecordDto` gains `quantity: String` and `accepted_at` (RFC 3339) and
  loses `value`.

### 4.3 `accepted_at` stamping (1.2)

- The service's existing per-request `now` becomes `accepted_at`:
  `service.rs` `create_usage_record_inner` and `create_usage_records_inner`.
  It passes into `project_and_admit` → `try_into_usage_record`. Every entry of
  one request carries the same instant, and it is the same instant the
  covered-period bounds use.
- No clock abstraction is added.
- A dedup hit returns the plugin's row unchanged, so an absorbed retry carries
  the stored `accepted_at`.
- Pipeline order is unchanged (4.3 is slice C).

### 4.4 Invalidation path

- `invalidation.rs` `faithful_copy_mismatch` compares `quantity` where it
  compared `value`; `COMPARED_FIELDS` is renamed to match.
- Remove the "caller key is a permitted departure" comments: an invalidation
  carries no caller key.
- Target lookup, scope and error lifts are unchanged (slice B).

## 5. Plugins and contract harness

### 5.1 TimescaleDB plugin

Migration: edit `migrations/0001_init.sql` in place. The gear is unreleased,
and its header already records that precedent.

- `value numeric NOT NULL` → `quantity numeric NOT NULL`.
- `ingested_at timestamptz NOT NULL DEFAULT now()` →
  `accepted_at timestamptz NOT NULL`, with no default (the host supplies it).
- `idempotency_key text NOT NULL` is unchanged. Invalidations store the
  derived `inv:<target>`, so `usage_records_dedup_uniq` keeps working.
- `0002_usage_rollup.sql`: `value` → `quantity` in the rollup `SUM`.

Code:

- `entity.rs` `UsageRecordRow`: `quantity: Decimal`,
  `accepted_at: OffsetDateTime`. Update the struct doc, which no longer drops
  the timestamp.
- `mapper.rs`:
  - builds `UsageQuantity` from the column's decimal text;
  - builds the key with `IdempotencyKey::from_stored`;
  - carries `accepted_at`.
  
  A stored value that fails `UsageQuantity` validation maps to `Internal`.
- `record_store.rs`:
  - `accepted_at` joins `INSERT_COLUMNS`;
  - the binds rename.
  
  The single and batch conflict paths already return the stored row.
  `canonical_equal` compares `quantity` and keeps excluding `accepted_at`. Its
  `origin` comparison stays (slice B).
- `migration_probe.rs`: rename `ingested_at` in the column filter.
- Rename `value` in `query/aggregate.rs`, `query/rollup.rs`,
  `rollup_maintenance.rs` and their tests.
- `tests/*_pg.rs`: rename, and add an integration test showing that an absorbed
  retry returns the first submission's `accepted_at`.

### 5.2 Noop plugin

The field renames only. It echoes the host-stamped record.

### 5.3 Contract harness (`usage-collector-sdk/src/contract/`)

- `reference.rs` `admit` absorbs on equality **ignoring `accepted_at`**, so a
  retry with a fresh stamp is absorbed and returns the stored entry. The
  caller-supplied-fields helper, which also drops `origin`, is slice B.
- `fixtures.rs`:
  - `fixture_record*` takes `UsageQuantity` and stamps a fixed
    `CONTRACT_ACCEPTED_AT` (amended in planning: a constant, not a parameter,
    because no slice-A check varies it);
  - `fixture_invalidation` drops its key parameter, and the key is derived.
- `checks/quantity_round_trip.rs`: compare `quantity.to_string()` with the
  submitted text.
- `contract_mutants.rs` and `contract_tests.rs`: renames only. No check is added
  or removed.

## 6. E2E and OpenAPI

- `testing/e2e/suites/usage_collector/conftest.py`: `record_payload` sends
  `quantity`. It sends `idempotency_key` only for records.
- `test_integration_seams.py`: `value=` → `quantity=` and
  `body["value"]` → `body["quantity"]`. Leave the aggregate bucket
  `b["value"]` alone.
- Regenerate `docs/api/api.json` with `make openapi`.
- `openapi_contract_tests.rs`: remove exception entries that this slice
  resolves. Entries still divergent stay listed.

## 7. Sequencing

The workspace compiles and all tests pass at every commit. Each commit is
test-first and signed off (`git commit -s`). A wire-breaking commit takes a `!`
and a `BREAKING CHANGE:` trailer.

1. **SDK additions.** `UsageQuantity` with tests: pattern, the 28-significant-digit
   bound, string-only serde, `-0`, leading zeros, `1e3`, `+1`, 29 digits,
   29 fraction digits, and scale preservation. The three reasons. Nothing uses
   them yet.
2. **String limits.** Character caps and nested violation fields (§3.4).
3. **`quantity` rename.** `UsageQuantity` replaces `Decimal` on records across
   the SDK, DTOs, gateway, both plugins, migrations, contract harness and tests,
   in one commit. It cannot compile half-applied.
4. **`accepted_at`.** SDK field, service stamp, DTO field, TimescaleDB column
   and mapper, reference-plugin absorb comparison, and the absorbed-retry pg
   test.
5. **Key rules.** Optional caller key, `inv:` reservation, `for_invalidation`,
   `from_stored`, exclusivity checks, and golden vectors.
6. **Batch cap and decoding.** Config key, service/handler/metric plumbing,
   per-entry decoding with the cap first.
7. **E2E and OpenAPI.** E2E payloads, regenerated `api.json`, shrunk contract
   exception lists.

## 8. Testing and verification

Tests that pin the change:

- `dto_tests::usage_record_dto_serialises_exactly_the_declared_wire_keys`:
  the new key set includes `quantity` and `accepted_at`, and no `value`.
- `models_tests::both_entry_shapes_round_trip_through_their_own_codecs`:
  it covers `quantity` and `accepted_at`, and a record with an `inv:` stored key.
- Handler tests:
  - a malformed entry at index *k* (bad quantity, unknown field, missing field)
    gives 207 with only *k* rejected;
  - an over-cap body containing a malformed entry gives a whole-request 400 on
    `records`;
  - an empty body gives a whole-request 400.
- Service tests:
  - every dispatched record in one request carries one `accepted_at`, within
    readings taken before and after the call;
  - an invalidation with a key is rejected with `KEY_ON_INVALIDATION`;
  - a record with an `inv:` key is rejected with `RESERVED_KEY_PREFIX`;
  - a record with no key is rejected;
  - an invalidation's dispatched key equals `inv:<target>`.
- Config tests: the `max_batch_records` default is 100, and 0 is rejected.
- Metrics tests: bucket boundaries for a cap below, equal to and above 100.

Verification before claiming completion:

- `cargo fmt --check`, workspace `cargo clippy` (pedantic, deny).
- `cargo nextest run -p cf-gears-usage-collector-sdk -p cf-gears-usage-collector
  -p cf-gears-noop-usage-collector-plugin -p cf-gears-timescaledb-usage-collector-plugin`.
- TimescaleDB integration tests via `make test-usage-collector-pg`
  (needs Docker).
- `make openapi` leaves `docs/api/api.json` unchanged after regeneration.
- The e2e usage-collector suite if it runs locally. Otherwise report that it
  was not run.

## 9. Risks

- Commit 3 is wide and mechanical. Execution splits it by file group inside
  one commit, and the compiler is the completeness check.
- Textual fidelity of `UsageQuantity` over `rust_decimal`: see §3.1. The
  implementation stops and asks rather than guessing.
- `accepted_at` without a clock seam makes exact-value assertions impossible.
  Tests bound the value between two readings, and check that it is equal
  across entries of one request.

## 10. Decisions

| Decision | Choice | Rejected |
| --- | --- | --- |
| Spec amendments | Code only; SPEC-DIFF Decisions win | Folding doc edits into slices |
| Slice cadence | Slice by slice | Umbrella spec |
| 4.3 placement | Slice C | Slice A (awkward interim action), slice F |
| Quantity carrier | `UsageQuantity(Decimal)` newtype | Newtype storing source text; bare `Decimal` |
| Key model | One `IdempotencyKey`, split constructors, `Option` on create | Separate `StoredIdempotencyKey`; entry-kind enum |
| Missing record key reason | `VALIDATION` | A dedicated reason |
| `accepted_at` granularity | Once per request, reusing the bounds `now` | Per entry; injected clock |
| TimescaleDB migration | Edit `0001` in place, rename columns | New migration; keep `value` column name |
