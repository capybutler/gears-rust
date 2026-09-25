# Usage Collector — Invalidation and dedup model (slice B)

**Date**: 2026-09-15
**Status**: Approved in brainstorming, pending spec review
**Scope**: `gears/system/usage-collector` (`usage-collector-sdk`, `usage-collector`,
`plugins/timescaledb-usage-collector-plugin`, `plugins/noop-usage-collector-plugin`),
`testing/e2e/suites/usage_collector`, `docs/api/api.json`
**Builds on**: slice A (`2026-09-15-usage-collector-record-shape-design.md`)

## 1. Why

`SPEC-DIFF.md` section 2 lists where the invalidation and dedup model in the code
disagrees with the spec. The code enforces "at most one invalidation" as a
store-side rule with its own plugin error and a partial unique index. The spec
makes it a plain dedup outcome of the derived `inv:<target>` key that slice A
introduced. The target lookup is unscoped and not converged-only. The dedup
comparison includes the server-assigned `origin` and compares quantities
numerically. Nothing declares a dedup level.

### Sources of truth

- `gears/system/usage-collector/docs/`: `DESIGN.md` (§3.1 invariants, §3.3 Plugin
  SPI, obligations and contract table, error lift table, invalidate sequence),
  `usage-collector-v1.yaml`, `ADR/0004`, `ADR/0010`.
- `SPEC-DIFF.md` **Decisions**: S-A1, S-B6, S-B7, S-C2, S-C3. Where a decision
  disagrees with the docs, the decision wins. Spec edits are applied separately.
- Stale, not a basis: `DECOMPOSITION.md`, `docs/features/*`.

## 2. Scope

**In scope (SPEC-DIFF items):**

| Item | Change |
| --- | --- |
| 2.1 | At most one invalidation is a dedup outcome; the store-side rule goes |
| 2.2 | `IdempotencyConflict` carries `existing`; `ALREADY_INVALIDATED` context names `invalidated_by` and `reason_code` |
| 2.3 | `converged_only` lookup, `UsageRecordNotConverged`, `TargetNotConverged` |
| 2.4 | Target lookup reads under the write permit's compiled scope |
| 2.5 | Dedup compares caller-supplied fields only; quantity compared as text |
| 2.6 | Gateway resolves same-identity entries within one batch |
| 2.7 | Declared dedup level, zero-valued late-convergence counter, `run_all` level input |
| 8.1 | Plugin error enum matches the spec variants (feed variant excepted) |
| 8.2 | Plugin API rustdoc states the §3.3 obligations |

Also the slice-A hand-offs for B: tighten tests that accept
`ALREADY_INVALIDATED | IDEMPOTENCY_CONFLICT`, and reject an explicit
`"idempotency_key": null` on an invalidation.

**Out of scope:**

- New contract checks (`converged-target-lookup`, `dedup-floor`,
  `dedup-concurrent`, `latest-tie-break`), the 11-check partition, and the
  TimescaleDB README deployment-guide items other than the dedup level → slice E
  (8.3, 8.4).
- `CursorBeyondRetention` → with the feed.
- The metric label vocabulary for the new conflict reasons → slice G (10.4).
- Pipeline order and `BACKFILL` action → slice C.

## 3. SDK (`usage-collector-sdk`)

### 3.1 Plugin API trait (`plugin_api.rs`)

```rust
async fn get_usage_record(
    &self,
    id: Uuid,
    scope: &ast::Expr,
    converged_only: bool,
) -> Result<UsageRecord, UsageCollectorPluginError>;
```

- `create_usage_record` / `create_usage_records` docs lose the at-most-one store
  obligation. They state that a collision is decided by the dedup identity and
  resolved by comparing caller-supplied fields
  (`UsageRecord::caller_supplied_eq`), and that same-identity entries in one
  batch call resolve later against earlier (S-B6).
- The trait-level doc restates the DESIGN §3.3 obligations:
  - no re-validation; a host-contract breach is `Internal`;
  - acknowledge only what is durable;
  - declare a dedup level and meet it; a second invalidation is an ordinary
    collision with no check of its own;
  - converged-only lookups: apply `scope` first, return the survivor once
    converged, never report an acknowledged retained entry missing, and answer
    `UsageRecordNotConverged` only until the plugin can decide.
- The `query_aggregated_usage_records` doc drops its reference to
  `AlreadyInvalidated` as "the store's admission-time rule".

### 3.2 Plugin error enum (`error.rs`)

```rust
pub enum UsageCollectorPluginError {
    Transient { detail: String, retry_after_seconds: Option<u64> },
    IdempotencyConflict { idempotency_key: String, existing: Box<UsageRecord> },
    UsageRecordNotFound { id: Uuid },
    UsageRecordNotConverged { id: Uuid },
    Internal(String),
}
```

- `AlreadyInvalidated` is removed.
- `existing` is the stored entry, boxed to keep the enum small.
- `CursorBeyondRetention` is not added; it comes with the feed.

### 3.3 `UsageQuantity` equality (S-B7)

- `PartialEq`, `Eq` and `Hash` are implemented by hand over
  `(mantissa, scale)`. `42.5 != 42.500`; `42.500 == 42.500`.
- Any ordering impl is removed or made consistent with this equality; planning
  checks which exist.
- Every hash-keyed or equality-based use of `UsageQuantity` is audited.
- The type doc points numeric comparison at `as_decimal()`.
- This makes the faithful-copy check textual without further change there.

### 3.4 `UsageRecord::caller_supplied_eq` (2.5)

```rust
impl UsageRecord {
    pub fn caller_supplied_eq(&self, other: &UsageRecord) -> bool;
}
```

- Destructures both sides, so a new field is a compile error until classified.
- Compares: `tenant_id`, `gts_type_id`, `idempotency_key`, `resource_ref`,
  `subject_ref` (whole `Option`), `window_start` and `window_end` (as instants),
  `quantity` (textual, §3.3), `metadata`, `invalidation` (target and
  `reason_code`).
- Ignores: `id`, `accepted_at`, `origin`.
- Used by the reference backend, the TimescaleDB plugin and the gateway's batch
  resolution.

### 3.5 Caller-facing errors (2.2, 2.3)

- `reason.rs`: `TARGET_NOT_CONVERGED` const and
  `ConflictReason::TargetNotConverged`, with `from_wire`/`as_wire` arms and
  `reason_tests.rs` coverage.
- `UsageCollectorError::Conflict` gains:

  ```rust
  invalidated_by: Option<Uuid>,
  reason_code: Option<ReasonCode>,
  ```

  both `Some` only for `AlreadyInvalidated`.
- Constructors:
  - `already_invalidated(target: Uuid, invalidated_by: Uuid, reason_code: ReasonCode)`;
  - `target_not_converged(target: Uuid)`, `name` = target;
  - `idempotency_conflict(idempotency_key, existing_id)` unchanged.
- Docs on `already_invalidated` and `ALREADY_INVALIDATED` drop "the store detects
  it atomically"; they say it is the dedup conflict of an invalidation.
- `is_retryable()` is unchanged: true for `ServiceUnavailable` alone (DESIGN
  §3.3). `TargetNotConverged` is retryable through the wire
  `context.retryable = true` only (§4.7).

### 3.6 `DedupLevel` (2.7)

```rust
pub enum DedupLevel {
    Linearizable,
    Eventual { convergence_bound: std::time::Duration },
}

pub async fn run_all(
    plugin: &dyn UsageCollectorPluginV1,
    level: DedupLevel,
) -> Vec<ContractViolation>;
```

Lives in `contract.rs`. In this slice only `at_most_one_invalidation` reads it.

## 4. Gateway (`usage-collector`)

### 4.1 Lifting plugin errors by entry kind (2.1, 2.2)

- `DomainError` variants:
  - `IdempotencyConflict { idempotency_key: String, existing_id: Uuid }`;
  - `AlreadyInvalidated { target: Uuid, invalidated_by: Uuid, reason_code: ReasonCode }`;
  - `TargetNotConverged { target: Uuid }`.

  Each maps to its §3.5 constructor in `From<DomainError> for UsageCollectorError`.
- `From<UsageCollectorPluginError> for DomainError` keeps the `Transient`,
  `Internal` and `UsageRecordNotFound` arms. `IdempotencyConflict` and
  `UsageRecordNotConverged` map to `Internal`, with a detail saying the error was
  lifted without its dispatch context. `is_plugin_error_exhaustive_today` follows
  the new enum.
- New `lift_dispatch_error(err: UsageCollectorPluginError, dispatched: &UsageRecord) -> DomainError`:
  - `IdempotencyConflict { existing, .. }` with `dispatched.invalidation = Some(inv)`
    → `AlreadyInvalidated { target: inv.target, invalidated_by: existing.id,
    reason_code: existing's reason code }`. If `existing` carries no invalidation,
    that is a plugin breach → `Internal`.
  - `IdempotencyConflict` on a record → `IdempotencyConflict { existing_id: existing.id }`.
  - Anything else → `From`.
- Call sites: the single create dispatch, every batch slot, and the in-batch
  resolution (§4.5).
- Both target lookups map `UsageRecordNotConverged` → `TargetNotConverged`
  explicitly. The caller-facing `get_usage_record` passes
  `converged_only = false`, so `UsageRecordNotConverged` there reaches `From` and
  is `Internal`.

### 4.2 Scoped target lookup (2.4, S-A1)

- `authz::authorize_attribution_tuple` (and `authorize_usage_record`) return the
  permit's `AccessScope` instead of `()`, after the unchanged per-entry
  attribution gate.
- Single create path: for an invalidation, compile the scope with
  `authz::scope_to_odata_filter` and pass it as the target lookup's `scope`.
- Batch path: the per-`AttributionTupleKey` PDP result keeps its scope. It is
  compiled once per tuple group that contains an invalidation, and the fan-out
  looks each target up under the scope of the entry naming it. Distinct
  `(target, compiled scope)` pairs are fetched once each.
- A scope that does not compile fails closed: `PermissionDenied`, the same
  posture as the list path.
- `service::target_pinned_read_filter` and its "unscoped read" commentary are
  deleted.
- `invalidation.rs`: the `verify_invalidation_target` and module docs are
  restated for a scoped read. An out-of-scope target answers exactly as an absent
  one (`InvalidationTargetNotFound`). The rule that a rejection never echoes the
  target's values stays, as defence in depth.

### 4.3 Converged-only and same-batch targets (2.3, S-B6)

- Both target lookups pass `converged_only = true`; the fan-out cache stores a
  `NotConverged` outcome per target.
- A target lookup answering `UsageRecordNotFound` becomes `TargetNotConverged`
  when the target id equals the derived `id` of another entry in the same batch
  that projected successfully, at any position. "Projected" rather than
  "eligible", because eligibility is not final while targets are resolved. If
  that entry is rejected later, a retry gets a definite `NotFound`.
- This reveals nothing: the caller submitted that entry.
- The single-record path has no batch, so this rule does not apply there.

### 4.4 Faithful copy (S-B7)

- `quantity` compares textually through `UsageQuantity`'s equality (§3.3).
- The comment arguing for numeric comparison is replaced by the S-B7 rule: the
  copy repeats the quantity digit for digit.
- The module doc's "At-most-one-invalidation belongs to the store" bullet is
  replaced: it follows from the derived key and is lifted at dispatch.

### 4.5 One dispatch per identity in a batch (2.6, S-B6)

After eligibility is final and before dispatch:

1. Group eligible entries by `id`, in input order.
2. Dispatch only the first entry of each group. The SPI result-count check
   applies to the dispatched set.
3. Resolve every later entry `e` of a group from the first entry's plugin
   outcome:
   - `Ok(r)`: if `r.caller_supplied_eq(e)` → `Ok(r)`; else
     `lift_dispatch_error(IdempotencyConflict { existing: r }, e)`.
   - `Err(IdempotencyConflict { existing })`: the same comparison against
     `existing`.
   - Any other error: `e` gets the same caller-facing error as the first.

- Results stay aligned with input positions.
- An absorbed later entry reports the stored `accepted_at`.
- The single-record path is unchanged.
- The plugin keeps its own same-identity handling as defence in depth (S-B6).

### 4.6 Metrics

`classify_record_error` counts `ConflictReason::AlreadyInvalidated` and
`ConflictReason::TargetNotConverged` as `invalidation_rule`. A later entry
resolved at the gateway is counted like any other outcome. The label vocabulary is
reconciled in slice G.

### 4.7 REST

- `sdk_error_mapping::usage_record_error_to_problem` sets extra keys on the built
  `Problem.context` (a `serde_json::Value`):
  - `ALREADY_INVALIDATED`: `invalidated_by`, `reason_code`;
  - `TARGET_NOT_CONVERGED`: `retryable: true`.

  Planning verifies that per-entry rejections in a 207 go through this function.
  If not, they are routed through it.
- `CreateUsageRecordRequest.idempotency_key` distinguishes absent from explicit
  `null`:
  - `null` on an invalidation → `InvalidArgument`, `KEY_ON_INVALIDATION` on
    `idempotency_key`;
  - `null` on a record → `VALIDATION` on `idempotency_key`, as absent.

  The OpenAPI type becomes `string`, no longer `["string","null"]`. If the SDK's
  `CreateUsageRecord` deserialization shadow accepts a `null` today, it gets the
  same rule.
- Regenerate `docs/api/api.json` with `make openapi`. Prune resolved entries in
  `openapi_contract_tests.rs`.

## 5. Plugins

### 5.1 TimescaleDB migration (edit `0001_init.sql` in place)

- Drop `usage_records_one_invalidation_uniq`.
- Add a non-unique lookup index for the fold's withdrawal exclusion:

  ```sql
  CREATE INDEX IF NOT EXISTS usage_records_invalidates_idx
      ON usage_records (invalidates, window_end, type_key)
      WHERE invalidates IS NOT NULL;
  ```

  Its comment says it is a lookup index, not a rule.
- `0002_usage_rollup.sql` header: rollup exactness rests on the dedup identity and
  the derived invalidation key, not the removed index.

### 5.2 TimescaleDB store

- `error.rs`: remove `ONE_INVALIDATION_UNIQUE` and its error class.
- `record_store.rs`: remove the one-invalidation arm of `map_insert_error`,
  `find_existing_invalidation`, `BatchPlan::duplicate_withdrawals`,
  `duplicate_withdrawal_in_batch`, and the pre-rejected branch of
  `resolve_batch`, together with the doc text about them. A second withdrawal now
  collides on `usage_records_dedup_uniq`. A collision with a row written by an
  earlier call resolves per row, so `create_batch` no longer fails whole.
- `resolve_dedup_hit`: map the stored row to `UsageRecord` with the existing
  mapper. Absorb when `stored.id == incoming.id && stored.caller_supplied_eq(incoming)`
  (the id comparison is defence in depth). Otherwise return
  `IdempotencyConflict { existing: Box::new(stored) }`. Delete `canonical_equal`.
- `get_usage_record(id, scope, converged_only)`: the plugin uses one pool on a
  single primary, so every lookup is converged. The flag is accepted and ignored,
  with a doc line saying why.

### 5.3 TimescaleDB metrics and README

- `infra/metrics.rs`: remove `uc_timescaledb_invalidation_rejected_rows_total`
  and the statement-level refused-invalidation instrument, with their `Metrics`
  methods. Add `uc_timescaledb_dedup_late_convergence_total`, registered and never
  incremented; its doc says it stays zero under `linearizable`.
- `README.md`:
  - The deduplication bullet declares the dedup level: `linearizable`,
    convergence bound zero, races resolved by Postgres commit order, convergence
    established at commit, late-convergence metric name.
  - The invalidation bullet and line-72 reference drop the index and the
    `DIVERGENCES.md` entries 21 and 22 citations (that file does not exist). A
    second withdrawal is an ordinary dedup collision.
  - Other §3.10 items stay for slice E.

### 5.4 TimescaleDB integration tests

- `records_ingest_integration_pg.rs`:
  - second withdrawal, same reason code → absorbed, returns the stored
    invalidation;
  - second withdrawal, different reason code → `IdempotencyConflict`, `existing`
    = the first invalidation;
  - a batch withdrawing a target already withdrawn by an earlier call → per-entry
    outcomes, other entries accepted;
  - two withdrawals of one target in one batch: same reason absorbed, different
    reason `IdempotencyConflict`;
  - remove refused-statement and in-batch-counter assertions.
- `schema_integration_pg.rs`: replace
  `the_at_most_one_invalidation_index_is_partial_and_unique` with a test that no
  unique index covers `invalidates` and `usage_records_invalidates_idx` exists.
- New:
  - an identical entry submitted with `origin` live, then backfill → absorbed
    (2.5);
  - `42.5` then `42.500` under one identity → `IdempotencyConflict` (S-B7).
- `tests/common/mod.rs`: update comments naming the removed index.

### 5.5 Noop plugin

Signature change only. It still answers `UsageRecordNotFound` whatever the flag.

## 6. Reference backend and contract harness (`usage-collector-sdk/src/contract/`)

### 6.1 Reference backend (`reference.rs`)

- `admit`: on an `id` collision, return the stored entry when
  `stored.caller_supplied_eq(&record)`; otherwise
  `IdempotencyConflict { existing: Box::new(stored.clone()) }`. Delete the
  at-most-one block and the doc paragraph on its ordering. `withdrawn_targets`
  stays.
- `get_usage_record(id, scope, converged_only)`: the in-memory ledger is always
  converged, so the flag is ignored, with a doc line. Scope handling is unchanged.
- `create_usage_records` already admits in input order, so same-identity pairs in
  one call resolve later against earlier.

### 6.2 Level wiring

- `contract_tests.rs` runs the reference backend with `DedupLevel::Linearizable`.
- `plugins/timescaledb-usage-collector-plugin/tests/contract_conformance_pg.rs`
  runs TimescaleDB with `DedupLevel::Linearizable`.

### 6.3 `at-most-one-invalidation` rewrite (`checks/at_most_one_invalidation.rs`)

`fixtures.rs` gains `fixture_invalidation_with_reason(target, reason)`. The
distinct-caller-key fixtures are removed.

**Separate calls.**

- Admit record R.
- Admit I₁ = invalidation of R with reason `a`.
- Resubmit with reason `a`: under `Linearizable`, the result is `Ok` and equals
  the stored I₁, including its `accepted_at`.
- Submit with reason `b`: under `Linearizable`, the result is
  `IdempotencyConflict` with `existing.id == I₁.id` and `existing`'s reason code
  `a`.

**One batch call.** On a fresh record, submit `[I(a), I(b)]`: under
`Linearizable`, the first is `Ok` and the second is `IdempotencyConflict`.

**`Eventual`.**

- Each divergent submission may be `Ok` or `IdempotencyConflict`.
- After sleeping `convergence_bound`, a ledger read shows exactly one invalidation
  of each target, and a `COUNT` fold over the range counts each withdrawn pair as
  zero.

**Every level.** Remove the comment accepting either error and every
`AlreadyInvalidated` expectation.

### 6.4 Mutants (`contract_mutants.rs`)

- Replace the mutants built on the old store rule with:
  - a backend that absorbs a withdrawal carrying a different reason code;
  - a backend that refuses a withdrawal carrying the same reason code.
- Each is asserted to trip `at-most-one-invalidation`.
- A backend that still compares `origin` has no contract check in this slice. It
  becomes a mutant with `dedup-floor` in slice E, and is covered meanwhile by the
  TimescaleDB integration test and host tests.
- The partition test, `IMPLEMENTED_CHECKS`, `BLOCKED_CHECKS` and the "seven"
  text are untouched (slice E).

## 7. E2E

- `test_invalidation_is_admitted_at_most_once`:
  - same reason code → accepted, returning the stored invalidation;
  - different reason code → 409 `ALREADY_INVALIDATED`, with `invalidated_by` and
    `reason_code` in `context`.
- Every other e2e or host test accepting `ALREADY_INVALIDATED | IDEMPOTENCY_CONFLICT`
  is tightened to the one expected reason.

## 8. Sequencing

Every commit compiles, passes its tests, is test-first and is signed off
(`git commit -s`). A caller-visible break takes `!` and a `BREAKING CHANGE:`
trailer.

1. **`feat(usage-collector-sdk)!`: textual quantity equality and
   caller-supplied comparison.** §3.3, §3.4, the §4.4 comment rewrite, and the
   hash/ordering audit.
2. **`feat(usage-collector)!`: converged-only target lookup.** `converged_only`
   in the trait, all three plugins and the reference; `UsageRecordNotConverged`;
   `TargetNotConverged` reason, constructor, `DomainError`, lift at both target
   lookups, and the REST `retryable` context; `get` passes `false`.
3. **`feat(usage-collector)!`: at most one invalidation through dedup.** One wide
   commit, split by file group during execution:
   - the §3.1 rustdoc and §3.2 enum;
   - the §3.5 `Conflict` fields;
   - §4.1 and the `ALREADY_INVALIDATED` REST context;
   - §5.1–§5.4;
   - §6.
4. **`fix(usage-collector)!`: scope the invalidation target lookup.** §4.2.
5. **`feat(usage-collector)`: resolve same-identity entries at the gateway.**
   §4.3 same-batch target and §4.5.
6. **`fix(usage-collector)!`: refuse an explicit null key; regenerate OpenAPI;
   tighten E2E.** §4.7 null handling, `api.json`, contract-test exceptions, §7.

## 9. Testing and verification

Host tests that pin the change, in addition to §5.4, §6.3 and §6.4:

- **SDK:**
  - `UsageQuantity`: `42.5 != 42.500`; equal values hash equally;
  - `caller_supplied_eq` flips for each compared field and ignores `id`,
    `accepted_at` and `origin`.
- **Lift:**
  - a plugin `IdempotencyConflict` on a record → `IDEMPOTENCY_CONFLICT` naming
    `existing.id`;
  - on an invalidation → `ALREADY_INVALIDATED`, `name` = target, context
    `invalidated_by` = `existing.id`, `reason_code` = the stored one;
  - through `From` without context → `Internal`.
- **Converged-only:**
  - a recording mock plugin sees `true` at both target lookups and `false` on
    `get`;
  - `UsageRecordNotConverged` at a target lookup → 409 `TARGET_NOT_CONVERGED`
    with `context.retryable = true`.
- **Scope:**
  - a faithful copy with one wrong field, targeting another tenant's row, answers
    404 identical to an absent target and never `INVALIDATION_FIELD_MISMATCH`;
  - a scope that does not compile → `PermissionDenied`;
  - the standard `OWNER_TENANT_ID In [...]` permit compiles and finds an
    in-scope target.
- **Batch:**
  - identical pair → one dispatch, both accepted with one row;
  - divergent record pair → second `IDEMPOTENCY_CONFLICT` naming the first's id;
  - invalidation pair with different reason codes → second
    `ALREADY_INVALIDATED`;
  - first entry conflicting with a stored row → a later entry equal to that row
    is accepted with it;
  - first entry `Transient` → later entry the same error;
  - invalidation whose target is another entry of the batch →
    `TARGET_NOT_CONVERGED`.
- **REST:**
  - `"idempotency_key": null` on an invalidation → `KEY_ON_INVALIDATION`;
  - on a record → `VALIDATION`.

Verification before claiming completion:

- `cargo fmt --check`, workspace `cargo clippy` (pedantic, deny).
- `cargo nextest run -p cf-gears-usage-collector-sdk -p cf-gears-usage-collector
  -p cf-gears-noop-usage-collector-plugin -p cf-gears-timescaledb-usage-collector-plugin`.
- `make test-usage-collector-pg` (needs Docker).
- `make openapi` leaves `docs/api/api.json` unchanged after regeneration.
- The e2e usage-collector suite if it runs locally; otherwise report it was not
  run.

## 10. Risks

- **Commit 3 is wide.** The compiler is the completeness check.
- **`UsageQuantity` hashing.** Changing `Hash` changes any grouping keyed on it.
  Commit 1 audits every use.
- **Scope compile versus the attribution gate.** A permit shape the gate admits
  but `scope_to_odata_filter` rejects would deny withdrawals of records the caller
  could emit. Both reject the same tree predicates. If a live permit shape
  diverges, execution stops and asks.
- **Batch fan-out keyed by scope.** Tuple groups with different scopes can fetch
  one target more than once. That is correct, and bounded by the batch cap.
- **Gateway-absorbed entries report the stored `accepted_at`.** Tests assert
  equality with the first entry's row, not a fresh instant.

## 11. Decisions

| Decision | Choice | Rejected |
| --- | --- | --- |
| Contract harness depth | Rewrite `at-most-one-invalidation`, reference backend, `DedupLevel` input; new checks stay in E | Compile-only edits; all of 8.3 in B |
| Textual quantity equality | `UsageQuantity`'s own `PartialEq`/`Eq`/`Hash` over mantissa and scale | Numeric `PartialEq` plus a `same_digits` helper |
| In-batch same identity | One dispatch per identity; later entries resolve against the stored entry (returned row or `existing`) | Pre-dispatch rejection against the earlier eligible entry; plugin only |
| Invalidation targeting an entry of the same batch | Lookup first; `NotFound` plus in-batch derived id → `TargetNotConverged` | `NotFound`; two dispatch rounds |
| Wire conflict context | Gear-local keys added to `Problem.context` | Extend `toolkit-canonical-errors` `AbortedV1`; typed SDK only, wire deferred |
| Conflict lift site | `lift_dispatch_error` at dispatch with the dispatched entry; context-free `From` yields `Internal` | Entry kind carried in the plugin error; post-hoc reclassification |
| `is_retryable` for `TargetNotConverged` | Unchanged (DESIGN); retryable via `context.retryable` | `is_retryable() == true` |
| Replacing the unique invalidation index | Non-unique partial lookup index on the same columns | No index |
