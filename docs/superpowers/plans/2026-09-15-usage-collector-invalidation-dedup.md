# Usage Collector — Invalidation and Dedup Model (Slice B) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make "at most one invalidation" a plain dedup outcome of the derived
`inv:<target>` key, and bring the dedup model in line with the spec:
- compare caller-supplied fields only, quantity as text;
- a converged-only target lookup under the write permit's scope;
- same-identity entries resolved at the gateway;
- a plugin error enum that matches the spec;
- a declared dedup level.

**Architecture:**
- **SDK:** the plugin API trait, its error enum and a shared
  `UsageRecord::caller_supplied_eq`.
- **Gateway:** chooses `AlreadyInvalidated` or `IdempotencyConflict` at dispatch,
  from the dispatched entry's kind (`lift_dispatch_error`); reads targets under
  the compiled `create` permit scope; sends one entry per identity to the plugin.
- **TimescaleDB:** drops its store-side index and resolves collisions with the
  shared comparison.
- **Contract harness:** the reference backend and `at-most-one-invalidation`
  follow the model.

**Tech Stack:** Rust 2024, `cargo nextest`, `clippy::pedantic` (deny),
`rust_decimal` 1.41, `time`, `serde`/`serde_json`, `thiserror`, `sqlx` +
TimescaleDB (Docker for pg tests), `utoipa` 5.5 via `toolkit_macros::api_dto`,
pytest e2e.

**Spec:** `docs/superpowers/specs/2026-09-15-usage-collector-invalidation-dedup-design.md`.
Background: `SPEC-DIFF.md` items 2.1–2.7, 8.1, 8.2, and the roadmap
`docs/superpowers/specs/2026-09-15-usage-collector-spec-diff-roadmap.md`.

## Global Constraints

- **Sources of truth:**
  - `gears/system/usage-collector/docs/{DESIGN.md,usage-collector-v1.yaml,ADR/*}`;
  - the `SPEC-DIFF.md` **Decisions** table, which wins where it disagrees.

  Do NOT edit spec docs. `DECOMPOSITION.md` and `docs/features/*` are stale.
- **Out of scope, do not touch:**
  - new contract checks, the 11-check partition, the `IMPLEMENTED_CHECKS` /
    `BLOCKED_CHECKS` constants and their "seven" text (slice E);
  - README items other than the dedup level (slice E);
  - metric label vocabulary (slice G);
  - pipeline order and the `BACKFILL` action (slice C);
  - `CursorBeyondRetention` (feed).
- **Quantity equality is textual:** `42.5 != 42.500` (decision S-B7).
- **Dedup comparison** (`UsageRecord::caller_supplied_eq`):
  - compares `tenant_id`, `gts_type_id`, `idempotency_key`, `resource_ref`,
    `subject_ref`, `window_start`, `window_end` (instants), `quantity`
    (textual), `metadata`, `invalidation`;
  - ignores `id`, `accepted_at`, `origin`.
- **`is_retryable()` does not change.** `TARGET_NOT_CONVERGED` is retryable only
  through the wire `context.retryable = true`.
- **Wire context keys:**
  - `ALREADY_INVALIDATED` carries `invalidated_by` (uuid string) and
    `reason_code`;
  - `TARGET_NOT_CONVERGED` carries `retryable: true`.
- **Commits:**
  - every commit compiles, passes `$LINT` and `$UNIT`;
  - signed with `git commit -s`;
  - a caller-visible break uses `!` plus a `BREAKING CHANGE:` trailer.
- **Package names:** `cf-gears-usage-collector-sdk`, `cf-gears-usage-collector`,
  `cf-gears-noop-usage-collector-plugin`, `cf-gears-timescaledb-usage-collector-plugin`.
- **`$UNIT`:**
  `cargo nextest run -p cf-gears-usage-collector-sdk --features contract -p cf-gears-usage-collector -p cf-gears-noop-usage-collector-plugin -p cf-gears-timescaledb-usage-collector-plugin`.
  If `--features contract` is rejected in a multi-package invocation, run the
  SDK separately with `cargo nextest run -p cf-gears-usage-collector-sdk --all-features`.
- **`$LINT`:**
  `cargo clippy -p cf-gears-usage-collector-sdk -p cf-gears-usage-collector -p cf-gears-noop-usage-collector-plugin -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings`
  plus `cargo fmt --all`.
- **`$PG`:** `make test-usage-collector-pg` (Docker). If Docker is unavailable,
  say so in the task report; never claim it passed.
- **New test code** uses fully qualified paths (`usage_collector_sdk::…`,
  `uuid::Uuid`, `toolkit_odata::ast::…`) wherever the file's existing imports
  are not shown in this plan, so nothing collides with an existing `use`.

Paths are relative to `gears/system/usage-collector/` unless they start with
`docs/` or `testing/`.

---

## File Structure

| File | Responsibility | Tasks |
| --- | --- | --- |
| `usage-collector-sdk/src/quantity.rs`, `quantity_tests.rs` | Textual equality and hash | 1 |
| `usage-collector-sdk/src/models.rs`, `models_tests.rs` | `caller_supplied_eq`; refuse explicit null key | 1, 6 |
| `usage-collector-sdk/src/plugin_api.rs` | `converged_only`; §3.3 obligations rustdoc | 2, 3 |
| `usage-collector-sdk/src/error.rs` | Plugin enum (`UsageRecordNotConverged`, `existing`, no `AlreadyInvalidated`); `Conflict` fields; constructors | 2, 3 |
| `usage-collector-sdk/src/reason.rs`, `reason_tests.rs` | `TARGET_NOT_CONVERGED` | 2 |
| `usage-collector-sdk/src/contract.rs`, `contract/reference.rs`, `contract/fixtures.rs`, `contract/checks/at_most_one_invalidation.rs`, `contract_mutants.rs`, `contract_tests.rs` | `DedupLevel`, reference admit, check rewrite, mutants | 2, 3 |
| `usage-collector/src/domain/error.rs`, `error_tests.rs` | `DomainError` variants, `lift_dispatch_error` | 2, 3 |
| `usage-collector/src/domain/service.rs`, `service_tests.rs`, `service_metrics_tests.rs` | Lookups, lift at dispatch, scope, per-identity dispatch | 2–5 |
| `usage-collector/src/domain/authz.rs`, `authz_tests.rs` | Return the permit scope | 4 |
| `usage-collector/src/domain/invalidation.rs`, `invalidation_tests.rs` | Textual copy; scoped-read docs | 1, 3, 4 |
| `usage-collector/src/domain/test_support.rs` | Converged-only knobs; scope-honouring target double | 2, 4 |
| `usage-collector/src/infra/sdk_error_mapping.rs`, `sdk_error_mapping_tests.rs` | Problem context keys | 2, 3 |
| `usage-collector/src/api/rest/dto.rs`, `handlers/usage_records.rs`, `handlers/usage_records_tests.rs` | Explicit null key | 6 |
| `plugins/timescaledb-usage-collector-plugin/migrations/{0001_init.sql,0002_usage_rollup.sql}` | Index swap, header | 3 |
| `plugins/timescaledb-usage-collector-plugin/src/infra/storage/{record_store.rs,record_store_tests.rs,error.rs,error_tests.rs}` | Store cleanup, dedup hit | 3 |
| `plugins/timescaledb-usage-collector-plugin/src/infra/{metrics.rs,metrics_tests.rs}` | Drop two counters, add late-convergence | 3 |
| `plugins/timescaledb-usage-collector-plugin/src/domain/adapter.rs` | `converged_only` | 2 |
| `plugins/timescaledb-usage-collector-plugin/tests/*.rs`, `README.md` | pg tests, dedup level | 3 |
| `plugins/noop-usage-collector-plugin/src/plugin.rs` | Signature | 2 |
| `testing/e2e/suites/usage_collector/test_integration_seams.py`, `docs/api/api.json` | E2E, OpenAPI | 6 |

---

### Task 1: Textual quantity equality and `caller_supplied_eq`

**Files:**
- Modify: `usage-collector-sdk/src/quantity.rs`, `usage-collector-sdk/src/quantity_tests.rs`
- Modify: `usage-collector-sdk/src/models.rs` (`impl UsageRecord`), `usage-collector-sdk/src/models_tests.rs`
- Modify: `usage-collector/src/domain/invalidation.rs`, `usage-collector/src/domain/invalidation_tests.rs`

**Interfaces:**
- Produces:
  - `impl PartialEq/Eq/Hash for UsageQuantity`, over `(mantissa, scale)`;
  - `UsageRecord::caller_supplied_eq(&self, other: &UsageRecord) -> bool`.

- [ ] **Step 1: Write the failing quantity tests.** In `quantity_tests.rs`,
  replace `equality_is_numeric_and_as_decimal_exposes_the_value` with:

```rust
#[test]
fn equality_is_textual_and_as_decimal_exposes_the_numeric_value() {
    let short = UsageQuantity::parse("42.5").unwrap();
    let long = UsageQuantity::parse("42.500").unwrap();
    assert_ne!(
        short, long,
        "equality is digit for digit: 42.5 and 42.500 are two quantities"
    );
    assert_eq!(short, UsageQuantity::parse("42.5").unwrap());
    assert_eq!(
        short.as_decimal(),
        long.as_decimal(),
        "the numeric value stays reachable through as_decimal"
    );
    assert_eq!(long.as_decimal(), Decimal::from_str("42.5").unwrap());
}

#[test]
fn hashing_agrees_with_textual_equality() {
    let set: std::collections::HashSet<UsageQuantity> = ["42.5", "42.500", "42.5", "-1", "0"]
        .into_iter()
        .map(|text| UsageQuantity::parse(text).unwrap())
        .collect();
    assert_eq!(
        set.len(),
        4,
        "equal texts hash together, differently scaled ones apart"
    );
}
```

- [ ] **Step 2: Write the failing `caller_supplied_eq` tests.** Append to
  `models_tests.rs`:

```rust
mod caller_supplied_eq {
    use std::collections::BTreeMap;

    use crate::models::{
        CreateUsageRecord, IdempotencyKey, Invalidation, MetadataKey, MeterTypeId, ReasonCode,
        RecordOrigin, ResourceRef, SubjectRef, UsageRecord,
    };
    use crate::quantity::UsageQuantity;

    const METER: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1~";
    const OTHER_METER: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.gb_hours.v1~";

    fn at(unix: i64) -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(unix).expect("valid timestamp")
    }

    fn record() -> UsageRecord {
        CreateUsageRecord {
            gts_type_id: MeterTypeId::new(METER).expect("valid meter id"),
            tenant_id: uuid::Uuid::from_u128(0xCA11),
            resource_ref: ResourceRef::new("res-1", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: UsageQuantity::parse("42.5").expect("valid quantity"),
            idempotency_key: Some(IdempotencyKey::new("idem-cse").expect("valid key")),
            invalidation: None,
            window_start: at(1_700_000_000),
            window_end: at(1_700_003_600),
        }
        .try_into_usage_record(RecordOrigin::Live, at(1_700_003_600))
        .expect("valid fixture")
    }

    #[test]
    fn server_assigned_fields_are_not_compared() {
        let a = record();
        let b = UsageRecord {
            id: uuid::Uuid::from_u128(1),
            accepted_at: a.accepted_at + time::Duration::minutes(5),
            origin: RecordOrigin::Backfill,
            ..a.clone()
        };
        assert!(a.caller_supplied_eq(&b));
    }

    #[test]
    fn every_caller_supplied_field_is_compared() {
        let base = record();
        let subject: SubjectRef =
            serde_json::from_value(serde_json::json!({ "subject_id": "sub-1" }))
                .expect("valid subject ref");
        let variants: Vec<(&str, UsageRecord)> = vec![
            ("gts_type_id", UsageRecord { gts_type_id: MeterTypeId::new(OTHER_METER).expect("valid"), ..base.clone() }),
            ("tenant_id", UsageRecord { tenant_id: uuid::Uuid::from_u128(0xCA12), ..base.clone() }),
            ("resource_ref", UsageRecord { resource_ref: ResourceRef::new("res-2", "compute.vm").expect("valid"), ..base.clone() }),
            ("subject_ref", UsageRecord { subject_ref: Some(subject), ..base.clone() }),
            ("metadata", UsageRecord { metadata: BTreeMap::from([(MetadataKey::new("region").expect("valid"), "eu".to_owned())]), ..base.clone() }),
            ("quantity", UsageRecord { quantity: UsageQuantity::parse("42.500").expect("valid"), ..base.clone() }),
            ("idempotency_key", UsageRecord { idempotency_key: IdempotencyKey::new("idem-other").expect("valid"), ..base.clone() }),
            ("invalidation", UsageRecord { invalidation: Some(Invalidation { target: uuid::Uuid::from_u128(9), reason: ReasonCode::new("emitter_defect").expect("valid") }), ..base.clone() }),
            ("window_start", UsageRecord { window_start: base.window_start - time::Duration::hours(1), ..base.clone() }),
            ("window_end", UsageRecord { window_end: base.window_end + time::Duration::hours(1), ..base.clone() }),
        ];
        for (field, other) in variants {
            assert!(!base.caller_supplied_eq(&other), "`{field}` must be compared");
        }
    }

    #[test]
    fn window_bounds_compare_as_instants() {
        let a = record();
        let offset = time::UtcOffset::from_hms(2, 0, 0).expect("valid offset");
        let b = UsageRecord {
            window_start: a.window_start.to_offset(offset),
            window_end: a.window_end.to_offset(offset),
            ..a.clone()
        };
        assert!(a.caller_supplied_eq(&b), "one instant under two offsets is one bound");
    }
}
```

- [ ] **Step 3: Write the failing faithful-copy test.** In
  `invalidation_tests.rs`, replace the whole `a_quantity_equal_at_another_scale_is_admissible`
  test, its comment included, with:

```rust
#[test]
fn a_quantity_at_another_scale_is_a_mismatch() {
    // SPEC-DIFF decision S-B7: the copy repeats the quantity digit for digit.
    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    entry.quantity = qty("42.500");
    assert_mismatch(&verify(&entry, &target), "quantity");
}
```

- [ ] **Step 4: Run the tests and watch them fail.**
  - Run: `cargo nextest run -p cf-gears-usage-collector-sdk -E 'test(equality_is_textual) | test(hashing_agrees) | test(caller_supplied_eq)'`
  - Expected: FAIL. `caller_supplied_eq` does not exist, and `assert_ne!` fails
    because equality is still numeric.

- [ ] **Step 5: Implement textual equality.** In `quantity.rs`:
  - change `#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]` to
    `#[derive(Debug, Clone, Copy)]`;
  - add the impls below;
  - replace the module doc line `//! Equality is numeric (\`42.5 == 42.500\`), the carrier's own.` with
    `//! Equality is textual: \`42.5\` and \`42.500\` are different quantities (SPEC-DIFF decision S-B7). Use [\`UsageQuantity::as_decimal\`] for numeric comparison.`

```rust
/// Digit for digit: the mantissa and the scale, so `42.5 != 42.500`. Negative
/// zero never parses, so a zero has one representation per scale.
impl PartialEq for UsageQuantity {
    fn eq(&self, other: &Self) -> bool {
        self.0.mantissa() == other.0.mantissa() && self.0.scale() == other.0.scale()
    }
}

impl Eq for UsageQuantity {}

impl core::hash::Hash for UsageQuantity {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.0.mantissa().hash(state);
        self.0.scale().hash(state);
    }
}
```

- [ ] **Step 6: Implement `caller_supplied_eq`.** In `models.rs`, inside
  `impl UsageRecord`, right after `entry_type`, add:

```rust
    /// Whether `other` carries the same caller-supplied fields as `self`.
    ///
    /// This is the dedup comparison (DESIGN §3.1 "Collision resolution"): two
    /// entries on one dedup identity are one entry when every field the caller
    /// supplied is equal, and a conflict otherwise. The three server-assigned
    /// fields are not compared: `id` (derived from the identity both sides
    /// already share), `accepted_at` (stamped afresh per request) and `origin`
    /// (the route, which a retry may change).
    ///
    /// `quantity` compares digit for digit, and the covered-period bounds as
    /// instants. Both sides are destructured, so a field added to
    /// [`UsageRecord`] fails to compile here until it is classified.
    #[must_use]
    pub fn caller_supplied_eq(&self, other: &Self) -> bool {
        let Self {
            id: _,
            gts_type_id,
            tenant_id,
            resource_ref,
            subject_ref,
            metadata,
            quantity,
            idempotency_key,
            accepted_at: _,
            origin: _,
            invalidation,
            window_start,
            window_end,
        } = self;
        let Self {
            id: _,
            gts_type_id: other_gts_type_id,
            tenant_id: other_tenant_id,
            resource_ref: other_resource_ref,
            subject_ref: other_subject_ref,
            metadata: other_metadata,
            quantity: other_quantity,
            idempotency_key: other_idempotency_key,
            accepted_at: _,
            origin: _,
            invalidation: other_invalidation,
            window_start: other_window_start,
            window_end: other_window_end,
        } = other;
        gts_type_id == other_gts_type_id
            && tenant_id == other_tenant_id
            && resource_ref == other_resource_ref
            && subject_ref == other_subject_ref
            && metadata == other_metadata
            && quantity == other_quantity
            && idempotency_key == other_idempotency_key
            && invalidation == other_invalidation
            && window_start == other_window_start
            && window_end == other_window_end
    }
```

- [ ] **Step 7: Rewrite the faithful-copy comment.** In `invalidation.rs`,
  replace the comment paragraph that starts `// That equality is \`UsageQuantity\`'s, derived from \`rust_decimal\`'s,`
  and ends `// The residue is that a withdrawal can serialize a different string than the entry it withdraws; no read path compares those strings.` with:

```rust
    // That equality is `UsageQuantity`'s, which is textual: the copy repeats
    // the quantity digit for digit (SPEC-DIFF decision S-B7), so `42.500`
    // does not copy `42.5`. The rejection names `quantity` and never the
    // target's value, for the oracle reason `verify_invalidation_target`
    // gives.
```

- [ ] **Step 8: Audit equality and hashing uses.**
  - Run: `grep -rn 'UsageQuantity' --include='*.rs' . | grep -n 'HashSet\|HashMap\|BTreeSet\|BTreeMap\|sort\|max()\|min()'`
  - Expected: no hash- or order-keyed use of `UsageQuantity`. Every quantity
    fold works on `BigDecimal` or `Decimal`.
  - If a use appears, stop and report it rather than changing its semantics.

- [ ] **Step 9: Run the tests.**
  - Run: `$UNIT` and `$LINT`.
  - Expected: PASS.
  - A test elsewhere that compared `42.5` with `42.500` and relied on numeric
    equality now fails. Fix the test to compare `as_decimal()`, or to use the
    same text, whichever matches what it asserts.

- [ ] **Step 10: Commit.**

```bash
git add -A gears/system/usage-collector
git commit -s -m "feat(usage-collector-sdk)!: compare quantities digit for digit

UsageQuantity equality and hashing are now textual, and UsageRecord gains
caller_supplied_eq, the dedup comparison over caller-supplied fields.

BREAKING CHANGE: an invalidation whose quantity differs from its target's in
scale alone (42.500 against 42.5) is now rejected as INVALIDATION_FIELD_MISMATCH."
```

---

### Task 2: Converged-only target lookup

**Files:**
- Modify: `usage-collector-sdk/src/plugin_api.rs`, `error.rs`, `reason.rs`, `reason_tests.rs`, `contract/reference.rs`, `contract_mutants.rs`, `contract/checks/*.rs` (call sites), `contract_tests.rs` (call sites)
- Modify: `usage-collector/src/domain/error.rs`, `error_tests.rs`, `service.rs`, `service_tests.rs`, `service_metrics_tests.rs`, `test_support.rs`
- Modify: `usage-collector/src/infra/sdk_error_mapping.rs`, `sdk_error_mapping_tests.rs`
- Modify: `plugins/noop-usage-collector-plugin/src/plugin.rs`, `plugins/timescaledb-usage-collector-plugin/src/domain/adapter.rs`

**Interfaces:**
- Produces:
  - `UsageCollectorPluginV1::get_usage_record(&self, id: Uuid, scope: &ast::Expr, converged_only: bool)`;
  - `UsageCollectorPluginError::UsageRecordNotConverged { id: Uuid }`;
  - `reason::TARGET_NOT_CONVERGED`, `ConflictReason::TargetNotConverged`;
  - `UsageCollectorError::target_not_converged(target: Uuid) -> Self`;
  - `DomainError::TargetNotConverged { target: Uuid }`;
  - `HappyPathPlugin::set_get_usage_record_not_converged(id: Uuid)`;
  - `HappyPathPlugin::get_usage_record_converged_only_flags() -> Vec<bool>`.

- [ ] **Step 1: Add the reason, with tests first.**
  - In `reason_tests.rs`:
    - add `(TARGET_NOT_CONVERGED, ConflictReason::TargetNotConverged),` to the
      list in `conflict_reason_round_trips_each_constant`;
    - add `TARGET_NOT_CONVERGED,` after `ALREADY_INVALIDATED,` in the `pin!`
      list.
  - In `reason.rs`, after the `ALREADY_INVALIDATED` const, add:

```rust
/// An invalidation named a target whose dedup identity has not converged yet.
/// Retryable: it clears within the plugin's convergence bound plus its
/// query-path lag bound.
pub const TARGET_NOT_CONVERGED: &str = "TARGET_NOT_CONVERGED";
```

  - Then add the variant `/// See [\`TARGET_NOT_CONVERGED\`].\n    TargetNotConverged,` after `AlreadyInvalidated`.
  - Add the arm `TARGET_NOT_CONVERGED => Self::TargetNotConverged,` to `from_wire`.
  - Add the arm `Self::TargetNotConverged => TARGET_NOT_CONVERGED,` to `as_wire`.

- [ ] **Step 2: Add the plugin variant and the caller-facing constructor.** In
  `usage-collector-sdk/src/error.rs`, after the `UsageRecordNotFound` variant of
  `UsageCollectorPluginError`, add:

```rust
    /// A converged-only lookup cannot yet decide whether `id` exists: its
    /// identity has not converged under the plugin's dedup level. Only
    /// `get_usage_record(.., converged_only = true)` may answer it. The
    /// gateway lifts it to a retryable `Conflict(TargetNotConverged)`.
    #[error("usage record not converged: {id}")]
    UsageRecordNotConverged {
        /// The `UsageRecord.id` the lookup named.
        id: Uuid,
    },
```

  In the `// ── Conflict / Aborted (409)` section of `impl UsageCollectorError`,
  after `idempotency_conflict`, add:

```rust
    /// An invalidation's target has not converged under the active plugin's
    /// dedup level. `name` is the target. Retryable through the wire
    /// `context.retryable = true` only: [`Self::is_retryable`] stays true for
    /// `ServiceUnavailable` alone (DESIGN §3.3).
    #[must_use]
    pub fn target_not_converged(target: Uuid) -> Self {
        Self::Conflict {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            name: target.to_string(),
            reason: ConflictReason::TargetNotConverged,
            detail: format!(
                "invalidates {target} names an entry that has not converged yet; retry"
            ),
        }
    }
```

- [ ] **Step 3: Change the trait signature.** In `plugin_api.rs`, change
  `get_usage_record` to the following, and insert the doc paragraph just
  before the `async fn` line, after the existing doc:

```rust
    ///
    /// **`converged_only`.** The gateway passes `true` when it resolves an
    /// invalidation's target and `false` on the caller-facing point read.
    /// With `true` the plugin applies `scope` first, returns the survivor once
    /// the identity has converged, never reports an acknowledged, retained
    /// entry missing, and answers
    /// [`UsageCollectorPluginError::UsageRecordNotConverged`] only until it can
    /// decide: within its convergence bound plus its query-path lag bound it
    /// returns the entry or [`UsageCollectorPluginError::UsageRecordNotFound`].
    /// A plugin whose every read is converged (a single linearizable primary)
    /// ignores the flag.
    async fn get_usage_record(
        &self,
        id: Uuid,
        scope: &ast::Expr,
        converged_only: bool,
    ) -> Result<UsageRecord, UsageCollectorPluginError>;
```

- [ ] **Step 4: Update every implementation.** Each implementation takes the
  new parameter. Run `grep -rn 'fn get_usage_record' --include='*.rs' . | grep -v 'api.rs\|local_client.rs\|service.rs\|_tests.rs:.*async fn get_usage_record_'`
  and change each of these:
  - **Reference backend** (`contract/reference.rs`): add `_converged_only: bool`.
    Add to its doc: `/// The in-memory ledger is always converged, so \`converged_only\` changes nothing.`
  - **Noop plugin** (`plugin.rs`): add `_converged_only: bool`.
  - **TimescaleDB adapter** (`adapter.rs`): add `_converged_only: bool`, keep
    `self.record.get(id, scope).await`, and add above it
    `// Single primary, one pool: every read is converged, so the flag changes nothing (DESIGN §3.3 converged-only lookups).`
  - **Contract mutants** (`contract_mutants.rs`, both impls): add
    `converged_only: bool`.
    - `WrappedReference` forwards it: `self.inner.get_usage_record(id, &only_this_row(id), converged_only)` and `self.inner.get_usage_record(id, scope, converged_only)`.
    - `MutantLedger` names it `_converged_only`.
  - **Gateway test support** (`test_support.rs`):
    - `MockPlugin` and `FoldingPlugin` take `_converged_only: bool`;
    - `HappyPathPlugin` takes `converged_only: bool` and calls
      `self.target_lookup.lookup(id, scope, converged_only)`.
  - **Service tests** (`service_tests.rs`): the inline plugin impl near
    `get_usage_record_plugin_transient_lifts_to_service_unavailable` takes
    `_converged_only: bool`.

  Then fix every call site: `cargo check --all-targets --all-features -p cf-gears-usage-collector-sdk`
  lists the calls in `contract/checks/*.rs` and `contract_tests.rs`. Each is a
  read that is not a target lookup, so append `, false` to each.

- [ ] **Step 5: Teach `TargetLookupDouble` the flag.** In `test_support.rs`:
  - add two fields to `TargetLookupDouble`:

```rust
    not_converged: Mutex<std::collections::BTreeSet<Uuid>>,
    converged_only: Mutex<Vec<bool>>,
```

  - add these methods:

```rust
    /// Mark `id` not yet converged: a converged-only lookup of it answers
    /// `UsageRecordNotConverged`. Checked after the transient knobs and before
    /// the not-found set.
    pub fn set_not_converged(&self, id: Uuid) {
        self.not_converged.lock().expect("mutex").insert(id);
    }
    /// The `converged_only` flag of every lookup, in call order.
    #[must_use]
    pub fn converged_only_flags(&self) -> Vec<bool> {
        self.converged_only.lock().expect("mutex").clone()
    }
```

  - change `lookup`:
    - its signature becomes `(&self, id: Uuid, scope: &ast::Expr, converged_only: bool)`;
    - right after pushing to `inputs`, push `self.converged_only.lock().expect("mutex").push(converged_only);`;
    - just before the `not_found` check, add:

```rust
        if converged_only && self.not_converged.lock().expect("mutex").contains(&id) {
            return Err(UsageCollectorPluginError::UsageRecordNotConverged { id });
        }
```

  - On `HappyPathPlugin` add:

```rust
    /// See [`TargetLookupDouble::set_not_converged`].
    pub fn set_get_usage_record_not_converged(&self, id: Uuid) {
        self.target_lookup.set_not_converged(id);
    }
    /// See [`TargetLookupDouble::converged_only_flags`].
    #[must_use]
    pub fn get_usage_record_converged_only_flags(&self) -> Vec<bool> {
        self.target_lookup.converged_only_flags()
    }
```

- [ ] **Step 6: Write the failing gateway tests.**
  - **`error_tests.rs`:** append:

```rust
#[test]
fn a_not_converged_answer_outside_a_target_lookup_is_internal() {
    let domain: DomainError =
        UsageCollectorPluginError::UsageRecordNotConverged { id: uuid::Uuid::from_u128(0xC0) }
            .into();
    assert!(matches!(domain, DomainError::Internal(_)), "got {domain:?}");
}

#[test]
fn target_not_converged_lifts_to_a_conflict_naming_the_target() {
    let target = uuid::Uuid::from_u128(0xC1);
    let sdk: UsageCollectorError = DomainError::TargetNotConverged { target }.into();
    match sdk {
        UsageCollectorError::Conflict { name, reason, .. } => {
            assert_eq!(name, target.to_string());
            assert_eq!(reason, ConflictReason::TargetNotConverged);
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
    assert!(
        !UsageCollectorError::target_not_converged(target).is_retryable(),
        "retryability rides the wire context, not is_retryable (DESIGN §3.3)"
    );
}
```

  - **`service_tests.rs`, batch module:** this is the module that defines
    `withdrawal_of(tenant_id, target, idem)` and `service_with_permit`. Append:

```rust
    #[tokio::test]
    async fn a_target_not_yet_converged_is_a_retryable_conflict() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5C1);
        let target = Uuid::from_u128(0x6C1);
        plugin.set_get_usage_record_not_converged(target);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.not_converged.records.v1",
        );

        let results = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![withdrawal_of(tenant_id, target, "idem-nc")],
            )
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::Conflict {
                reason: ConflictReason::TargetNotConverged,
                name,
                ..
            }) => assert_eq!(name, &target.to_string()),
            other => panic!("a not-converged target is TargetNotConverged, got {other:?}"),
        }
        assert_eq!(plugin.get_usage_record_converged_only_flags(), vec![true]);
        assert!(plugin.last_create_records_input().is_none(), "nothing is dispatched");
    }
```

  - **`service_tests.rs`, singular module:** this is the module that defines
    `counter_withdrawal`. Append:

```rust
    #[tokio::test]
    async fn create_usage_record_not_converged_target_is_a_retryable_conflict() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x70C);
        let target = Uuid::from_u128(0x80C);
        plugin.set_get_usage_record_not_converged(target);
        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.not_converged.records.v1",
        );

        let err = service
            .create_usage_record(
                &authenticated_ctx(),
                counter_withdrawal(tenant_id, target, "idem-nc"),
            )
            .await
            .expect_err("a not-converged target is refused");

        match err {
            UsageCollectorError::Conflict {
                reason: ConflictReason::TargetNotConverged,
                name,
                ..
            } => assert_eq!(name, target.to_string()),
            other => panic!("expected TargetNotConverged, got {other:?}"),
        }
        assert_eq!(plugin.get_usage_record_converged_only_flags(), vec![true]);
        assert!(plugin.last_create_record_input().is_none(), "nothing is dispatched");
    }
```

  - **`service_tests.rs`, `get_usage_record_happy_path_returns_loaded_record`:**
    append this assertion before the closing brace:

```rust
        assert_eq!(
            plugin.get_usage_record_converged_only_flags(),
            vec![false],
            "the caller-facing point read is not converged-only"
        );
```

  - **`service_metrics_tests.rs`:** in the classification `cases` array, after
    the `already_invalidated` case, add:

```rust
        (
            UsageCollectorError::target_not_converged(Uuid::from_u128(14)),
            RecordErrorCategory::InvalidationRule,
        ),
```

  - **`sdk_error_mapping_tests.rs`:**
    - add `UsageCollectorError::target_not_converged(uuid),` to
      `every_usage_record_surface_variant`;
    - append:

```rust
#[test]
fn target_not_converged_problem_is_409_and_marked_retryable() {
    let target = Uuid::from_u128(0xC2);
    let problem = usage_record_error_to_problem(UsageCollectorError::target_not_converged(target));
    assert_eq!(problem.status, Some(409));
    assert_eq!(
        problem_context_string(&problem, "reason").as_deref(),
        Some("TARGET_NOT_CONVERGED"),
    );
    assert_eq!(problem.context.get("retryable"), Some(&serde_json::Value::Bool(true)));
}

#[test]
fn an_idempotency_conflict_problem_carries_no_retryable_hint() {
    let problem = usage_record_error_to_problem(UsageCollectorError::idempotency_conflict(
        "k",
        Uuid::from_u128(1),
    ));
    assert_eq!(problem.context.get("retryable"), None);
}
```

- [ ] **Step 7: Run the new tests and watch them fail.**
  - Run: `cargo nextest run -p cf-gears-usage-collector -E 'test(not_converged) | test(converged) | test(get_usage_record_happy_path)'`
  - Expected: FAIL. `DomainError::TargetNotConverged` is missing, and the
    gateway passes no `converged_only`.

- [ ] **Step 8: Implement the domain lift.** In `usage-collector/src/domain/error.rs`:
  - after the `UsageRecordNotFound` variant add:

```rust
    /// An invalidation target lookup answered `UsageRecordNotConverged`.
    /// Raised only at the two target lookups, which read converged-only.
    #[error("invalidation target {target} has not converged")]
    TargetNotConverged { target: Uuid },
```

  - in `From<UsageCollectorPluginError> for DomainError`, before `other =>`, add:

```rust
            UsageCollectorPluginError::UsageRecordNotConverged { id } => Self::Internal(format!(
                "storage plugin answered UsageRecordNotConverged for {id} outside a converged-only lookup"
            )),
```

  - add `| UsageCollectorPluginError::UsageRecordNotConverged { .. }` to
    `is_plugin_error_exhaustive_today`;
  - in `From<DomainError> for UsageCollectorError`, after the `UsageRecordNotFound`
    arm, add:
    `DomainError::TargetNotConverged { target } => Self::target_not_converged(target),`

- [ ] **Step 9: Implement the service changes.** In `service.rs`:
  - **Single path** (`create_usage_record_inner`):
    - the target lookup becomes `plugin.get_usage_record(invalidation.target, &target_pinned_read_filter(invalidation.target), true)`;
    - add this arm before `Err(e) =>`:

```rust
                Err(UsageCollectorPluginError::UsageRecordNotConverged { .. }) => {
                    return Err(UsageCollectorError::target_not_converged(invalidation.target));
                }
```

  - **Batch fan-out** (`resolve_invalidation_targets`): replace
    `plugin.get_usage_record(target, &target_pinned_read_filter(target)),` and
    the following `.map_err(DomainError::from);` with:

```rust
                plugin.get_usage_record(target, &target_pinned_read_filter(target), true),
            )
            .await
            .map_err(|e| match e {
                UsageCollectorPluginError::UsageRecordNotConverged { .. } => {
                    DomainError::TargetNotConverged { target }
                }
                other => DomainError::from(other),
            });
```

  - **Point read** (`get_usage_record`): `plugin.get_usage_record(id, &scope_expr, false)`.
  - **`classify_record_error`:** in the `Conflict` arm, add
    `ConflictReason::TargetNotConverged => RecordErrorCategory::InvalidationRule,`
    after the `AlreadyInvalidated` line.

- [ ] **Step 10: Add the REST context key.** In `sdk_error_mapping.rs`:
  - import `ConflictReason` from `usage_collector_sdk`;
  - replace `usage_record_error_to_problem` with the code below;
  - in its doc, replace the sentence "Both paths now share the same lift, so a
    per-record and a whole-request rejection of the same error are
    byte-identical on the wire." with "Both paths share the canonical lift. This
    one also adds the conflict context keys the batch envelope publishes
    (`retryable`, and in slice B `invalidated_by` / `reason_code`). A
    whole-request rejection never carries a `Conflict`."

```rust
#[must_use]
pub(crate) fn usage_record_error_to_problem(err: UsageCollectorError) -> Problem {
    let extra = conflict_context_extras(&err);
    let mut problem = Problem::from(usage_collector_error_to_canonical_for_usage_record(err));
    if let Some(context) = problem.context.as_object_mut() {
        context.extend(extra);
    }
    problem
}

/// The `context` keys a conflict carries beyond the canonical `reason`, which
/// the platform `Aborted` context has no slot for (usage-collector-v1.yaml
/// `RejectedUsageRecord`).
fn conflict_context_extras(err: &UsageCollectorError) -> serde_json::Map<String, serde_json::Value> {
    let mut extra = serde_json::Map::new();
    if let UsageCollectorError::Conflict { reason, .. } = err
        && *reason == ConflictReason::TargetNotConverged
    {
        extra.insert("retryable".to_owned(), serde_json::Value::Bool(true));
    }
    extra
}
```

- [ ] **Step 11: Run everything.**
  - Run: `$UNIT` and `$LINT`.
  - Expected: PASS.

- [ ] **Step 12: Commit.**

```bash
git add -A gears/system/usage-collector
git commit -s -m "feat(usage-collector)!: converged-only invalidation target lookup

The Plugin API point lookup takes converged_only and may answer
UsageRecordNotConverged; the gateway reads invalidation targets converged-only
and lifts that answer to a retryable Conflict(TARGET_NOT_CONVERGED).

BREAKING CHANGE: UsageCollectorPluginV1::get_usage_record takes a third
parameter, and a withdrawal of a not-yet-converged target is answered 409
TARGET_NOT_CONVERGED with context.retryable = true."
```

---

### Task 3: At most one invalidation through dedup

One commit: removing `AlreadyInvalidated` from the plugin enum cannot compile
half-applied. Work through the file groups in order, and let
`cargo check --all-targets --all-features` be the completeness check between
groups.

**Files:**
- SDK: `usage-collector-sdk/src/{error.rs,plugin_api.rs,reason.rs,contract.rs,contract/reference.rs,contract/fixtures.rs,contract/checks/at_most_one_invalidation.rs,contract_mutants.rs,contract_tests.rs}`
- Gateway: `usage-collector/src/domain/{error.rs,error_tests.rs,service.rs,service_tests.rs,service_metrics_tests.rs,invalidation.rs,test_support.rs}`, `usage-collector/src/infra/{sdk_error_mapping.rs,sdk_error_mapping_tests.rs}`
- TimescaleDB:
  - `migrations/{0001_init.sql,0002_usage_rollup.sql}`
  - `src/infra/storage/{record_store.rs,record_store_tests.rs,error.rs,error_tests.rs}`
  - `src/infra/{metrics.rs,metrics_tests.rs}`
  - `tests/{records_ingest_integration_pg.rs,schema_integration_pg.rs,id_uniqueness_integration_pg.rs,contract_conformance_pg.rs,common/mod.rs}`
  - `README.md`

**Interfaces:**
- Consumes: `UsageRecord::caller_supplied_eq` (Task 1), `TARGET_NOT_CONVERGED` plumbing (Task 2).
- Produces:
  - `UsageCollectorPluginError::IdempotencyConflict { idempotency_key: String, existing: Box<UsageRecord> }`;
  - `UsageCollectorPluginError::idempotency_conflict(idempotency_key: impl Into<String>, existing: UsageRecord) -> Self`;
  - `UsageCollectorError::Conflict { resource_type, name, reason, invalidated_by: Option<Uuid>, reason_code: Option<ReasonCode>, detail }`;
  - `UsageCollectorError::already_invalidated(target: Uuid, invalidated_by: Uuid, reason_code: ReasonCode) -> Self`;
  - `DomainError::AlreadyInvalidated { target: Uuid, invalidated_by: Uuid, reason_code: ReasonCode }`;
  - `crate::domain::error::lift_dispatch_error(err: UsageCollectorPluginError, dispatched_invalidation: Option<&Invalidation>) -> DomainError`;
  - `usage_collector_sdk::contract::DedupLevel { Linearizable, Eventual { convergence_bound: std::time::Duration } }`;
  - `contract::run_all(plugin: &dyn UsageCollectorPluginV1, level: DedupLevel)`;
  - `contract::fixtures::fixture_invalidation_with_reason(target: &UsageRecord, reason: &str) -> Result<UsageRecord, String>`.

#### 3A — SDK error and plugin API

- [ ] **Step 1: Replace the plugin conflict variant.** In
  `usage-collector-sdk/src/error.rs`:
  - delete the `AlreadyInvalidated` variant of `UsageCollectorPluginError`;
  - replace `IdempotencyConflict` with the variant below;
  - add the constructor below to `impl UsageCollectorPluginError`.

```rust
    /// Idempotency conflict at the persistence boundary: an entry with this
    /// dedup identity is already stored, and its caller-supplied fields differ
    /// from the submission's ([`crate::models::UsageRecord::caller_supplied_eq`]).
    /// `existing` is the stored entry, so the gateway can name it, and when the
    /// dispatched entry is an invalidation, report the conflict as
    /// `AlreadyInvalidated` naming `existing.id` and its reason code.
    #[error("idempotency conflict: key {idempotency_key} already bound to record {}", .existing.id)]
    IdempotencyConflict {
        /// The dispatched entry's idempotency key (`inv:<target>` on an invalidation).
        idempotency_key: String,
        /// The stored entry the key is already bound to.
        existing: Box<crate::models::UsageRecord>,
    },
```

```rust
    /// Constructs a [`UsageCollectorPluginError::IdempotencyConflict`] against
    /// the stored entry.
    #[must_use]
    pub fn idempotency_conflict(
        idempotency_key: impl Into<String>,
        existing: crate::models::UsageRecord,
    ) -> Self {
        Self::IdempotencyConflict {
            idempotency_key: idempotency_key.into(),
            existing: Box::new(existing),
        }
    }
```

- [ ] **Step 2: Add the typed conflict fields.** In the same file, add these two
  fields to `UsageCollectorError::Conflict`, between `reason` and `detail`:

```rust
        /// For [`ConflictReason::AlreadyInvalidated`]: the stored invalidation
        /// that already withdrew the target. `None` for every other reason.
        invalidated_by: Option<Uuid>,
        /// For [`ConflictReason::AlreadyInvalidated`]: the reason code the stored
        /// invalidation carries. `None` for every other reason.
        reason_code: Option<crate::models::ReasonCode>,
```

  Then:
  - replace `already_invalidated` with the constructor below;
  - add `invalidated_by: None, reason_code: None,` to the `Self::Conflict`
    literals in `idempotency_conflict` and `target_not_converged`;
  - in the doc of `invalidation_target_not_record`, replace
    "The separate cap of one withdrawal per entry is the store's
    ([`Self::already_invalidated`]), not this check's." with
    "The separate cap of one withdrawal per entry follows from the derived
    `inv:<target>` key ([`Self::already_invalidated`]), not from this check."

```rust
    /// A second invalidation of a record under a reason code other than the
    /// stored one: the dedup conflict of an invalidation. Every invalidation of
    /// one record shares the derived `inv:<target>` key, so the store reports an
    /// ordinary `IdempotencyConflict`, and the gateway, knowing the dispatched
    /// entry is an invalidation, reports it as this. `name` is the target;
    /// `invalidated_by` and `reason_code` name the invalidation in place.
    #[must_use]
    pub fn already_invalidated(
        target: Uuid,
        invalidated_by: Uuid,
        reason_code: crate::models::ReasonCode,
    ) -> Self {
        Self::Conflict {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            name: target.to_string(),
            reason: ConflictReason::AlreadyInvalidated,
            detail: format!(
                "usage record {target} is already invalidated by {invalidated_by} with reason code {}",
                reason_code.as_str()
            ),
            invalidated_by: Some(invalidated_by),
            reason_code: Some(reason_code),
        }
    }
```

- [ ] **Step 3: Update the reason doc.** In `reason.rs`, replace the doc of
  `ALREADY_INVALIDATED` with:
  `/// A second invalidation of a record under another reason code: the dedup conflict of an invalidation, reported by the gateway.`

- [ ] **Step 4: Rewrite the plugin API rustdoc** (`plugin_api.rs`).
  - **Trait doc.** Replace the two lines above the `// @cpt-dod` comments
    ("Backend storage adapter trait …" through "… gateway's responsibility.")
    with:

```rust
/// Backend storage adapter trait implemented by
/// `usage-collector-plugin-<backend>` crates.
///
/// # Obligations (DESIGN §3.3)
///
/// - **Pure persistence.** Authorization, type resolution, shape and quantity
///   validation and the invalidation copy rules are the gateway's. A plugin
///   that observes a violation of one has observed a host-contract breach and
///   returns [`UsageCollectorPluginError::Internal`]; it does not re-validate.
/// - **Acknowledge only what is durable.** A persist call returns only after
///   every entry it reports accepted is durable. A plugin may buffer and
///   coalesce calls, but a buffer holds only unacknowledged entries.
/// - **Declare a dedup level and meet it** (DESIGN §3.1 "Dedup level", published
///   in the plugin's deployment guide). A collision on the dedup identity
///   resolves by [`UsageRecord::caller_supplied_eq`]: equal is absorbed and
///   returns the stored entry, different is
///   [`UsageCollectorPluginError::IdempotencyConflict`] carrying it. A second
///   invalidation of one record is an ordinary collision on its derived
///   `inv:<target>` key, with no check of its own.
/// - **Decide converged-only lookups.** See [`Self::get_usage_record`].
///
/// The contract suite in `usage_collector_sdk::contract` (feature `contract`)
/// is what binds an implementation to these.
```

  - **`create_usage_record` doc.** Replace everything from "An exact-equality
    retry …" to the end of that doc with:

```rust
    /// A collision on the dedup identity (and so on `id`, derived from it)
    /// resolves by comparing caller-supplied fields
    /// ([`UsageRecord::caller_supplied_eq`]). An equal submission is absorbed
    /// and the **stored** entry is returned, its `accepted_at` and `origin`
    /// included. A different one is
    /// [`UsageCollectorPluginError::IdempotencyConflict`] carrying the stored
    /// entry. The rule covers invalidations too: every invalidation of one
    /// record derives the same `inv:<target>` key, so a second one with the
    /// same reason code is absorbed and one with another reason code conflicts.
    /// There is no separate at-most-one check.
```

  - **`create_usage_records` doc.** Replace the text after "Persist a batch of
    usage records." with:

```rust
    ///
    /// Per-record outcomes are aligned with the input order, each resolved as
    /// [`Self::create_usage_record`] resolves one. Two same-identity entries in
    /// one call resolve later against earlier: the later is absorbed when its
    /// caller-supplied fields equal the earlier accepted entry's, and conflicts
    /// otherwise. The gateway already sends one entry per identity; this is
    /// defence in depth.
```

  - **Fold doc** (`query_aggregated_usage_records`). Replace the paragraph
    starting "This is a read-path obligation, distinct from the store's one
    admission-time invalidation rule" with:

```rust
    /// This is a read-path obligation. The gear does not enforce it — it
    /// dispatches this call and returns what the plugin computes — so the
    /// `invalidation-excluded-from-fold` contract test DESIGN §3.3 "Plugin SPI"
    /// requires of every conforming plugin is what binds an implementation to
    /// it.
```

#### 3B — Gateway

- [ ] **Step 5: Write the failing lift tests.** In `usage-collector/src/domain/error_tests.rs`:
  - delete `idempotency_conflict_lifts_to_conflict_keyed_by_existing_id` and
    `the_plugins_at_most_one_check_lifts_to_a_conflict`;
  - in `sdk_already_invalidated_is_not_retryable`, change the constructor call
    to `UsageCollectorError::already_invalidated(uuid::Uuid::nil(), uuid::Uuid::nil(), usage_collector_sdk::ReasonCode::new("emitter_defect").expect("valid reason code"))`;
  - append:

```rust
// ── Lifting a store conflict by the dispatched entry's kind ─────────

fn stored_entry(
    invalidation: Option<usage_collector_sdk::Invalidation>,
) -> usage_collector_sdk::UsageRecord {
    let key = invalidation.is_none().then(|| {
        usage_collector_sdk::IdempotencyKey::new("idem-stored").expect("valid key")
    });
    usage_collector_sdk::CreateUsageRecord {
        gts_type_id: MeterTypeId::new("gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1~")
            .expect("valid meter id"),
        tenant_id: uuid::Uuid::from_u128(0x7E57),
        resource_ref: usage_collector_sdk::ResourceRef::new("res-1", "compute.vm")
            .expect("valid resource ref"),
        subject_ref: None,
        metadata: std::collections::BTreeMap::new(),
        quantity: usage_collector_sdk::UsageQuantity::parse("1").expect("valid quantity"),
        idempotency_key: key,
        invalidation,
        window_start: time::OffsetDateTime::UNIX_EPOCH,
        window_end: time::OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
    }
    .try_into_usage_record(
        usage_collector_sdk::RecordOrigin::Live,
        time::OffsetDateTime::UNIX_EPOCH,
    )
    .expect("valid fixture")
}

#[test]
fn a_conflict_on_a_dispatched_record_is_an_idempotency_conflict_naming_the_stored_entry() {
    let existing = stored_entry(None);
    let err = UsageCollectorPluginError::idempotency_conflict("idem-stored", existing.clone());
    let domain = super::lift_dispatch_error(err, None);
    assert!(
        matches!(
            &domain,
            DomainError::IdempotencyConflict { idempotency_key, existing_id }
                if idempotency_key == "idem-stored" && *existing_id == existing.id
        ),
        "got {domain:?}"
    );
    match UsageCollectorError::from(domain) {
        UsageCollectorError::Conflict {
            name,
            reason,
            invalidated_by,
            reason_code,
            ..
        } => {
            assert_eq!(name, existing.id.to_string());
            assert_eq!(reason, ConflictReason::IdempotencyConflict);
            assert_eq!(invalidated_by, None);
            assert_eq!(reason_code, None);
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
}

#[test]
fn a_conflict_on_a_dispatched_invalidation_is_already_invalidated() {
    let target = uuid::Uuid::from_u128(0x7A);
    let stored_reason =
        usage_collector_sdk::ReasonCode::new("emitter_defect").expect("valid reason code");
    let existing = stored_entry(Some(usage_collector_sdk::Invalidation {
        target,
        reason: stored_reason.clone(),
    }));
    let dispatched = usage_collector_sdk::Invalidation {
        target,
        reason: usage_collector_sdk::ReasonCode::new("late_correction").expect("valid reason code"),
    };
    let err =
        UsageCollectorPluginError::idempotency_conflict(format!("inv:{target}"), existing.clone());
    match UsageCollectorError::from(super::lift_dispatch_error(err, Some(&dispatched))) {
        UsageCollectorError::Conflict {
            name,
            reason,
            invalidated_by,
            reason_code,
            ..
        } => {
            assert_eq!(name, target.to_string(), "the rejection names the target");
            assert_eq!(reason, ConflictReason::AlreadyInvalidated);
            assert_eq!(invalidated_by, Some(existing.id));
            assert_eq!(
                reason_code,
                Some(stored_reason),
                "the stored reason code, not the submitted one"
            );
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
}

#[test]
fn a_stored_entry_that_is_not_an_invalidation_is_an_invariant_breach() {
    let target = uuid::Uuid::from_u128(0x7B);
    let dispatched = usage_collector_sdk::Invalidation {
        target,
        reason: usage_collector_sdk::ReasonCode::new("emitter_defect").expect("valid reason code"),
    };
    let err = UsageCollectorPluginError::idempotency_conflict(
        format!("inv:{target}"),
        stored_entry(None),
    );
    assert!(matches!(
        super::lift_dispatch_error(err, Some(&dispatched)),
        DomainError::Internal(_)
    ));
}

#[test]
fn a_conflict_reaching_the_context_free_lift_is_internal() {
    let err = UsageCollectorPluginError::idempotency_conflict("idem-stored", stored_entry(None));
    assert!(matches!(DomainError::from(err), DomainError::Internal(_)));
}
```

- [ ] **Step 6: Implement the domain side.** In `usage-collector/src/domain/error.rs`:
  - add `Invalidation` and `ReasonCode` to the `use usage_collector_sdk::{…}`
    list;
  - replace the `AlreadyInvalidated` variant of `DomainError` with the variant
    below;
  - in `From<UsageCollectorPluginError>`:
    - replace the `IdempotencyConflict` arm with the arm below;
    - delete the `AlreadyInvalidated` arm;
    - remove `| UsageCollectorPluginError::AlreadyInvalidated { .. }` from
      `is_plugin_error_exhaustive_today`;
  - in `From<DomainError> for UsageCollectorError`, replace the
    `AlreadyInvalidated` arm with
    `DomainError::AlreadyInvalidated { target, invalidated_by, reason_code } => Self::already_invalidated(target, invalidated_by, reason_code),`;
  - append `lift_dispatch_error` (below) after the `From` impls.

```rust
    /// A dispatched invalidation collided with a stored invalidation of the
    /// same target under another reason code. Built by
    /// [`lift_dispatch_error`], never by the context-free `From`.
    #[error("usage record {target} is already invalidated by {invalidated_by}")]
    AlreadyInvalidated {
        target: Uuid,
        invalidated_by: Uuid,
        reason_code: ReasonCode,
    },
```

```rust
            // Which conflict this is depends on the entry that was dispatched,
            // which this conversion cannot see. `lift_dispatch_error` is the
            // lift for a dispatch result; reaching here is a host breach.
            UsageCollectorPluginError::IdempotencyConflict { existing, .. } => {
                Self::Internal(format!(
                    "storage plugin conflict on {} was lifted without its dispatched entry",
                    existing.id
                ))
            }
```

```rust
/// Lift a `create_usage_record(s)` outcome, knowing what was dispatched.
///
/// A store `IdempotencyConflict` is reported by the kind of the **dispatched**
/// entry (DESIGN §3.3 error lift table):
/// - on a record, as `IdempotencyConflict` naming the stored entry;
/// - on an invalidation, as `AlreadyInvalidated` naming the target, the stored
///   invalidation and its reason code. Every invalidation of one record derives
///   the same `inv:<target>` key, so a conflict on one can only be against
///   another invalidation of that target; a stored entry that is not one is a
///   plugin breach.
///
/// Every other error takes the context-free `From`.
pub(crate) fn lift_dispatch_error(
    err: UsageCollectorPluginError,
    dispatched_invalidation: Option<&Invalidation>,
) -> DomainError {
    let UsageCollectorPluginError::IdempotencyConflict {
        idempotency_key,
        existing,
    } = err
    else {
        return DomainError::from(err);
    };
    let existing = *existing;
    let Some(dispatched) = dispatched_invalidation else {
        return DomainError::IdempotencyConflict {
            idempotency_key,
            existing_id: existing.id,
        };
    };
    match existing.invalidation {
        Some(stored) => DomainError::AlreadyInvalidated {
            target: dispatched.target,
            invalidated_by: existing.id,
            reason_code: stored.reason,
        },
        None => DomainError::Internal(format!(
            "storage plugin reported entry {} as the stored invalidation of {}, but it carries no invalidation",
            existing.id, dispatched.target
        )),
    }
}
```

- [ ] **Step 7: Lift at dispatch in the service.** In `usage-collector/src/domain/service.rs`:
  - import `lift_dispatch_error` next to `DomainError`
    (`use crate::domain::error::{DomainError, lift_dispatch_error};`, or extend
    the existing `use` line);
  - **single path:** replace the final dispatch in `create_usage_record_inner`
    (`instrument_spi(… plugin.create_usage_record(record)) .await .map_err(…)`)
    with:

```rust
        let dispatched_invalidation = record.invalidation.clone();
        instrument_spi(
            self.metrics.as_ref(),
            PluginOp::CreateUsageRecord,
            plugin.create_usage_record(record),
        )
        .await
        .map_err(|e| {
            UsageCollectorError::from(lift_dispatch_error(e, dispatched_invalidation.as_ref()))
        })
```

  - **batch path:** in `create_usage_records_inner`, right after
    `let (indices, dispatched): (Vec<usize>, Vec<UsageRecord>) = eligible.into_iter().unzip();`,
    add
    `let dispatched_invalidations: Vec<Option<Invalidation>> = dispatched.iter().map(|record| record.invalidation.clone()).collect();`.
    Then replace the `for (index, spi_result) in indices.into_iter().zip(spi_results) { … }`
    body, keeping its `@cpt` markers, with:

```rust
            for ((index, spi_result), invalidation) in indices
                .into_iter()
                .zip(spi_results)
                .zip(dispatched_invalidations)
            {
                results[index] = Some(spi_result.map_err(|e| {
                    UsageCollectorError::from(lift_dispatch_error(e, invalidation.as_ref()))
                }));
            }
```

  - **`classify_record_error`:** replace the comment above
    `ConflictReason::AlreadyInvalidated =>` with
    `// A second invalidation of a record under another reason code: the dedup conflict of an invalidation, same family as the gateway's rules above.`
  - **`resolve_invalidation_targets` doc:** replace the paragraph starting
    "At-most-one-invalidation is deliberately **not** checked here." with:
    "At-most-one-invalidation is not checked here: it follows from the derived
    `inv:<target>` key, so a second invalidation collides on the dedup identity
    at the store and is lifted at dispatch ([`lift_dispatch_error`])."

- [ ] **Step 8: Update the invalidation module doc.** In `invalidation.rs`,
  replace the `* **At-most-one-invalidation** belongs to the store, …` bullet
  (four lines) with:

```rust
//! * **At-most-one-invalidation** is no check at all. Every invalidation of
//!   one record derives the same `inv:<target>` key, so a second one collides
//!   on the dedup identity; the gateway lifts that conflict as
//!   `AlreadyInvalidated` at dispatch (`error::lift_dispatch_error`).
```

- [ ] **Step 9: Add the REST context keys.** In `sdk_error_mapping.rs`:
  - add `..` to the `E::Conflict { resource_type, name, reason, detail }`
    destructure;
  - replace `conflict_context_extras` with:

```rust
fn conflict_context_extras(err: &UsageCollectorError) -> serde_json::Map<String, serde_json::Value> {
    let mut extra = serde_json::Map::new();
    if let UsageCollectorError::Conflict {
        reason,
        invalidated_by,
        reason_code,
        ..
    } = err
    {
        if *reason == ConflictReason::TargetNotConverged {
            extra.insert("retryable".to_owned(), serde_json::Value::Bool(true));
        }
        if let Some(invalidated_by) = invalidated_by {
            extra.insert(
                "invalidated_by".to_owned(),
                serde_json::Value::String(invalidated_by.to_string()),
            );
        }
        if let Some(reason_code) = reason_code {
            extra.insert(
                "reason_code".to_owned(),
                serde_json::Value::String(reason_code.as_str().to_owned()),
            );
        }
    }
    extra
}
```

- [ ] **Step 10: Rewrite the gateway tests that named the removed variant.**
  - **`sdk_error_mapping_tests.rs`:**
    - replace `already_invalidated_maps_to_409_aborted_with_already_invalidated_reason`
      with the test below;
    - in `every_usage_record_surface_variant`, change the `already_invalidated`
      entry to
      `UsageCollectorError::already_invalidated(uuid, Uuid::new_v4(), usage_collector_sdk::ReasonCode::new("emitter_defect").expect("valid reason code")),`.

```rust
#[test]
fn already_invalidated_problem_names_the_invalidation_in_place_and_its_reason_code() {
    let target = Uuid::from_u128(0xCAFE_BABE);
    let invalidated_by = Uuid::from_u128(0xFEED);
    let already = || {
        UsageCollectorError::already_invalidated(
            target,
            invalidated_by,
            usage_collector_sdk::ReasonCode::new("emitter_defect").expect("valid reason code"),
        )
    };

    let c = lift_record(already());
    assert_eq!(c.status_code(), 409);
    assert_eq!(c.resource_type(), Some(USAGE_RECORD_RESOURCE));
    assert_eq!(c.resource_name(), Some(target.to_string().as_str()));

    let problem = usage_record_error_to_problem(already());
    assert_eq!(
        problem_context_string(&problem, "reason").as_deref(),
        Some("ALREADY_INVALIDATED"),
    );
    assert_eq!(
        problem_context_string(&problem, "invalidated_by").as_deref(),
        Some(invalidated_by.to_string().as_str()),
    );
    assert_eq!(
        problem_context_string(&problem, "reason_code").as_deref(),
        Some("emitter_defect"),
    );
    assert_eq!(problem.context.get("retryable"), None, "not retryable");
}
```

  - **`service_metrics_tests.rs`:** change the `already_invalidated` case to
    `UsageCollectorError::already_invalidated(Uuid::from_u128(10), Uuid::from_u128(13), usage_collector_sdk::ReasonCode::new("emitter_defect").expect("valid reason code")),`
    and its comment to `// A second invalidation under another reason code joins the gateway's invalidation rules.`
  - **`service_tests.rs`, batch module:** replace
    `a_plugin_already_invalidated_rejection_is_lifted_as_a_conflict` with these
    two tests:

```rust
    /// A second withdrawal under another reason code collides on the derived
    /// `inv:<target>` key. The store reports an ordinary conflict carrying the
    /// stored invalidation, and the gateway reports it as `AlreadyInvalidated`
    /// because the dispatched entry is an invalidation.
    #[tokio::test]
    async fn a_store_conflict_on_a_withdrawal_is_lifted_as_already_invalidated() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x507);
        let target = Uuid::from_u128(0x60A);
        plugin.set_get_record_for(target, target_row(tenant_id, target));
        let stored = projected(&withdrawal_of(tenant_id, target, "idem-first"));
        let stored_id = stored.id;
        plugin.set_create_records(vec![Err(UsageCollectorPluginError::idempotency_conflict(
            format!("inv:{target}"),
            stored,
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.already_invalidated.records.v1",
        );

        let results = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![withdrawal_of(tenant_id, target, "idem-second")],
            )
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::Conflict {
                reason: ConflictReason::AlreadyInvalidated,
                name,
                invalidated_by,
                reason_code,
                ..
            }) => {
                assert_eq!(name, &target.to_string());
                assert_eq!(*invalidated_by, Some(stored_id));
                assert_eq!(
                    reason_code.as_ref().map(ReasonCode::as_str),
                    Some("emitter_defect")
                );
            }
            other => panic!("a conflict on a dispatched invalidation is AlreadyInvalidated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_store_conflict_on_a_record_stays_an_idempotency_conflict() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x508);
        let stored = projected(&ordinary_record(tenant_id, "idem-conflict"));
        let stored_id = stored.id;
        plugin.set_create_records(vec![Err(UsageCollectorPluginError::idempotency_conflict(
            "idem-conflict",
            stored,
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.record.idempotency_conflict.records.v1",
        );
        let mut divergent = ordinary_record(tenant_id, "idem-conflict");
        divergent.quantity = crate::domain::test_support::qty("11");

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![divergent])
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::Conflict {
                reason: ConflictReason::IdempotencyConflict,
                name,
                invalidated_by: None,
                reason_code: None,
                ..
            }) => assert_eq!(name, &stored_id.to_string()),
            other => panic!("a conflict on a record stays IdempotencyConflict, got {other:?}"),
        }
    }
```

  - **`service_tests.rs`, singular module:** replace
    `create_usage_record_plugin_already_invalidated_lifts_to_conflict` with:

```rust
    #[tokio::test]
    async fn create_usage_record_store_conflict_on_a_withdrawal_is_already_invalidated() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x70A);
        let target = Uuid::from_u128(0x805);
        plugin.set_get_record(target_row(tenant_id, target));
        let stored = projected(&counter_withdrawal(tenant_id, target, "idem-first"));
        let stored_id = stored.id;
        plugin.set_create_record_err(UsageCollectorPluginError::idempotency_conflict(
            format!("inv:{target}"),
            stored,
        ));
        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.already_invalidated.records.v1",
        );

        let err = service
            .create_usage_record(
                &authenticated_ctx(),
                counter_withdrawal(tenant_id, target, "idem-second-withdrawal"),
            )
            .await
            .expect_err("a second withdrawal MUST surface as Err");

        match err {
            UsageCollectorError::Conflict {
                reason: ConflictReason::AlreadyInvalidated,
                name,
                invalidated_by,
                reason_code,
                ..
            } => {
                assert_eq!(name, target.to_string());
                assert_eq!(invalidated_by, Some(stored_id));
                assert_eq!(
                    reason_code.as_ref().map(ReasonCode::as_str),
                    Some("emitter_defect")
                );
            }
            other => panic!("expected AlreadyInvalidated; got {other:?}"),
        }
    }
```

  - **Remaining references.** Run
    `grep -rn 'AlreadyInvalidated\|existing_id' usage-collector/src`. Every hit
    left is one of:
    - `ConflictReason::AlreadyInvalidated` (kept);
    - `DomainError::IdempotencyConflict { existing_id }` (kept);
    - doc text in `test_support.rs` or elsewhere that calls it "the store's
      rule". Reword each doc hit to "the dedup conflict of an invalidation".

- [ ] **Step 11: Check the SDK and gateway compile.**
  - Run: `cargo check --all-targets --all-features -p cf-gears-usage-collector-sdk -p cf-gears-usage-collector`
  - Expected: errors only inside `usage-collector-sdk/src/contract*`. Those
    are 3C.

#### 3C — Reference backend and contract harness

- [ ] **Step 12: Add `DedupLevel` and the `run_all` input.** In `usage-collector-sdk/src/contract.rs`:
  - add the enum below after `ContractViolation`'s `Display` impl;
  - change `run_all` to the version below;
  - in the module doc example, change `contract::run_all(&plugin).await` to
    `contract::run_all(&plugin, contract::DedupLevel::Linearizable).await`.

```rust
/// The dedup level a plugin declares in its deployment guide (DESIGN §3.1
/// "Dedup level", §3.10 item 9), handed to [`run_all`] so a check can hold the
/// plugin to the outcomes that level promises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupLevel {
    /// Every write is decided as it commits; the convergence bound is zero.
    Linearizable,
    /// A write can be acknowledged and later discarded; an identity converges
    /// within `convergence_bound`.
    Eventual {
        /// The declared convergence bound.
        convergence_bound: std::time::Duration,
    },
}
```

```rust
pub async fn run_all(
    plugin: &dyn UsageCollectorPluginV1,
    level: DedupLevel,
) -> Vec<ContractViolation> {
    let mut violations = quantity_round_trip(plugin).await;
    violations.extend(window_end_selection(plugin).await);
    violations.extend(dedup_identity_over_window(plugin).await);
    violations.extend(invalidation_excluded_from_fold(plugin).await);
    violations.extend(at_most_one_invalidation(plugin, level).await);
    violations.extend(scope_is_a_filter_on_every_read_path(plugin).await);
    violations
}
```

  Add a doc paragraph to `run_all`:
  `/// \`level\` is the dedup level the plugin declares. Only \`at-most-one-invalidation\` reads it today.`

- [ ] **Step 13: Update the reference backend's admission.** In `contract/reference.rs`:
  - replace `admit` and the doc above it with the code below;
  - in the module doc, replace "idempotent re-admission, at-most-one
    invalidation checked atomically," with "dedup by caller-supplied fields
    (which is also at most one invalidation per record),";
  - replace the doc of `create_usage_record` with
    `/// Admits one entry, or reports why it was refused, under one lock acquisition.`;
  - in the doc of `create_usage_records`, replace the second paragraph (from
    "The whole batch is decided …" to "… would let both through.") with:
    "The whole batch is decided under one lock acquisition, in input order, so
    two same-identity entries arriving in one call are ordered against each
    other: the first is admitted and the second is decided against it."

```rust
/// Decides one entry against the ledger it is being admitted to.
///
/// A collision on `id` — the `UUIDv5` of the five dedup-identity attributes, so
/// a collision already means those five agree — is decided by the rest of what
/// the caller supplied ([`UsageRecord::caller_supplied_eq`]): equal is an
/// idempotent replay answering with the stored entry, different is
/// [`UsageCollectorPluginError::IdempotencyConflict`] carrying it. A second
/// invalidation of one record is this same branch: every invalidation of one
/// record derives the same `inv:<target>` key.
fn admit(
    ledger: &mut Vec<UsageRecord>,
    record: UsageRecord,
) -> Result<UsageRecord, UsageCollectorPluginError> {
    if let Some(stored) = ledger.iter().find(|entry| entry.id == record.id) {
        if stored.caller_supplied_eq(&record) {
            return Ok(stored.clone());
        }
        return Err(UsageCollectorPluginError::idempotency_conflict(
            record.idempotency_key.as_str(),
            stored.clone(),
        ));
    }
    ledger.push(record.clone());
    Ok(record)
}
```

- [ ] **Step 14: Add the reason-carrying fixture.** Append to `contract/fixtures.rs`:

```rust
/// A faithful invalidation of `target` stating `reason`.
///
/// Every invalidation of one target derives the same `inv:<target>` key, so
/// the reason code is the one field two withdrawals of one target can differ
/// in, and `at-most-one-invalidation` needs two that do.
pub fn fixture_invalidation_with_reason(
    target: &UsageRecord,
    reason: &str,
) -> Result<UsageRecord, String> {
    let reason = ReasonCode::new(reason)
        .map_err(|err| format!("the check's own reason code is invalid: {err}"))?;
    CreateUsageRecord {
        gts_type_id: target.gts_type_id.clone(),
        tenant_id: target.tenant_id,
        resource_ref: target.resource_ref.clone(),
        subject_ref: target.subject_ref.clone(),
        metadata: target.metadata.clone(),
        quantity: target.quantity,
        idempotency_key: None,
        invalidation: Some(Invalidation {
            target: target.id,
            reason,
        }),
        window_start: target.window_start,
        window_end: target.window_end,
    }
    .try_into_usage_record(RecordOrigin::Live, CONTRACT_ACCEPTED_AT)
    .map_err(|err| format!("the check's own submission is not projectable: {err}"))
}
```

- [ ] **Step 15: Rewrite the check.** Replace the whole of
  `contract/checks/at_most_one_invalidation.rs` with:

```rust
//! The DESIGN §3.3 `at-most-one-invalidation` check.
//!
//! See [`at_most_one_invalidation`] for what it asserts.

use bigdecimal::BigDecimal;

use crate::contract::fixtures::{
    FIXTURE_EPOCH, contract_query, fixture_invalidation_with_reason, fixture_record, violation,
};
use crate::contract::{AT_MOST_ONE_INVALIDATION, ContractViolation, DedupLevel, HARNESS_FAULT};
use crate::error::UsageCollectorPluginError;
use crate::models::{AggregationFold, IdempotencyKey, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::time_range::TimeRange;

/// The start of this check's covered periods: a hundred and twenty days past
/// [`FIXTURE_EPOCH`], clear of the ranges the checks that read back dispatch.
const AT_MOST_ONE_WINDOW_FROM: time::OffsetDateTime =
    FIXTURE_EPOCH.saturating_add(time::Duration::days(120));

/// The quantity every entry here carries; an invalidation echoes it.
const AT_MOST_ONE_QUANTITY: &str = "1";

/// The reason code the accepted withdrawal of each target states.
const FIRST_REASON: &str = "at-most-one-invalidation-first";

/// The reason code the divergent withdrawal of each target states.
const SECOND_REASON: &str = "at-most-one-invalidation-second";

/// The read limit of the `Eventual` ledger read: twice the entries one pair's
/// range holds.
const AT_MOST_ONE_PAGE_LIMIT: u64 = 8;

/// A record and two withdrawals of it that differ in their reason code alone.
struct Pair {
    target: UsageRecord,
    first: UsageRecord,
    second: UsageRecord,
}

/// The pair decided across separate calls, and the pair decided in one batch.
struct AtMostOneFixtures {
    separate: Pair,
    batched: Pair,
}

/// `at-most-one-invalidation` — *"At the SPI, a second invalidation of one
/// record under the same reason code returns the stored invalidation, and under
/// a different one is `IdempotencyConflict`. Concurrent submissions follow the
/// declared dedup level."*
///
/// There is no store-side at-most-one rule to test. Every invalidation of one
/// record derives the same `inv:<target>` key, so a second one is an ordinary
/// collision on the dedup identity, and this check holds a plugin to the dedup
/// outcomes on it:
///
/// * **Separate calls.** Resubmitting the accepted withdrawal answers with the
///   stored invalidation; submitting one under another reason code is
///   `IdempotencyConflict` whose `existing` is the accepted withdrawal.
/// * **One batch call.** Of two withdrawals of one record in a single
///   `create_usage_records`, the first is accepted and the second resolves
///   against it as `IdempotencyConflict`.
///
/// Under [`DedupLevel::Eventual`] a divergent withdrawal may be acknowledged
/// and later discarded, so an acceptance is not a violation there. After the
/// declared convergence bound the ledger must hold exactly one invalidation of
/// each record, and a `COUNT` over the pair's range must count nothing.
pub async fn at_most_one_invalidation(
    plugin: &dyn UsageCollectorPluginV1,
    level: DedupLevel,
) -> Vec<ContractViolation> {
    let fixtures = match at_most_one_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own `{AT_MOST_ONE_INVALIDATION}` \
                     fixtures, so nothing was submitted. This is a fault in the suite, not in the \
                     plugin under test: {detail}"
                ),
            )];
        }
    };
    let mut violations = separate_calls(plugin, &fixtures.separate, level).await;
    violations.extend(one_batch_call(plugin, &fixtures.batched, level).await);
    if let DedupLevel::Eventual { convergence_bound } = level {
        toolkit::tokio::time::sleep(convergence_bound).await;
        for pair in [&fixtures.separate, &fixtures.batched] {
            violations.extend(one_invalidation_survives(plugin, pair).await);
        }
    }
    violations
}

/// Whether `existing` is the accepted withdrawal of `pair`, reason code included.
fn names_the_first(existing: &UsageRecord, pair: &Pair) -> bool {
    existing.id == pair.first.id
        && existing
            .invalidation
            .as_ref()
            .map(|invalidation| invalidation.reason.as_str())
            == Some(FIRST_REASON)
}

/// Whether `stored` is `expected` as the plugin would answer with it.
fn is_the_stored(stored: &UsageRecord, expected: &UsageRecord) -> bool {
    stored.id == expected.id && stored.caller_supplied_eq(expected)
}

async fn separate_calls(
    plugin: &dyn UsageCollectorPluginV1,
    pair: &Pair,
    level: DedupLevel,
) -> Vec<ContractViolation> {
    for (role, record) in [
        ("the entry to be withdrawn", &pair.target),
        ("the first withdrawal of it", &pair.first),
    ] {
        if let Err(err) = plugin.create_usage_record(record.clone()).await {
            return vec![violation(
                AT_MOST_ONE_INVALIDATION,
                format!(
                    "`create_usage_record` refused {role} (record {id}), so there was no accepted \
                     withdrawal to decide a second one against: {err}",
                    id = record.id,
                ),
            )];
        }
    }
    let target = pair.target.id;
    let first = pair.first.id;
    let mut violations = Vec::new();

    match plugin.create_usage_record(pair.first.clone()).await {
        Ok(stored) if is_the_stored(&stored, &pair.first) => {}
        Ok(stored) => violations.push(violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "resubmitting withdrawal {first} of record {target} under its own reason code \
                 answered {stored:?}; an exact retry must return the stored invalidation"
            ),
        )),
        Err(err) => violations.push(violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "resubmitting withdrawal {first} of record {target} under its own reason code was \
                 refused as `{err}`; it is an exact retry and must be absorbed, returning the \
                 stored invalidation"
            ),
        )),
    }

    match (plugin.create_usage_record(pair.second.clone()).await, level) {
        (Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }), _)
            if names_the_first(&existing, pair) => {}
        (Ok(_), DedupLevel::Eventual { .. }) => {}
        (outcome, _) => violations.push(violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "a second withdrawal of record {target} under another reason code answered \
                 {outcome:?}; it collides on the derived `inv:{target}` key and must be \
                 `IdempotencyConflict` whose `existing` is the accepted withdrawal {first}"
            ),
        )),
    }
    violations
}

async fn one_batch_call(
    plugin: &dyn UsageCollectorPluginV1,
    pair: &Pair,
    level: DedupLevel,
) -> Vec<ContractViolation> {
    let target = pair.target.id;
    if let Err(err) = plugin.create_usage_record(pair.target.clone()).await {
        return vec![violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "`create_usage_record` refused the entry the batched withdrawals aim at (record \
                 {target}): {err}"
            ),
        )];
    }
    let outcomes = match plugin
        .create_usage_records(vec![pair.first.clone(), pair.second.clone()])
        .await
    {
        Ok(outcomes) => outcomes,
        Err(err) => {
            return vec![violation(
                AT_MOST_ONE_INVALIDATION,
                format!(
                    "`create_usage_records` failed the whole batch carrying two withdrawals of \
                     record {target}: {err}. Outcomes are per entry."
                ),
            )];
        }
    };
    if outcomes.len() != 2 {
        return vec![violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "`create_usage_records` answered {} outcomes for a batch of two entries",
                outcomes.len()
            ),
        )];
    }
    let mut outcomes = outcomes.into_iter();
    let (earlier, later) = (outcomes.next(), outcomes.next());
    let mut violations = Vec::new();
    match earlier {
        Some(Ok(stored)) if is_the_stored(&stored, &pair.first) => {}
        other => violations.push(violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "the first of two withdrawals of record {target} in one batch answered {other:?}; \
                 it is the first entry of its identity in the call and must be accepted"
            ),
        )),
    }
    match (later, level) {
        (Some(Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. })), _)
            if names_the_first(&existing, pair) => {}
        (Some(Ok(_)), DedupLevel::Eventual { .. }) => {}
        (other, _) => violations.push(violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "the second of two withdrawals of record {target} in one batch, under another \
                 reason code, answered {other:?}; a later same-identity entry resolves against \
                 the earlier one and must be `IdempotencyConflict` naming {first}",
                first = pair.first.id,
            ),
        )),
    }
    violations
}

/// The `Eventual` half: after the convergence bound, one invalidation of the
/// record survives on the ledger and the fold counts the pair as nothing.
async fn one_invalidation_survives(
    plugin: &dyn UsageCollectorPluginV1,
    pair: &Pair,
) -> Vec<ContractViolation> {
    let target = pair.target.id;
    let range = match TimeRange::new(
        pair.target.window_end,
        pair.target.window_end.saturating_add(time::Duration::minutes(1)),
    ) {
        Ok(range) => range,
        Err(err) => return vec![violation(HARNESS_FAULT, format!("the check's own range is invalid: {err:?}"))],
    };
    let query = contract_query(AT_MOST_ONE_PAGE_LIMIT);
    let mut violations = Vec::new();

    match plugin
        .list_usage_records(pair.target.gts_type_id.clone(), range, &query, &[])
        .await
    {
        Ok(page) => {
            let withdrawals = page
                .items
                .iter()
                .filter(|entry| {
                    entry
                        .invalidation
                        .as_ref()
                        .is_some_and(|invalidation| invalidation.target == target)
                })
                .count();
            if withdrawals != 1 {
                violations.push(violation(
                    AT_MOST_ONE_INVALIDATION,
                    format!(
                        "after the declared convergence bound the ledger holds {withdrawals} \
                         invalidations of record {target}; one identity reads at most once, so \
                         exactly one survives"
                    ),
                ));
            }
        }
        Err(err) => violations.push(violation(
            AT_MOST_ONE_INVALIDATION,
            format!("the ledger read over record {target}'s period failed: {err}"),
        )),
    }

    match plugin
        .query_aggregated_usage_records(
            pair.target.gts_type_id.clone(),
            range,
            AggregationFold::Count,
            &query,
            &[],
            &[],
        )
        .await
    {
        Ok(result) => {
            let zero = BigDecimal::from(0);
            if result
                .buckets
                .iter()
                .filter_map(|bucket| bucket.value.as_ref())
                .any(|value| *value != zero)
            {
                violations.push(violation(
                    AT_MOST_ONE_INVALIDATION,
                    format!(
                        "a COUNT over record {target}'s withdrawn pair counted something \
                         ({:?}); a withdrawn pair counts none",
                        result.buckets
                    ),
                ));
            }
        }
        Err(err) => violations.push(violation(
            AT_MOST_ONE_INVALIDATION,
            format!("the COUNT over record {target}'s period failed: {err}"),
        )),
    }
    violations
}

fn at_most_one_fixtures() -> Result<AtMostOneFixtures, String> {
    let quantity = UsageQuantity::parse(AT_MOST_ONE_QUANTITY)
        .map_err(|err| format!("the check's own quantity literal is invalid: {err}"))?;
    let separate_end = AT_MOST_ONE_WINDOW_FROM.saturating_add(time::Duration::hours(1));
    let batched_end = AT_MOST_ONE_WINDOW_FROM.saturating_add(time::Duration::hours(2));
    Ok(AtMostOneFixtures {
        separate: pair("separate-target", quantity, AT_MOST_ONE_WINDOW_FROM, separate_end)?,
        batched: pair("batched-target", quantity, separate_end, batched_end)?,
    })
}

fn pair(
    role: &str,
    quantity: UsageQuantity,
    window_start: time::OffsetDateTime,
    window_end: time::OffsetDateTime,
) -> Result<Pair, String> {
    let key = IdempotencyKey::new(format!("{AT_MOST_ONE_INVALIDATION}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))?;
    let target = fixture_record(&key, quantity, window_start, window_end)?;
    let first = fixture_invalidation_with_reason(&target, FIRST_REASON)?;
    let second = fixture_invalidation_with_reason(&target, SECOND_REASON)?;
    if first.id != second.id {
        return Err(format!(
            "the two withdrawals of record {} derive different ids ({} vs {}); every withdrawal \
             of one target derives `inv:<target>`, so this pair would not collide",
            target.id, first.id, second.id
        ));
    }
    Ok(Pair {
        target,
        first,
        second,
    })
}
```

  If `bucket.value` is not `Option<BigDecimal>`, `cargo check` will say so.
  Read `AggregationBucket` in `models.rs` and adapt the one `filter_map` line.
  The assertion stays "no bucket counts anything".

  The `Eventual` branch sleeps through `toolkit::tokio::time::sleep`, the
  toolkit's tokio re-export (the TimescaleDB plugin uses the same path). The SDK
  already depends on `toolkit`. If `cargo check -p cf-gears-usage-collector-sdk --features contract`
  reports `time` as missing, stop and report. Do not add a direct `tokio`
  dependency to the SDK without asking.

- [ ] **Step 16: Replace the at-most-one mutant.** In `contract_mutants.rs`:
  - **`Defect` enum.** Replace `ChecksThenInsertsTheInvalidation` with:

```rust
    /// Absorbs a second withdrawal of a record even under another reason code —
    /// the mistake a backend makes by comparing only the dedup identity.
    AbsorbsAWithdrawalWithAnotherReason,
    /// Refuses a second withdrawal of a record even under the same reason code —
    /// the mistake a backend makes by keeping an at-most-one rule of its own.
    RefusesAWithdrawalWithTheSameReason,
```

  - **`mutant()`.** Route both new defects to `WrappedReference`, and only
    `SelectsOnWindowStart | FoldsTheInvalidation` to `MutantLedger`.
  - **`on_admission`.** Its exhaustive no-op arm lists
    `IgnoresScopeOnThePointRead | AbsorbsAWithdrawalWithAnotherReason | RefusesAWithdrawalWithTheSameReason | SelectsOnWindowStart | FoldsTheInvalidation`.
  - **`WrappedReference::period_blind_keys`** becomes
    `Mutex<BTreeMap<PeriodBlindKey, UsageRecord>>`. In `claim_period_blind_key`:
    - the conflict branch is `if let Some(existing) = claimed.get(&key) && existing.id != record.id { return Err(UsageCollectorPluginError::idempotency_conflict(record.idempotency_key.as_str(), existing.clone())); }`;
    - the insert is `claimed.insert(key, record.clone());`.
  - **New helpers.** Add these to `impl WrappedReference`:

```rust
    /// [`Defect::RefusesAWithdrawalWithTheSameReason`]: a withdrawal whose
    /// derived id is already stored is refused, whatever it states.
    async fn refused_as_a_second_withdrawal(
        &self,
        record: &UsageRecord,
    ) -> Option<UsageCollectorPluginError> {
        if self.defect != Defect::RefusesAWithdrawalWithTheSameReason
            || record.invalidation.is_none()
        {
            return None;
        }
        let stored = self
            .inner
            .get_usage_record(record.id, &tenant_scope(record.tenant_id), true)
            .await
            .ok()?;
        Some(UsageCollectorPluginError::idempotency_conflict(
            record.idempotency_key.as_str(),
            stored,
        ))
    }

    /// [`Defect::AbsorbsAWithdrawalWithAnotherReason`]: a conflict on a
    /// withdrawal is answered as an absorb of the stored entry.
    fn after_admission(
        &self,
        record: &UsageRecord,
        outcome: Result<UsageRecord, UsageCollectorPluginError>,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        match outcome {
            Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. })
                if self.defect == Defect::AbsorbsAWithdrawalWithAnotherReason
                    && record.invalidation.is_some() =>
            {
                Ok(*existing)
            }
            other => other,
        }
    }
```

  - **`tenant_scope`.** Add this free fn next to `only_this_row`:

```rust
/// `tenant_id eq <tenant>`: the scope a mutant reads its own ledger under.
fn tenant_scope(tenant: Uuid) -> ast::Expr {
    ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("tenant_id".to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::Uuid(tenant))),
    )
}
```

  - **`WrappedReference::create_usage_record`** becomes:

```rust
        let record = self.on_admission(record)?;
        if let Some(refusal) = self.refused_as_a_second_withdrawal(&record).await {
            return Err(refusal);
        }
        let outcome = self.inner.create_usage_record(record.clone()).await;
        self.after_admission(&record, outcome)
```

  - **`WrappedReference::create_usage_records`.** Keep the empty-batch guard.
    Replace the `prepared` construction with the code below. The final mapping
    `Ok(record) => { let outcome = inner.next().unwrap_or_else(…); self.after_admission(&record, outcome) }`
    keeps its existing `unwrap_or_else` body.

```rust
        let mut prepared: Vec<Result<UsageRecord, UsageCollectorPluginError>> =
            Vec::with_capacity(records.len());
        for record in records {
            let entry = match self.on_admission(record) {
                Ok(record) => match self.refused_as_a_second_withdrawal(&record).await {
                    Some(refusal) => Err(refusal),
                    None => Ok(record),
                },
                Err(err) => Err(err),
            };
            prepared.push(entry);
        }
```

  - **`MutantLedger`.**
    - `create_usage_records` loses the `ChecksThenInsertsTheInvalidation`
      snapshot branch and always admits in order.
    - `decide` becomes the body below.
    - Delete the admission-order paragraph in the `MutantLedger` doc (from
      "**What that does not cover …**" to "… rather than 'it mirrors the
      reference'."). Replace the one-line doc sentence about "whether a batch is
      decided under one lock" with "which column a range meets and which rows a
      fold walks".

```rust
fn decide(
    ledger: &[UsageRecord],
    record: &UsageRecord,
) -> Result<Option<UsageRecord>, UsageCollectorPluginError> {
    match ledger.iter().find(|entry| entry.id == record.id) {
        Some(stored) if stored.caller_supplied_eq(record) => Ok(Some(stored.clone())),
        Some(stored) => Err(UsageCollectorPluginError::idempotency_conflict(
            record.idempotency_key.as_str(),
            stored.clone(),
        )),
        None => Ok(None),
    }
}
```

  - **Module doc.** It says "three" wrapped and "three" own-ledger subjects.
    Make it "five wrapped (quantity, period-blind dedup, point-read scope, and
    the two withdrawal defects)" and "two own-ledger (selection column, fold
    exclusion)".

- [ ] **Step 17: Update the contract tests.** In `contract_tests.rs`:
  - import `DedupLevel`;
  - every `run_all(x)` call becomes `run_all(x, DedupLevel::Linearizable)`;
  - in `DISCRIMINATION_MATRIX`, replace the `ChecksThenInsertsTheInvalidation`
    row with:

```rust
    (
        Defect::AbsorbsAWithdrawalWithAnotherReason,
        &[AT_MOST_ONE_INVALIDATION],
    ),
    (
        Defect::RefusesAWithdrawalWithTheSameReason,
        &[AT_MOST_ONE_INVALIDATION],
    ),
```

  - in the module doc, "six deliberately non-conforming subjects" becomes
    "seven";
  - in the doc of `each_check_fails_against_its_own_defect_and_no_other`, "all
    six mutants" becomes "all seven" and "the other five" becomes "the others".

- [ ] **Step 18: Check the SDK suite.**
  - Run: `cargo nextest run -p cf-gears-usage-collector-sdk --features contract`
  - Expected: PASS, `the_reference_backend_conforms` and the discrimination
    matrix included.

#### 3D — TimescaleDB plugin

Paths in this part are relative to `plugins/timescaledb-usage-collector-plugin/`.

- [ ] **Step 19: Swap the index in `migrations/0001_init.sql`.** Edit it in
  place; the gear is unreleased. Replace the block from
  `-- At most one accepted invalidation per entry, enforced by the database rather`
  through `    WHERE invalidates IS NOT NULL;` with:

```sql
-- Lookup index for the fold's second withdrawal-exclusion obligation: "is this
-- entry named by an accepted invalidation?" `invalidates` leads it under
-- exactly that partial predicate. It is not a rule. At most one invalidation
-- per entry follows from the derived `inv:<target>` idempotency key, which
-- makes a second invalidation an ordinary collision on
-- `usage_records_dedup_uniq` (DESIGN §3.1 "At most one invalidation").
-- `window_end` and `type_key` are carried because an invalidation copies both
-- from its target, so a lookup by target can prune chunks on them.
CREATE INDEX IF NOT EXISTS usage_records_invalidates_idx
    ON usage_records (invalidates, window_end, type_key)
    WHERE invalidates IS NOT NULL;
```

- [ ] **Step 20: Fix the rollup header.** In `migrations/0002_usage_rollup.sql`, replace

```sql
-- invalidation (usage_records_one_invalidation_uniq plus the gateway;
-- DIVERGENCES.md entry 21); (3) the gateway admits an invalidation only for an
```

  with

```sql
-- invalidation (every invalidation of a record derives one inv:<target> key,
-- so a second collides on usage_records_dedup_uniq); (3) the gateway admits an
-- invalidation only for an
```

  Then join the line that follows (`-- existing record; …`) so the sentence
  still reads "(3) the gateway admits an invalidation only for an existing
  record;". Only comments change.

- [ ] **Step 21: Drop the index's error class.** In `src/infra/storage/error.rs`:
  - delete `ONE_INVALIDATION_UNIQUE` and its doc;
  - delete the `AlreadyInvalidated` variant of `DbErrorClass`;
  - delete the `Some(c) if is_constraint(c, ONE_INVALIDATION_UNIQUE) => …` arm;
  - in the comment inside `classify_db`:
    - replace "Match each unique constraint by name. Any other unique
      constraint — the records PK `(id, window_end)`, say — must fall through to
      `Other` rather than be silently misread as one of these two." with
      "Match the dedup constraint by name. Any other unique constraint — the
      records PK `(id, window_end)`, say — must fall through to `Other`.";
    - delete the paragraph starting "[`ONE_INVALIDATION_UNIQUE`] is the
      opposite";
    - change "Both are matched through [`is_constraint`]" to "It is matched
      through [`is_constraint`]".

  In `error_tests.rs`, delete `a_chunk_local_one_invalidation_index_is_already_invalidated`.

- [ ] **Step 22: Clean up the store.** In `src/infra/storage/record_store.rs`:
  - **Delete** `map_insert_error`, `invalidation_index_slots`,
    `find_existing_invalidation`, `duplicate_withdrawal_in_batch` and
    `canonical_equal`, each with its doc.
  - **Insert failures.** At both failed-insert sites (`create_inner` and
    `create_batch_inner`), replace
    `rollback(tx).await; let slots = …; return Err(self.map_insert_error(&mut conn, &e, &slots).await);`
    with:

```rust
                rollback(tx).await;
                return Err(self.record_backend_error(&e));
```

    In `create_inner`, also delete the comment above it that mentions "the
    diagnostic read below".
  - **`DEDUP_CONFLICT_TARGET` doc** becomes:
    `/// The dedup 5-tuple plus the partition key the hypertable requires in every UNIQUE, as an \`ON CONFLICT\` arbiter. Both insert paths spend their one arbiter here.`
  - **`create_inner` doc.** Replace the two paragraphs "**One backend
    transaction, and it has to be one.** …" and "**The at-most-one guarantee is
    conditional, and on the gateway.** …" with:

```rust
    /// **One backend transaction.** The entry's `acceptance_sequence` is
    /// claimed from `usage_acceptance_sequence` (the gear's DESIGN §3.7) and
    /// inserted in the same transaction, so the two commit or roll back
    /// together.
```

    In the counters sentence of the same doc, change "(dedup absorbed /
    idempotency conflict / invalidation accepted / invalidation refused /
    backend error)" to "(dedup absorbed / idempotency conflict / invalidation
    accepted / backend error)".
  - **`resolve_dedup_hit`.** Replace it and its doc with:

```rust
    /// Resolve a dedup-key hit into absorb or conflict.
    ///
    /// The stored row is mapped into a [`UsageRecord`] and compared with
    /// [`UsageRecord::caller_supplied_eq`], the SDK's one definition of the
    /// comparison, so `origin` and `accepted_at` are ignored and the quantity
    /// compares digit for digit. `id` is compared as well, as defence in depth:
    /// it is derived from the identity both sides share, so a mismatch is the
    /// shape a corrupted stored row takes. Equal is absorbed and answers with
    /// the stored entry; different is
    /// [`UsageCollectorPluginError::IdempotencyConflict`] carrying it. A stored
    /// row that cannot be mapped (corrupt metadata, say) is `Internal`.
    fn resolve_dedup_hit(
        &self,
        row: UsageRecordRow,
        record: &UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        let stored = record_row_to_model(row)?;
        if stored.id == record.id && stored.caller_supplied_eq(record) {
            self.metrics.inc_dedup_absorbed();
            Ok(stored)
        } else {
            self.metrics.inc_idempotency_conflict();
            Err(UsageCollectorPluginError::idempotency_conflict(
                record.idempotency_key.as_str(),
                stored,
            ))
        }
    }
```

  - **Batch planning.** Replace `BatchPlan` and `plan_batch`, with their docs,
    with:

```rust
/// Deterministic plan for a batch insert.
///
/// `reps` are the first-occurrence representative records, one per distinct
/// dedup key, **sorted** by [`DedupKey`] so concurrent batches take the
/// 5-tuple-UNIQUE conflict locks — and the per-scope acceptance-sequence row
/// locks — in one global order (deadlock-free). `first_index` maps each key to
/// the input index of its first occurrence, the only row that can win the slot.
/// Later same-key rows resolve against the winner's stored row, exactly as the
/// single-row path resolves a same-key hit.
struct BatchPlan<'a> {
    reps: Vec<&'a UsageRecord>,
    first_index: HashMap<DedupKey, usize>,
}

/// Collapse a batch to its distinct dedup keys (first occurrence wins), sorted
/// for a stable lock order. Pure — no DB. `reps` borrow from `records`.
///
/// Two invalidations of one target in one batch share the derived
/// `inv:<target>` key and so one slot: the later resolves against the earlier
/// like any other same-key pair.
fn plan_batch(records: &[UsageRecord]) -> BatchPlan<'_> {
    let mut first_index: HashMap<DedupKey, usize> = HashMap::new();
    let mut reps: Vec<(DedupKey, &UsageRecord)> = Vec::new();
    for (i, record) in records.iter().enumerate() {
        let key = dedup_key(record);
        if let std::collections::hash_map::Entry::Vacant(slot) = first_index.entry(key.clone()) {
            slot.insert(i);
            reps.push((key, record));
        }
    }
    reps.sort_by(|a, b| a.0.cmp(&b.0));
    BatchPlan {
        reps: reps.into_iter().map(|(_, r)| r).collect(),
        first_index,
    }
}
```

  - **`resolve_batch`.** Delete the leading
    `if let Some(&invalidated_by) = plan.duplicate_withdrawals.get(&i) { … continue; }`
    block and its comment.
  - **`create_batch_inner` doc.** Delete the sentence "and the
    at-most-one-invalidation index has to refuse a second withdrawal atomically
    with the entry it admits, which the SPI is explicit about." and the whole
    paragraph "**Two invalidations of one target that arrive together are
    handled before the insert, not by it** …".
  - **`is_retryable_batch_error` doc.** Change "`Internal`,
    `IdempotencyConflict`, `AlreadyInvalidated` and the other typed domain
    outcomes are non-retryable and returned unchanged — an already-withdrawn
    target does not become withdrawable by waiting." to "`Internal`,
    `IdempotencyConflict` and the other typed domain outcomes are non-retryable
    and returned unchanged."
  - **`create_batch`'s `on_retry` closure comment.** Keep the first paragraph up
    to "`uc_timescaledb_invalidation_rejected_statements_total`, not this
    one)." but drop that parenthetical's `map_insert_error` clause, so it ends
    "… (most move the backend-error counter instead).". Delete the following
    paragraph "That arm returns `Transient` …".
  - **Imports.** Remove any `use` items that are now unused (for example
    `metadata_jsonb_to_map`, `OffsetDateTime` if unused). Clippy names them.

- [ ] **Step 23: Swap the metrics.** In `src/infra/metrics.rs`:
  - delete the fields `invalidation_rejected_rows` and
    `invalidation_rejected_statements`, their `build()` calls, their `Self { … }`
    entries, and the methods `inc_invalidation_rejected_row` and
    `inc_invalidation_rejected_statement`;
  - add the field
    `/// \`uc_timescaledb_dedup_late_convergence_total\` — writes discarded after their dedup identity converged. Always zero: this plugin is \`linearizable\`, so no write is decided after convergence.\n    dedup_late_convergence: Counter<u64>,`
    after `dedup_stale`;
  - in `with_meter`, after `dedup_stale` is built, add:

```rust
        let dedup_late_convergence = meter
            .u64_counter("uc_timescaledb_dedup_late_convergence_total")
            .with_description(
                "Writes discarded after their dedup identity converged; always 0 under this \
                 plugin's linearizable dedup level",
            )
            .build();
        // Recorded once at zero so the series is exported: an OpenTelemetry
        // counter nothing has recorded on is not exported at all, and the
        // deployment guide names this series (DESIGN §3.10 item 9).
        dedup_late_convergence.add(0, &[]);
```

  - add `dedup_late_convergence,` to the `Self { … }` literal;
  - in `declared_instrument_names`:
    - replace `invalidation_rejected_rows: _,` and
      `invalidation_rejected_statements: _,` with `dedup_late_convergence: _,`;
    - replace the two names `"uc_timescaledb_invalidation_rejected_rows_total"`
      and `"uc_timescaledb_invalidation_rejected_statements_total"` with
      `"uc_timescaledb_dedup_late_convergence_total"`.

  In `src/infra/metrics_tests.rs`:
  - delete `the_two_rejection_paths_are_separate_instruments_because_their_units_differ`
    and `each_rejection_counters_description_carries_its_own_unit`;
  - in `every_exported_instrument_obeys_the_naming_convention`, delete the
    `metrics.inc_invalidation_rejected_row();` and
    `metrics.inc_invalidation_rejected_statement();` calls;
  - append:

```rust
/// The late-convergence counter is exported at zero from construction, so the
/// series the deployment guide names exists before anything could increment it.
#[tokio::test]
async fn the_late_convergence_counter_is_published_at_zero() {
    let (provider, exporter) = local_provider();
    let _metrics = Metrics::with_meter(&provider.meter("uc.timescaledb"), lazy_pool());
    provider.force_flush().unwrap();

    assert!(
        exported_names(&exporter)
            .contains(&"uc_timescaledb_dedup_late_convergence_total".to_owned()),
        "the counter must be exported without a recording helper being called"
    );
    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_dedup_late_convergence_total"),
        0
    );
}
```

- [ ] **Step 24: Rewrite the store unit tests.** In `src/infra/storage/record_store_tests.rs`:
  - remove `canonical_equal` and `invalidation_index_slots` from the
    `use super::{…}` list;
  - replace the five `canonical_equal_*` tests (keep `row_matching`) with:

```rust
#[tokio::test]
async fn resolve_dedup_hit_surfaces_a_corrupt_stored_row_as_internal() {
    let store = lazy_store();
    let record = unit_record(uuid::Uuid::from_u128(7), "k", 700);
    let row = row_matching(&record, serde_json::Value::String("corrupt".to_owned()));
    assert!(
        matches!(
            store.resolve_dedup_hit(row, &record),
            Err(UsageCollectorPluginError::Internal(_))
        ),
        "stored-data corruption must not read as an absorb or a conflict"
    );
}

#[tokio::test]
async fn resolve_dedup_hit_absorbs_an_exact_match_and_returns_the_stored_entry() {
    let store = lazy_store();
    let record = unit_record(uuid::Uuid::from_u128(8), "k", 800);
    let mut row = row_matching(&record, serde_json::Value::Object(serde_json::Map::new()));
    row.accepted_at = record.accepted_at + time::Duration::hours(1);
    let stored_at = row.accepted_at;

    let absorbed = store
        .resolve_dedup_hit(row, &record)
        .expect("equal caller-supplied fields absorb");
    assert_eq!(absorbed.accepted_at, stored_at, "the stored entry is returned");
}

#[tokio::test]
async fn resolve_dedup_hit_conflicts_on_a_caller_supplied_field_and_carries_the_stored_entry() {
    let store = lazy_store();
    let record = unit_record(uuid::Uuid::from_u128(9), "k", 900);
    let mut row = row_matching(&record, serde_json::Value::Object(serde_json::Map::new()));
    row.quantity = rust_decimal::Decimal::new(999, 0);

    match store.resolve_dedup_hit(row, &record) {
        Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }) => {
            assert_eq!(existing.id, record.id);
            assert_eq!(existing.quantity.to_string(), "999");
        }
        other => panic!("a differing quantity must conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn resolve_dedup_hit_ignores_origin() {
    let store = lazy_store();
    let record = unit_record(uuid::Uuid::from_u128(11), "k", 1100);
    let mut row = row_matching(&record, serde_json::Value::Object(serde_json::Map::new()));
    row.origin = usage_collector_sdk::RecordOrigin::Backfill
        .as_str()
        .to_owned();
    assert!(
        store.resolve_dedup_hit(row, &record).is_ok(),
        "origin is server-assigned and not compared (SPEC-DIFF 2.5)"
    );
}

#[tokio::test]
async fn resolve_dedup_hit_compares_the_quantity_digit_for_digit() {
    let store = lazy_store();
    let record = unit_record(uuid::Uuid::from_u128(13), "k", 1300);
    let mut row = row_matching(&record, serde_json::Value::Object(serde_json::Map::new()));
    row.quantity = rust_decimal::Decimal::new(1000, 3); // 1.000 against the record's 1
    assert!(
        matches!(
            store.resolve_dedup_hit(row, &record),
            Err(UsageCollectorPluginError::IdempotencyConflict { .. })
        ),
        "1.000 is not 1 digit for digit (decision S-B7)"
    );
}

#[tokio::test]
async fn resolve_dedup_hit_treats_id_as_canonical() {
    let store = lazy_store();
    let record = unit_record(uuid::Uuid::from_u128(10), "k", 1000);
    let mut row = row_matching(&record, serde_json::Value::Object(serde_json::Map::new()));
    row.id = uuid::Uuid::from_u128(0xDEAD_BEEF);
    assert!(matches!(
        store.resolve_dedup_hit(row, &record),
        Err(UsageCollectorPluginError::IdempotencyConflict { .. })
    ));
}

#[tokio::test]
async fn resolve_dedup_hit_compares_the_reason_code() {
    let store = lazy_store();
    let record = withdrawal(uuid::Uuid::from_u128(12), "k", 1200, uuid::Uuid::from_u128(0x1200));
    let mut row = row_matching(&record, serde_json::Value::Object(serde_json::Map::new()));
    row.reason_code = Some("late_correction".to_owned());
    assert!(matches!(
        store.resolve_dedup_hit(row, &record),
        Err(UsageCollectorPluginError::IdempotencyConflict { .. })
    ));
}
```

  - in `plan_batch_collapses_and_sorts_distinct_keys`, delete the
    `plan.duplicate_withdrawals.is_empty()` assertion;
  - replace `plan_batch_pre_rejects_a_second_withdrawal_of_one_target`,
    `plan_batch_leaves_an_identical_repeat_withdrawal_to_the_dedup_path` and
    `invalidation_index_slots_names_only_the_withdrawals` with:

```rust
/// A withdrawal of `target` under its derived `inv:<target>` key, as the
/// gateway dispatches it.
fn derived_withdrawal(
    tenant: uuid::Uuid,
    seq: u128,
    target: uuid::Uuid,
    reason: &str,
) -> usage_collector_sdk::UsageRecord {
    usage_collector_sdk::UsageRecord {
        idempotency_key: usage_collector_sdk::IdempotencyKey::for_invalidation(target),
        invalidation: Some(usage_collector_sdk::Invalidation {
            target,
            reason: usage_collector_sdk::ReasonCode::new(reason).expect("valid reason code"),
        }),
        ..unit_record(tenant, "placeholder", seq)
    }
}

#[test]
fn plan_batch_collapses_two_withdrawals_of_one_target_onto_one_slot() {
    let tenant = uuid::Uuid::from_u128(0xC1);
    let target = uuid::Uuid::from_u128(0xC100);
    let records = vec![
        derived_withdrawal(tenant, 0xC101, target, "duplicate_submission"),
        unit_record(tenant, "plain", 0xC102),
        derived_withdrawal(tenant, 0xC101, target, "late_correction"),
    ];

    let plan = plan_batch(&records);

    assert_eq!(dedup_key(&records[0]), dedup_key(&records[2]));
    assert_eq!(plan.reps.len(), 2, "one slot for the pair, one for the plain entry");
    assert_eq!(plan.first_index[&dedup_key(&records[0])], 0, "the earlier withdrawal holds it");
}
```

  - replace `resolve_batch_reports_a_pre_rejected_withdrawal_as_already_invalidated`
    with:

```rust
#[tokio::test]
async fn resolve_batch_resolves_a_second_withdrawal_against_the_first() {
    let store = lazy_store();
    let tenant = uuid::Uuid::from_u128(0xB5);
    let target = uuid::Uuid::from_u128(0xB500);
    let first = derived_withdrawal(tenant, 0xB501, target, "duplicate_submission");
    let records = vec![
        first.clone(),
        first.clone(),
        derived_withdrawal(tenant, 0xB501, target, "late_correction"),
    ];
    let plan = plan_batch(&records);
    let inserted: HashMap<DedupKey, UsageRecordRow> = HashMap::from([(
        dedup_key(&first),
        row_matching(&first, serde_json::Value::Object(serde_json::Map::new())),
    )]);
    let conflict: HashMap<DedupKey, ConflictRead> = HashMap::new();

    let results = store.resolve_batch(&records, &plan, &inserted, &conflict);

    assert_eq!(results.len(), 3, "one result per input row, in input order");
    assert_eq!(results[0].as_ref().expect("the first is accepted").id, first.id);
    assert_eq!(
        results[1].as_ref().expect("the same-reason repeat is absorbed").id,
        first.id
    );
    match &results[2] {
        Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }) => {
            assert_eq!(existing.id, first.id);
            assert_eq!(
                existing.invalidation.as_ref().map(|i| i.reason.as_str()),
                Some("duplicate_submission")
            );
        }
        other => panic!("a second withdrawal under another reason conflicts, got {other:?}"),
    }
}
```

  - in `resolve_batch_conflicts_an_in_batch_duplicate_whose_canonical_fields_differ`,
    change the match arm to
    `Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }) => { assert_eq!(existing.id, records[0].id, "the conflict names the row already holding the slot"); }`;
  - in `batch_retry_predicate_retries_only_transient`, change the conflict
    literal to
    `&UsageCollectorPluginError::idempotency_conflict("k", unit_record(uuid::Uuid::from_u128(1), "k", 1))`.

- [ ] **Step 25: Run the plugin unit tests.**
  - Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin`
  - Expected: PASS.

- [ ] **Step 26: Rewrite the pg ingest tests.** In `tests/records_ingest_integration_pg.rs`:
  - **File header.** Replace "the at-most-one-invalidation guarantee in all
    three shapes it can be broken in (a later call, one batch, two concurrent
    calls)" with "at most one invalidation as a dedup outcome (a later call, one
    batch, two concurrent calls)". Delete the bullet
    "* **The two at-most-one rejection counters.** …".
  - **`ASSERTED_COUNTERS`** becomes:

```rust
const ASSERTED_COUNTERS: &[&str] = &[
    "uc_timescaledb_dedup_absorbed_total",
    "uc_timescaledb_idempotency_conflicts_total",
    "uc_timescaledb_invalidations_total",
    "uc_timescaledb_batch_retries_total",
];
```

  - **The at-most-one section.** Replace everything from the
    `// At most one invalidation per entry` section banner through the end of
    `two_withdrawals_of_one_target_from_different_tenants_race_on_the_index_alone`
    with the code below. The cross-tenant race test is deleted: two withdrawals
    in different tenants are different identities, and the gateway's faithful
    copy is what refuses a cross-tenant withdrawal.

```rust
// ---------------------------------------------------------------------------
// At most one invalidation per entry: a dedup outcome of the derived key
// ---------------------------------------------------------------------------

/// A second withdrawal with the **same** reason code is an exact retry of the
/// first: absorbed, answering with the stored invalidation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_withdrawal_with_the_same_reason_is_absorbed() {
    let (h, store, provider, exporter) = setup_metered().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1_A710);

    let target = store
        .create(common::entry(&meter, tenant, "idem-target", Decimal::new(10, 0)))
        .await
        .expect("create the target");
    let first = store
        .create(common::withdrawal_of(&target))
        .await
        .expect("the first withdrawal is accepted");

    let retry = UsageRecord {
        accepted_at: first.accepted_at + Duration::minutes(1),
        ..common::withdrawal_of(&target)
    };
    let absorbed = store
        .create(retry)
        .await
        .expect("a same-reason withdrawal is absorbed");
    assert_eq!(absorbed.id, first.id);
    assert_eq!(
        absorbed.accepted_at, first.accepted_at,
        "the stored invalidation is returned"
    );

    let withdrawals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE invalidates = $1")
            .bind(target.id)
            .fetch_one(&h.pool)
            .await
            .expect("count withdrawals");
    assert_eq!(withdrawals, 1);

    provider.force_flush().expect("flush metrics");
    assert_eq!(counter_sum(&exporter, "uc_timescaledb_dedup_absorbed_total"), 1);
    assert_eq!(counter_sum(&exporter, "uc_timescaledb_invalidations_total"), 1);
}

/// A second withdrawal under **another** reason code collides on the derived
/// `inv:<target>` key and conflicts, carrying the accepted withdrawal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_withdrawal_with_another_reason_conflicts_carrying_the_first() {
    let (h, store, provider, exporter) = setup_metered().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1_A711);

    let target = store
        .create(common::entry(&meter, tenant, "idem-target", Decimal::new(10, 0)))
        .await
        .expect("create the target");
    let first = store
        .create(common::withdrawal_of(&target))
        .await
        .expect("the first withdrawal is accepted");

    let err = store
        .create(common::withdrawal_of_with_reason(&target, "second_withdrawal"))
        .await
        .expect_err("a divergent second withdrawal is refused");
    match err {
        UsageCollectorPluginError::IdempotencyConflict { existing, .. } => {
            assert_eq!(existing.id, first.id);
            assert_eq!(
                existing.invalidation.as_ref().map(|i| i.reason.as_str()),
                Some("duplicate_submission"),
                "the stored reason code travels with the conflict"
            );
        }
        other => panic!("expected IdempotencyConflict, got {other:?}"),
    }

    let withdrawals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE invalidates = $1")
            .bind(target.id)
            .fetch_one(&h.pool)
            .await
            .expect("count withdrawals");
    assert_eq!(withdrawals, 1);

    provider.force_flush().expect("flush metrics");
    assert_eq!(counter_sum(&exporter, "uc_timescaledb_idempotency_conflicts_total"), 1);
}

/// A batch carrying a withdrawal of a target an **earlier call** already
/// withdrew answers per row: the withdrawal conflicts and the rest of the batch
/// keeps its outcomes. With the store-side index gone nothing can abort the
/// whole statement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_withdrawing_an_already_withdrawn_target_answers_per_row() {
    let (h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1_A712);

    let target = store
        .create(common::entry(&meter, tenant, "idem-target", Decimal::new(10, 0)))
        .await
        .expect("create the target");
    let first = store
        .create(common::withdrawal_of(&target))
        .await
        .expect("the first withdrawal is accepted");
    let unrelated = common::entry(&meter, tenant, "idem-unrelated", Decimal::new(3, 0));

    let results = store
        .create_batch(vec![
            common::withdrawal_of_with_reason(&target, "second_withdrawal"),
            unrelated.clone(),
        ])
        .await
        .expect("the batch as a whole succeeds and answers per row");

    match &results[0] {
        Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }) => {
            assert_eq!(existing.id, first.id);
        }
        other => panic!("row 0 must be IdempotencyConflict, got {other:?}"),
    }
    assert_eq!(
        results[1].as_ref().expect("the unrelated row is accepted").id,
        unrelated.id
    );

    let withdrawals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE invalidates = $1")
            .bind(target.id)
            .fetch_one(&h.pool)
            .await
            .expect("count withdrawals");
    assert_eq!(withdrawals, 1);
}

/// Withdrawals of one target inside **one batch** resolve later against
/// earlier: an identical repeat is absorbed, one under another reason code
/// conflicts, and an unrelated row is unaffected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn withdrawals_of_one_target_in_one_batch_resolve_later_against_earlier() {
    let (h, store, provider, exporter) = setup_metered().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1_A713);

    let target = store
        .create(common::entry(&meter, tenant, "idem-target", Decimal::new(10, 0)))
        .await
        .expect("create the target");
    let w1 = common::withdrawal_of(&target);
    let w2 = common::withdrawal_of_with_reason(&target, "second_withdrawal");
    let unrelated = common::entry(&meter, tenant, "idem-unrelated", Decimal::new(3, 0));

    let results = store
        .create_batch(vec![w1.clone(), w1.clone(), w2, unrelated.clone()])
        .await
        .expect("per-row outcomes");

    assert_eq!(results.len(), 4);
    assert_eq!(results[0].as_ref().expect("w1 accepted").id, w1.id);
    assert_eq!(results[1].as_ref().expect("the repeat is absorbed").id, w1.id);
    match &results[2] {
        Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }) => {
            assert_eq!(existing.id, w1.id);
        }
        other => panic!("row 2 must be IdempotencyConflict, got {other:?}"),
    }
    assert_eq!(results[3].as_ref().expect("unrelated accepted").id, unrelated.id);

    let withdrawals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE invalidates = $1")
            .bind(target.id)
            .fetch_one(&h.pool)
            .await
            .expect("count withdrawals");
    assert_eq!(withdrawals, 1);

    provider.force_flush().expect("flush metrics");
    assert_eq!(counter_sum(&exporter, "uc_timescaledb_dedup_absorbed_total"), 1);
    assert_eq!(counter_sum(&exporter, "uc_timescaledb_idempotency_conflicts_total"), 1);
    assert_eq!(counter_sum(&exporter, "uc_timescaledb_invalidations_total"), 1);
}

/// Two `create` calls withdrawing one target concurrently, under different
/// reason codes: exactly one is accepted, and the loser conflicts carrying the
/// winner. `ON CONFLICT DO NOTHING` on the dedup identity serialises them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_withdrawals_of_one_target_admit_exactly_one() {
    let (h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1_A714);

    let target = store
        .create(common::entry(&meter, tenant, "idem-target", Decimal::new(10, 0)))
        .await
        .expect("create the target");

    let a = common::withdrawal_of(&target);
    let b = common::withdrawal_of_with_reason(&target, "second_withdrawal");
    let (sa, sb) = (store.clone(), store.clone());
    let (ra, rb) = tokio::join!(
        tokio::spawn(async move { sa.create(a).await }),
        tokio::spawn(async move { sb.create(b).await }),
    );
    let ra = ra.expect("task a did not panic");
    let rb = rb.expect("task b did not panic");

    let accepted = usize::from(ra.is_ok()) + usize::from(rb.is_ok());
    assert_eq!(accepted, 1, "exactly one is admitted; got a={ra:?} b={rb:?}");
    let (winner, loser) = if let Ok(winner) = &ra {
        (winner, &rb)
    } else {
        (rb.as_ref().expect("exactly one of the two is Ok"), &ra)
    };
    match loser {
        Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }) => {
            assert_eq!(existing.id, winner.id, "the conflict carries the winner");
        }
        other => panic!("the losing withdrawal must be IdempotencyConflict, got {other:?}"),
    }

    let withdrawals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE invalidates = $1")
            .bind(target.id)
            .fetch_one(&h.pool)
            .await
            .expect("count withdrawals");
    assert_eq!(withdrawals, 1);
}

/// SPEC-DIFF 2.5: `origin` is server-assigned, so one entry arriving first live
/// and then through backfill is one entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_identical_entry_from_live_then_backfill_is_absorbed() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let live = common::entry(&meter, Uuid::from_u128(0x1_A715), "idem-origin", Decimal::new(4, 0));

    let stored = store.create(live.clone()).await.expect("live write");
    let absorbed = store
        .create(UsageRecord {
            origin: RecordOrigin::Backfill,
            ..live
        })
        .await
        .expect("same caller-supplied fields absorb whatever the route");
    assert_eq!(absorbed.id, stored.id);
    assert_eq!(absorbed.origin, RecordOrigin::Live, "the stored entry is returned");
}

/// Decision S-B7: `42.5` then `42.500` under one identity is a conflict.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quantity_at_another_scale_under_one_identity_is_a_conflict() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1_A716);

    let first = common::entry(&meter, tenant, "idem-scale", Decimal::new(425, 1));
    store.create(first.clone()).await.expect("first write");
    let rescaled = common::entry(&meter, tenant, "idem-scale", Decimal::new(42_500, 3));
    assert_eq!(rescaled.id, first.id, "one identity");

    match store.create(rescaled).await {
        Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }) => {
            assert_eq!(existing.quantity.to_string(), "42.5");
        }
        other => panic!("expected IdempotencyConflict, got {other:?}"),
    }
}
```

  - **`existing_id` users.** In the remaining tests
    (`a_divergent_same_key_write_is_an_idempotency_conflict`,
    `batch_outcomes_are_aligned_with_input_order`,
    `an_in_batch_duplicate_resolves_against_the_row_the_batch_wrote`,
    `a_batch_in_which_every_row_conflicts_inserts_nothing_and_stays_aligned`),
    change each `IdempotencyConflict { existing_id, .. }` or
    `{ idempotency_key, existing_id }` pattern to bind `existing`, and compare
    `existing.id` where `existing_id` / `*existing_id` was compared. Do the same
    in `tests/id_uniqueness_integration_pg.rs`.

- [ ] **Step 27: Replace the schema test.** In `tests/schema_integration_pg.rs`, replace
  `the_at_most_one_invalidation_index_is_partial_and_unique`, doc included, with:

```rust
/// The invalidation lookup index is partial and **not** unique: at most one
/// invalidation per entry is a dedup outcome of the derived `inv:<target>` key,
/// not a store-side rule (DESIGN §3.1 "At most one invalidation").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_invalidation_lookup_index_is_partial_and_not_unique() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let def: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes \
         WHERE tablename = 'usage_records' AND indexname = 'usage_records_invalidates_idx'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("usage_records_invalidates_idx must exist");
    assert!(!def.contains("UNIQUE"), "the lookup index must not be unique: {def}");
    assert!(
        def.contains("btree (invalidates, window_end, type_key)"),
        "the index must lead with `invalidates`: {def}"
    );
    assert!(
        def.contains("WHERE (invalidates IS NOT NULL)"),
        "the index must be partial on `invalidates IS NOT NULL`: {def}"
    );

    let unique_over_invalidates: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_indexes WHERE tablename = 'usage_records' \
         AND indexdef LIKE 'CREATE UNIQUE INDEX%' AND indexdef LIKE '%invalidates%'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("count unique indexes");
    assert_eq!(unique_over_invalidates, 0, "no unique index may cover `invalidates`");
}
```

- [ ] **Step 28: Wire the declared level and fix the fixture doc.**
  - In `tests/contract_conformance_pg.rs`, the call becomes
    `contract::run_all(&backend, contract::DedupLevel::Linearizable).await`.
    Add to its doc:
    `//! The plugin declares \`linearizable\` in its README, so the suite runs at that level.`
  - In `tests/common/mod.rs`, replace the `withdrawal_of` doc bullet
    "* **The covered period** — `usage_records_one_invalidation_uniq` is over …
    does not hold for it." with
    "* **The covered period** — two of the five dedup-identity inputs, so a
    withdrawal carrying a different period is a different identity and no
    longer collides with another withdrawal of the same target."

- [ ] **Step 29: Declare the dedup level in the README.** In `README.md`:
  - append to the end of the **Deduplication** bullet:
    " **Dedup level: `linearizable`** (DESIGN §3.10 item 9). The convergence
    bound is zero: a single primary decides every write as it commits, races
    under one identity resolve in Postgres commit order through
    `ON CONFLICT DO NOTHING`, and an identity has converged once the winning
    transaction commits. No write is discarded after convergence, so
    `uc_timescaledb_dedup_late_convergence_total` stays at zero."
  - replace the **Invalidation** bullet with:
    "- **Invalidation** — there is no mutation path. A withdrawal is an ordinary
    appended entry carrying `invalidates` (the entry it withdraws) and
    `reason_code`; the withdrawn entry is never rewritten. At most one
    withdrawal per target follows from its derived `inv:<target>` idempotency
    key: a second withdrawal collides on `usage_records_dedup_uniq` and is
    absorbed (same reason code) or an `IdempotencyConflict` (another reason
    code). `usage_records_invalidates_idx` is a lookup index for the fold, not
    a rule."
  - in **Aggregate path → Why the signed sum is exact**, replace
    "A record carries at most one invalidation (`DIVERGENCES.md` entry 21)." with
    "A record carries at most one invalidation (its derived `inv:<target>` key)."

- [ ] **Step 30: Run everything.**
  - Run: `$UNIT`, `$LINT`, then `$PG`.
  - Expected: PASS. Report `$PG` as not run if Docker is unavailable.
  - Also run `grep -rn 'AlreadyInvalidated\|one_invalidation_uniq\|invalidation_rejected\|existing_id' gears/system/usage-collector --include='*.rs' --include='*.sql' --include='*.md' | grep -v '/docs/'`.
    Expected hits: only `ConflictReason::AlreadyInvalidated`,
    `DomainError::AlreadyInvalidated`, `DomainError::IdempotencyConflict { existing_id }`
    and their uses.

- [ ] **Step 31: Commit.**

```bash
git add -A gears/system/usage-collector
git commit -s -m "feat(usage-collector)!: at most one invalidation as a dedup outcome

Remove the store-side at-most-one rule: the plugin error enum loses
AlreadyInvalidated and IdempotencyConflict carries the stored entry, the host
reports a conflict on a dispatched invalidation as ALREADY_INVALIDATED with
invalidated_by and reason_code, TimescaleDB drops
usage_records_one_invalidation_uniq and resolves collisions by caller-supplied
fields, and the contract harness takes a declared DedupLevel.

BREAKING CHANGE: UsageCollectorPluginError::AlreadyInvalidated is gone and
IdempotencyConflict carries existing: Box<UsageRecord>; a second withdrawal
under the same reason code is now absorbed instead of refused; contract::run_all
takes a DedupLevel."
```

---

### Task 4: Scope the invalidation target lookup

**Files:**
- Modify: `usage-collector/src/domain/authz.rs`, `usage-collector/src/domain/authz_tests.rs`
- Modify: `usage-collector/src/domain/service.rs`, `usage-collector/src/domain/service_tests.rs`
- Modify: `usage-collector/src/domain/invalidation.rs`, `usage-collector/src/domain/test_support.rs`

**Interfaces:**
- Consumes: `authz::scope_to_odata_filter(&AccessScope) -> Result<ast::Expr, DomainError>` (existing).
- Produces:
  - `authz::authorize_attribution_tuple(..) -> Result<AccessScope, DomainError>`;
  - `authz::authorize_usage_record(..) -> Result<AccessScope, DomainError>`;
  - `PendingInvalidationTarget { index, submission, invalidation, record, lookup_scope: Arc<ast::Expr>, lookup_scope_id: usize }`;
  - `InvalidationTargetCache = HashMap<(Uuid, usize), Result<UsageRecord, DomainError>>`.

- [ ] **Step 1: Make the target double honour a tenant scope.** In
  `test_support.rs`, add this free fn above `impl TargetLookupDouble`:

```rust
/// Whether a programmed row satisfies the scope a lookup carries, for the one
/// shape the gateway compiles from a single-tenant permit: `tenant_id eq <uuid>`.
/// Any other shape admits, so tests that do not exercise scope are unaffected.
fn target_scope_admits(scope: &ast::Expr, row: &UsageRecord) -> bool {
    match scope {
        ast::Expr::Compare(field, ast::CompareOperator::Eq, value) => {
            match (field.as_ref(), value.as_ref()) {
                (ast::Expr::Identifier(name), ast::Expr::Value(ast::Value::Uuid(tenant)))
                    if name == "tenant_id" =>
                {
                    row.tenant_id == *tenant
                }
                _ => true,
            }
        }
        _ => true,
    }
}
```

  In `TargetLookupDouble::lookup`, wrap both successful returns so an
  out-of-scope row reads as absent:

```rust
        let row = match self.row_by_id.lock().expect("mutex").get(&id) {
            Some(row) => Some(row.clone()),
            None => self.row.lock().expect("mutex").clone(),
        };
        match row {
            Some(row) if target_scope_admits(scope, &row) => Ok(row),
            Some(_) => Err(UsageCollectorPluginError::UsageRecordNotFound { id }),
            None => Err(not_programmed("get_usage_record")),
        }
```

  That replaces the two trailing `if let Some(row) … return Ok(row.clone());`
  and `self.row…ok_or_else(…)` statements.

- [ ] **Step 2: Write the failing tests.**
  - **`service_tests.rs`, batch module:** append:

```rust
    /// SPEC-DIFF 2.4 / decision S-A1: the target is read under the scope compiled
    /// from the submitter's own `create` permit, so another tenant's row answers
    /// exactly like an absent one, never with a field mismatch that would
    /// confirm it exists.
    #[tokio::test]
    async fn a_withdrawal_naming_another_tenants_row_answers_as_absent() {
        let plugin = HappyPathPlugin::new();
        let caller_tenant = Uuid::from_u128(0x5A1);
        let other_tenant = Uuid::from_u128(0x5A2);
        let target = Uuid::from_u128(0x6A1);
        plugin.set_get_record_for(target, target_row(other_tenant, target));
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.cross_tenant.records.v1",
        );

        let results = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![withdrawal_of(caller_tenant, target, "idem-cross")],
            )
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::NotFound {
                reason: usage_collector_sdk::NotFoundReason::InvalidationTargetNotFound,
                name,
                ..
            }) => assert_eq!(name, &target.to_string()),
            other => panic!("an out-of-scope target must read as absent, got {other:?}"),
        }
        assert_translatable_scope(&plugin, "resolve_invalidation_targets");
        let scope = plugin.last_get_scope().expect("the lookup dispatched");
        assert!(
            scope.contains(&format!("{caller_tenant:?}")) && !scope.contains(&format!("{target:?}")),
            "the lookup reads under the caller's compiled permit, not `id eq <target>`: {scope}"
        );
    }
```

  - **`service_tests.rs`, singular module:** append:

```rust
    #[tokio::test]
    async fn create_usage_record_naming_another_tenants_row_answers_as_absent() {
        let plugin = HappyPathPlugin::new();
        let caller_tenant = Uuid::from_u128(0x70D);
        let other_tenant = Uuid::from_u128(0x70E);
        let target = Uuid::from_u128(0x80D);
        plugin.set_get_record(target_row(other_tenant, target));
        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.cross_tenant.records.v1",
        );

        let err = service
            .create_usage_record(
                &authenticated_ctx(),
                counter_withdrawal(caller_tenant, target, "idem-cross"),
            )
            .await
            .expect_err("an out-of-scope target is refused");

        match err {
            UsageCollectorError::NotFound {
                reason: usage_collector_sdk::NotFoundReason::InvalidationTargetNotFound,
                name,
                ..
            } => assert_eq!(name, target.to_string()),
            other => panic!("an out-of-scope target must read as absent, got {other:?}"),
        }
        assert!(plugin.last_create_record_input().is_none(), "nothing is dispatched");
    }
```

  - **Remove the obsolete test.** Delete
    `the_invalidation_target_scope_translates_for_a_conforming_backend` (the
    `target_pinned_read_filter` regression test near the top of
    `service_tests.rs`), and drop `target_pinned_read_filter` from that file's
    imports. `assert_translatable_scope` stays: its call sites now check the
    compiled permit scope.

- [ ] **Step 3: Run the new tests and watch them fail.**
  - Run: `cargo nextest run -p cf-gears-usage-collector -E 'test(another_tenants_row)'`
  - Expected: FAIL. The lookup still passes `id eq <target>`, so the double
    finds the row and the copy check answers `INVALIDATION_FIELD_MISMATCH` on
    `tenant_id`.

- [ ] **Step 4: Return the permit scope from authz.** In `authz.rs`:
  - `authorize_usage_record` returns `Result<AccessScope, DomainError>` and its
    body stays `authorize_attribution_tuple(enforcer, metrics, op, ctx, &key).await`;
  - `authorize_attribution_tuple` returns `Result<AccessScope, DomainError>`,
    and its gate's success arm becomes `Ok(scope)` instead of `Ok(())`;
  - add to both docs:
    `/// On a permit, returns the granted [\`AccessScope\`]: an invalidation's target lookup reads under it, compiled by [\`scope_to_odata_filter\`] (SPEC-DIFF decision S-A1).`

  Run `cargo check --all-targets -p cf-gears-usage-collector`. In
  `authz_tests.rs`, any comparison against `Ok(())` or binding of the unit value
  becomes `.is_ok()`, or `.expect("permit")` where the value is discarded.

- [ ] **Step 5: Scope the single-path lookup.** In `service.rs`,
  `create_usage_record_inner`:
  - bind the authorization result:
    `let permit_scope = authz::authorize_usage_record(…).await.map_err(UsageCollectorError::from)?;`
  - inside `if let Some((submission, invalidation)) = withdrawal {`, first compile:

```rust
            // SPEC-DIFF decision S-A1: the target is read under the scope of the
            // `create` permit that authorized this submission, so a row outside
            // the caller's grant answers exactly like an absent one.
            let lookup_scope =
                authz::scope_to_odata_filter(&permit_scope).map_err(UsageCollectorError::from)?;
```

  - the lookup becomes `plugin.get_usage_record(invalidation.target, &lookup_scope, true)`.

- [ ] **Step 6: Scope the batch lookups.** In `service.rs`:
  - `type PdpGroupDecision = (Vec<usize>, Result<AccessScope, DomainError>);`
    Import `toolkit_security::AccessScope` if it is not imported.
  - `type InvalidationTargetCache = HashMap<(Uuid, usize), Result<UsageRecord, DomainError>>;`
    Update its doc to say the key is `(target, lookup scope id)`.
  - Add these two fields to `PendingInvalidationTarget`:

```rust
    /// The compiled scope of the `create` permit that authorized this
    /// submission (SPEC-DIFF decision S-A1). The target is read under it.
    lookup_scope: Arc<ast::Expr>,
    /// Identifies [`Self::lookup_scope`] within the request: one id per PDP
    /// tuple group, so entries sharing a permit share a lookup.
    lookup_scope_id: usize,
```

  - In `create_usage_records_inner`, replace the PDP projection loop
    (`for (indices, decision) in pdp_decisions { if let Err(e) = decision { … } }`)
    with the code below:

```rust
        // A permitted tuple group that carries an invalidation keeps its scope,
        // compiled once, for the target lookups (SPEC-DIFF decision S-A1).
        let mut lookup_scopes: Vec<Option<Result<(usize, Arc<ast::Expr>), DomainError>>> =
            (0..submission_count).map(|_| None).collect();
        let mut next_scope_id = 0_usize;
        for (indices, decision) in pdp_decisions {
            match decision {
                Ok(scope) => {
                    if indices.iter().any(|index| withdrawals[*index].is_some()) {
                        let scope_id = next_scope_id;
                        next_scope_id += 1;
                        let compiled = authz::scope_to_odata_filter(&scope)
                            .map(|expr| (scope_id, Arc::new(expr)));
                        for index in indices {
                            lookup_scopes[index] = Some(compiled.clone());
                        }
                    }
                }
                Err(e) => {
                    // A PDP-transport failure (`AuthorizationUnavailable`) and a
                    // plugin `Transient` both lift to `ServiceUnavailable`; their
                    // curated `detail` strings keep them distinguishable for
                    // operator triage without a separate per-record origin tag.
                    for index in indices {
                        results[index] = Some(Err(UsageCollectorError::from(e.clone())));
                        pdp_allowed[index] = false;
                    }
                }
            }
        }
```

  - In the validation loop, replace the pending-target push with:

```rust
            if let Some((submission, invalidation)) = withdrawals[index].take() {
                let (lookup_scope_id, lookup_scope) = match lookup_scopes[index].take() {
                    Some(Ok(scope)) => scope,
                    Some(Err(e)) => {
                        results[index] = Some(Err(UsageCollectorError::from(e)));
                        continue;
                    }
                    None => {
                        results[index] = Some(Err(invariant_breach(format!(
                            "no permit scope was kept for the invalidation at input {index}"
                        ))));
                        continue;
                    }
                };
                pending_targets.push(PendingInvalidationTarget {
                    index,
                    submission,
                    invalidation,
                    record,
                    lookup_scope,
                    lookup_scope_id,
                });
                continue;
            }
```

  - In `resolve_invalidation_targets`, replace the `distinct_targets` set and
    the fan-out with:

```rust
    let distinct_lookups: HashMap<(Uuid, usize), Arc<ast::Expr>> = pending
        .iter()
        .map(|entry| {
            (
                (entry.invalidation.target, entry.lookup_scope_id),
                Arc::clone(&entry.lookup_scope),
            )
        })
        .collect();

    let target_cache: InvalidationTargetCache =
        stream::iter(distinct_lookups.into_iter().map(|(key, scope)| async move {
            let (target, _) = key;
            let outcome = instrument_spi(
                metrics,
                PluginOp::GetUsageRecord,
                plugin.get_usage_record(target, &scope, true),
            )
            .await
            .map_err(|e| match e {
                UsageCollectorPluginError::UsageRecordNotConverged { .. } => {
                    DomainError::TargetNotConverged { target }
                }
                other => DomainError::from(other),
            });
            (key, outcome)
        }))
        .buffer_unordered(TARGET_LOOKUP_FANOUT_CONCURRENCY)
        .collect()
        .await;
```

    In the loop below it:
    - destructure `lookup_scope_id` and `lookup_scope: _` from
      `PendingInvalidationTarget`;
    - read `target_cache.get(&(invalidation.target, lookup_scope_id))`;
    - in the doc paragraph starting "Builds a request-local
      `Map<target, Result<UsageRecord, _>>`", say the map is keyed by
      `(target, permit scope)`, so a batch withdrawing one target twice under
      one permit costs one read.
  - **Delete `target_pinned_read_filter`** and its doc.
  - **`ast` import.** If `service.rs` does not import `toolkit_odata::ast`, add it.

- [ ] **Step 7: Restate the invalidation docs for a scoped read.** In `invalidation.rs`:
  - in `verify_invalidation_target`'s doc, replace the sentence "They name
    **`invalidation.target`, the reference the caller supplied, never
    `target.id`**: the lookup that produced `target` is unscoped, so a
    mis-paired row would otherwise hand the caller an identifier it never sent
    and has no scope for." with "They name **`invalidation.target`, the
    reference the caller supplied, never `target.id`**, so a mis-paired row can
    never hand the caller an identifier it did not send.";
  - replace the paragraph starting "**A rejection names what differs, never what
    it differs from, and that is a security rule rather than a style one.**"
    through "… do not \"improve\" the diagnostic by echoing the target's value
    into it." with:

```rust
/// **A rejection names what differs, never what it differs from.** The target
/// is read under the scope compiled from the submitter's own `create` permit
/// (SPEC-DIFF decision S-A1), so a row outside that grant answers as absent and
/// never reaches this function. The rule is kept as defence in depth: a message
/// carrying the target's value would be an oracle the moment a plugin answered
/// outside the scope it was handed — submit a faithful copy with one field
/// wrong, read the real value out of the 400, iterate. The field *name* leaks
/// nothing (the caller sent that field); the value is the part that must not
/// cross. `UsageCollectorError::invalidation_field_mismatch` is written that
/// way deliberately — do not "improve" the diagnostic by echoing the target's
/// value into it.
```

- [ ] **Step 8: Run everything.**
  - Run: `$UNIT` and `$LINT`.
  - Expected: PASS.
  - An existing service or handler test that programmed a target row under a
    tenant other than the submission's now reads `InvalidationTargetNotFound`.
    That is the intended behaviour. Change such a fixture to the submission's
    tenant only when the test is about something other than scope, and say so
    in the task report.
  - No test is added for "scope fails to compile". The per-entry gate
    (`scope_admits_attribution_tuple`) and `scope_to_odata_filter` reject the
    same shapes (`authz_tests::gates_agree_on_leaf_verdict_across_scope_corpus`),
    so no permit reaches the compile step and fails it. The `?` there is an
    invariant guard.

- [ ] **Step 9: Commit.**

```bash
git add -A gears/system/usage-collector
git commit -s -m "fix(usage-collector)!: read invalidation targets under the caller's permit

The target lookup reads under the scope compiled from the create permit that
authorized the submission (SPEC-DIFF decision S-A1), so another tenant's row
answers exactly like an absent one instead of revealing its fields through
INVALIDATION_FIELD_MISMATCH.

BREAKING CHANGE: a withdrawal naming an entry outside the caller's grant is
now answered 404, as for an entry that does not exist."
```

---

### Task 5: Resolve same-identity entries at the gateway

**Files:**
- Modify: `usage-collector/src/domain/service.rs`, `usage-collector/src/domain/service_tests.rs`

**Interfaces:**
- Consumes: `lift_dispatch_error` (Task 3), `UsageRecord::caller_supplied_eq` (Task 1), `UsageCollectorError::target_not_converged` (Task 2).
- Produces (private to `service.rs`):
  - `enum Resolution { Against(UsageRecord), Failed(DomainError) }`;
  - `fn settle_dispatched(outcome: Result<UsageRecord, UsageCollectorPluginError>, invalidation: Option<&Invalidation>) -> (Result<UsageRecord, UsageCollectorError>, Resolution)`;
  - `fn resolve_follower(entry: &UsageRecord, resolution: &Resolution) -> Result<UsageRecord, UsageCollectorError>`.

- [ ] **Step 1: Write the failing tests.** In `service_tests.rs`, batch module
  (the module with `ordinary_record`, `withdrawal_of`, `target_row`,
  `service_with_permit`), append:

```rust
    #[tokio::test]
    async fn identical_entries_in_one_batch_dispatch_once_and_share_the_stored_row() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5B1);
        let entry = ordinary_record(tenant_id, "idem-twice");
        let stored = UsageRecord {
            origin: usage_collector_sdk::RecordOrigin::Backfill,
            ..projected(&entry)
        };
        plugin.set_create_records(vec![Ok(stored.clone())]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.identity.identical_pair.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![entry.clone(), entry])
            .await
            .expect("batch dispatch succeeded");

        assert_eq!(
            plugin.last_create_records_input().expect("dispatched").len(),
            1,
            "one dispatch per dedup identity"
        );
        for (position, result) in results.iter().enumerate() {
            assert_eq!(
                result.as_ref().expect("accepted"),
                &stored,
                "entry {position} reports the stored row"
            );
        }
    }

    #[tokio::test]
    async fn a_divergent_later_entry_conflicts_with_the_earlier_one() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5B2);
        let first = ordinary_record(tenant_id, "idem-split");
        let mut second = first.clone();
        second.quantity = crate::domain::test_support::qty("11");
        let stored = projected(&first);
        plugin.set_create_records(vec![Ok(stored.clone())]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.identity.divergent_pair.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![first, second])
            .await
            .expect("batch dispatch succeeded");

        assert_eq!(results[0].as_ref().expect("the first is accepted"), &stored);
        match results[1].as_ref() {
            Err(UsageCollectorError::Conflict {
                reason: ConflictReason::IdempotencyConflict,
                name,
                ..
            }) => assert_eq!(name, &stored.id.to_string()),
            other => panic!("the later divergent entry conflicts, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn two_withdrawals_of_one_target_in_one_batch_report_already_invalidated() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5B3);
        let target = Uuid::from_u128(0x6B3);
        plugin.set_get_record_for(target, target_row(tenant_id, target));
        let first = withdrawal_of(tenant_id, target, "idem-w1");
        let mut second = withdrawal_of(tenant_id, target, "idem-w2");
        second.invalidation = Some(Invalidation {
            target,
            reason: ReasonCode::new("late_correction").expect("valid reason code"),
        });
        let stored = projected(&first);
        plugin.set_create_records(vec![Ok(stored.clone())]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.identity.withdrawal_pair.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![first, second])
            .await
            .expect("batch dispatch succeeded");

        assert_eq!(plugin.last_create_records_input().expect("dispatched").len(), 1);
        assert!(results[0].is_ok(), "the first withdrawal is accepted: {:?}", results[0]);
        match results[1].as_ref() {
            Err(UsageCollectorError::Conflict {
                reason: ConflictReason::AlreadyInvalidated,
                name,
                invalidated_by,
                reason_code,
                ..
            }) => {
                assert_eq!(name, &target.to_string());
                assert_eq!(*invalidated_by, Some(stored.id));
                assert_eq!(
                    reason_code.as_ref().map(ReasonCode::as_str),
                    Some("emitter_defect")
                );
            }
            other => panic!("the later withdrawal is AlreadyInvalidated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_later_entry_equal_to_what_the_store_holds_is_accepted_with_it() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5B4);
        let held = ordinary_record(tenant_id, "idem-held");
        let mut first = held.clone();
        first.quantity = crate::domain::test_support::qty("11");
        let stored = projected(&held);
        plugin.set_create_records(vec![Err(UsageCollectorPluginError::idempotency_conflict(
            "idem-held",
            stored.clone(),
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.identity.against_stored.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![first, held])
            .await
            .expect("batch dispatch succeeded");

        assert!(
            matches!(
                results[0].as_ref(),
                Err(UsageCollectorError::Conflict { reason: ConflictReason::IdempotencyConflict, .. })
            ),
            "the first conflicts with the stored row: {:?}",
            results[0]
        );
        assert_eq!(
            results[1].as_ref().expect("the later entry matches what the store holds"),
            &stored
        );
    }

    #[tokio::test]
    async fn a_later_entry_shares_the_earlier_entrys_backend_failure() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5B5);
        let entry = ordinary_record(tenant_id, "idem-blip");
        plugin.set_create_records(vec![Err(UsageCollectorPluginError::transient("backend blip"))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.identity.shared_failure.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![entry.clone(), entry])
            .await
            .expect("batch dispatch succeeded");

        for (position, result) in results.iter().enumerate() {
            assert!(
                matches!(result, Err(UsageCollectorError::ServiceUnavailable { .. })),
                "entry {position} carries the backend failure: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_withdrawal_of_an_entry_in_the_same_batch_is_not_converged() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5B6);
        let record = ordinary_record(tenant_id, "idem-same-batch");
        let record_id = projected(&record).id;
        plugin.set_get_usage_record_not_found(record_id);
        plugin.set_create_records(vec![Ok(projected(&record))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.identity.same_batch_target.records.v1",
        );

        let results = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![record, withdrawal_of(tenant_id, record_id, "idem-same-batch")],
            )
            .await
            .expect("batch dispatch succeeded");

        assert!(results[0].is_ok(), "the record is accepted: {:?}", results[0]);
        match results[1].as_ref() {
            Err(UsageCollectorError::Conflict {
                reason: ConflictReason::TargetNotConverged,
                name,
                ..
            }) => assert_eq!(name, &record_id.to_string()),
            other => panic!("a target submitted in the same batch is not converged, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run the new tests and watch them fail.**
  - Run: `cargo nextest run -p cf-gears-usage-collector -E 'test(one_batch) | test(later_entry) | test(same_batch)'`
  - Expected: FAIL. Both entries are dispatched (`len() == 2`, and the double's
    one programmed result is a count mismatch), and the same-batch target
    answers `NotFound`.

- [ ] **Step 3: Add the resolution helpers.** In `service.rs`, after
  `PendingInvalidationTarget`:

```rust
/// What a later same-identity entry of a batch resolves against, once the first
/// entry of its identity has been dispatched (DESIGN §3.1 "Collision
/// resolution", SPEC-DIFF decision S-B6).
enum Resolution {
    /// The entry the store holds for the identity: the first entry's accepted
    /// row, or the stored entry its `IdempotencyConflict` carried.
    Against(UsageRecord),
    /// The first entry failed for a reason that holds no stored entry; a later
    /// entry is told the same.
    Failed(DomainError),
}

/// Lift one dispatched entry's plugin outcome, and keep what a later entry of
/// the same identity resolves against.
fn settle_dispatched(
    outcome: Result<UsageRecord, UsageCollectorPluginError>,
    invalidation: Option<&Invalidation>,
) -> (Result<UsageRecord, UsageCollectorError>, Resolution) {
    match outcome {
        Ok(stored) => (Ok(stored.clone()), Resolution::Against(stored)),
        Err(UsageCollectorPluginError::IdempotencyConflict {
            idempotency_key,
            existing,
        }) => {
            let stored = (*existing).clone();
            let lifted = lift_dispatch_error(
                UsageCollectorPluginError::IdempotencyConflict {
                    idempotency_key,
                    existing,
                },
                invalidation,
            );
            (Err(UsageCollectorError::from(lifted)), Resolution::Against(stored))
        }
        Err(other) => {
            let lifted = lift_dispatch_error(other, invalidation);
            (
                Err(UsageCollectorError::from(lifted.clone())),
                Resolution::Failed(lifted),
            )
        }
    }
}

/// Resolve a later same-identity entry the way the store would have: absorbed
/// into the stored entry when its caller-supplied fields are equal, a conflict
/// lifted by its own kind otherwise.
fn resolve_follower(
    entry: &UsageRecord,
    resolution: &Resolution,
) -> Result<UsageRecord, UsageCollectorError> {
    match resolution {
        Resolution::Against(stored) if stored.caller_supplied_eq(entry) => Ok(stored.clone()),
        Resolution::Against(stored) => Err(UsageCollectorError::from(lift_dispatch_error(
            UsageCollectorPluginError::idempotency_conflict(
                entry.idempotency_key.as_str(),
                stored.clone(),
            ),
            entry.invalidation.as_ref(),
        ))),
        Resolution::Failed(error) => Err(UsageCollectorError::from(error.clone())),
    }
}
```

- [ ] **Step 4: Dispatch one entry per identity.** In `create_usage_records_inner`:
  - before the PDP tuple loop (`let mut distinct_tuples …`), record the
    batch's derived ids:

```rust
        // Every derived id in this request, so a withdrawal naming another entry
        // of the same batch is told its target has not converged rather than
        // that it does not exist (SPEC-DIFF 12.2, S-B6).
        let batch_ids: HashSet<Uuid> = derived.iter().map(|(_, record)| record.id).collect();
```

  - pass `&batch_ids` to `resolve_invalidation_targets`:
    - add it as its last parameter, `batch_ids: &HashSet<Uuid>`;
    - add `#[allow(clippy::too_many_arguments)] // the pre-pass reads the request's caches and writes its result slots; each argument is one of those`;
    - in its loop, replace the `Some(Err(DomainError::UsageRecordNotFound { .. }))` arm body with:

```rust
                results[index] = Some(Err(if batch_ids.contains(&invalidation.target) {
                    UsageCollectorError::target_not_converged(invalidation.target)
                } else {
                    UsageCollectorError::invalidation_target_not_found(invalidation.target)
                }));
                continue;
```

  - replace the whole `if !eligible.is_empty() { … }` dispatch block, keeping
    its `@cpt` markers around the same statements, with:

```rust
        // One dispatch per dedup identity (DESIGN §3.1 "Collision resolution",
        // SPEC-DIFF decision S-B6). `eligible` is in input order, so the first
        // entry of each identity is the earliest; later ones resolve against
        // what the store holds for it once it returns.
        let mut representative_of: HashMap<Uuid, usize> = HashMap::new();
        let mut representatives: Vec<(usize, UsageRecord)> = Vec::new();
        let mut followers: Vec<(usize, UsageRecord)> = Vec::new();
        for (index, record) in eligible {
            if representative_of.contains_key(&record.id) {
                followers.push((index, record));
            } else {
                representative_of.insert(record.id, representatives.len());
                representatives.push((index, record));
            }
        }

        if !representatives.is_empty() {
            let (indices, dispatched): (Vec<usize>, Vec<UsageRecord>) =
                representatives.into_iter().unzip();
            let dispatched_invalidations: Vec<Option<Invalidation>> = dispatched
                .iter()
                .map(|record| record.invalidation.clone())
                .collect();
            let spi_results = instrument_spi(
                self.metrics.as_ref(),
                PluginOp::CreateUsageRecords,
                plugin.create_usage_records(dispatched),
            )
            .await
            .map_err(|e| UsageCollectorError::from(DomainError::from(e)))?;

            if spi_results.len() != indices.len() {
                return Err(invariant_breach(format!(
                    "plugin returned {} per-record results for {} dispatched records",
                    spi_results.len(),
                    indices.len()
                )));
            }

            let mut resolutions: Vec<Resolution> = Vec::with_capacity(indices.len());
            for ((index, spi_result), invalidation) in indices
                .into_iter()
                .zip(spi_results)
                .zip(dispatched_invalidations)
            {
                let (outcome, resolution) = settle_dispatched(spi_result, invalidation.as_ref());
                results[index] = Some(outcome);
                resolutions.push(resolution);
            }

            for (index, record) in followers {
                results[index] = Some(
                    match representative_of
                        .get(&record.id)
                        .and_then(|position| resolutions.get(*position))
                    {
                        Some(resolution) => resolve_follower(&record, resolution),
                        None => Err(invariant_breach(format!(
                            "no dispatched entry resolved identity {} of input {index}",
                            record.id
                        ))),
                    },
                );
            }
        }
```

- [ ] **Step 5: Run everything.**
  - Run: `$UNIT` and `$LINT`.
  - Expected: PASS. `withdrawals_of_one_target_share_a_single_lookup` still
    passes: five identical withdrawals now dispatch once. If it asserted a
    dispatched count of five, change it to one and say why in the assertion
    message ("one dispatch per identity").

- [ ] **Step 6: Commit.**

```bash
git add -A gears/system/usage-collector
git commit -s -m "feat(usage-collector): resolve same-identity entries at the gateway

A batch sends one entry per dedup identity to the plugin and resolves later
entries against what the store holds for it: equal caller-supplied fields are
accepted with the stored entry, different ones conflict. A withdrawal naming
another entry of the same batch answers TARGET_NOT_CONVERGED."
```

---

### Task 6: Refuse an explicit null key; regenerate OpenAPI; tighten E2E

**Files:**
- Modify: `usage-collector-sdk/src/models.rs` (`CreateUsageRecordWire`), `usage-collector-sdk/src/models_tests.rs`
- Modify: `usage-collector/src/api/rest/dto.rs`, `usage-collector/src/api/rest/handlers/usage_records.rs`, `usage-collector/src/api/rest/handlers/usage_records_tests.rs`
- Modify: `docs/api/api.json` (regenerated), `testing/e2e/suites/usage_collector/test_integration_seams.py`
- Modify: `docs/superpowers/specs/2026-09-15-usage-collector-spec-diff-roadmap.md`

**Interfaces:**
- Consumes: `UsageCollectorError::idempotency_key_on_invalidation()`, `UsageCollectorError::missing_idempotency_key()` (slice A).

- [ ] **Step 1: Write the failing SDK tests.** Append to `models_tests.rs`:

```rust
mod explicit_null_idempotency_key {
    use crate::models::CreateUsageRecord;

    fn body(extra: serde_json::Value) -> serde_json::Value {
        let mut body = serde_json::json!({
            "gts_type_id": "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1~",
            "tenant_id": "00000000-0000-4000-8000-000000000001",
            "resource_ref": { "resource_id": "res-1", "resource_type": "compute.vm" },
            "quantity": "1",
            "window_start": "2023-11-14T22:13:20Z",
            "window_end": "2023-11-14T23:13:20Z",
        });
        let object = body.as_object_mut().expect("object");
        for (key, value) in extra.as_object().expect("extra is an object") {
            object.insert(key.clone(), value.clone());
        }
        body
    }

    #[test]
    fn an_explicit_null_key_is_refused_on_a_record() {
        let refused = serde_json::from_value::<CreateUsageRecord>(body(
            serde_json::json!({ "idempotency_key": null }),
        ));
        assert!(refused.is_err(), "`null` is not a key: {refused:?}");
    }

    #[test]
    fn an_explicit_null_key_is_refused_on_an_invalidation() {
        let refused = serde_json::from_value::<CreateUsageRecord>(body(serde_json::json!({
            "idempotency_key": null,
            "invalidates": "00000000-0000-4000-8000-0000000000aa",
            "reason_code": "emitter_defect",
        })));
        assert!(refused.is_err(), "the property is forbidden on an invalidation: {refused:?}");
    }

    #[test]
    fn an_absent_key_on_an_invalidation_still_decodes() {
        let decoded = serde_json::from_value::<CreateUsageRecord>(body(serde_json::json!({
            "invalidates": "00000000-0000-4000-8000-0000000000aa",
            "reason_code": "emitter_defect",
        })))
        .expect("an invalidation carries no key");
        assert!(decoded.idempotency_key.is_none());
    }
}
```

- [ ] **Step 2: Write the failing handler tests.** In `usage_records_tests.rs`:
  - add this helper after `create_request_json_with`:

```rust
/// [`create_request_json_with`] with the base body's `idempotency_key` removed,
/// the shape an invalidation is submitted in.
fn create_withdrawal_json_with(extra: &serde_json::Value) -> serde_json::Value {
    let mut body = create_request_json_with(extra);
    body["records"][0]
        .as_object_mut()
        .expect("record object")
        .remove("idempotency_key");
    body
}
```

  - in the two existing tests that send `"idempotency_key": null` (the
    unfaithful-copy test and `a_faithful_copy_reaches_the_plugin`):
    - call `create_withdrawal_json_with` instead of `create_request_json_with`;
    - delete the `"idempotency_key": null,` line;
    - delete the comment sentences about nulling it out;
  - append:

```rust
async fn dispatch_null_key_body(suffix: &str, extra: serde_json::Value) -> serde_json::Value {
    let plugin = HappyPathPlugin::new();
    let service = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build(
            Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
            suffix,
        );
    let req: CreateUsageRecordsRequest = serde_json::from_value(create_request_json_with(&extra))
        .expect("the request body deserializes");
    let response = handle_create_usage_records(
        Extension(authenticated_ctx()),
        Extension(service),
        Json(req),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::MULTI_STATUS);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("body is JSON");
    body["results"][0].clone()
}

#[tokio::test]
async fn an_explicit_null_key_on_an_invalidation_is_refused_as_key_on_invalidation() {
    let item = dispatch_null_key_body(
        "test.handler.create_records.null_key_invalidation.v1",
        serde_json::json!({
            "invalidates": Uuid::from_u128(0x4244).to_string(),
            "reason_code": HAPPY_REASON_CODE,
            "idempotency_key": null,
        }),
    )
    .await;
    assert_eq!(rejected_violation_field(&item), "idempotency_key");
    assert_eq!(rejected_violation_reason(&item), "KEY_ON_INVALIDATION");
}

#[tokio::test]
async fn an_explicit_null_key_on_a_record_is_refused_as_validation() {
    let item = dispatch_null_key_body(
        "test.handler.create_records.null_key_record.v1",
        serde_json::json!({ "idempotency_key": null }),
    )
    .await;
    assert_eq!(rejected_violation_field(&item), "idempotency_key");
    assert_eq!(rejected_violation_reason(&item), "VALIDATION");
}
```

- [ ] **Step 3: Run them and watch them fail.**
  - Run:
    - `cargo nextest run -p cf-gears-usage-collector-sdk -E 'test(explicit_null)'`
    - `cargo nextest run -p cf-gears-usage-collector -E 'test(explicit_null_key)'`
  - Expected: FAIL. The SDK accepts `null` as `None`. On REST the invalidation
    passes decoding, and the record fails later as a missing key with a
    different path.

- [ ] **Step 4: Refuse `null` in the SDK create decoder.** In `models.rs`, change
  the `idempotency_key` field of `CreateUsageRecordWire` to the version below,
  and add the fn next to the struct:

```rust
    #[serde(default, deserialize_with = "present_idempotency_key")]
    idempotency_key: Option<IdempotencyKey>,
```

```rust
/// Decodes a **present** `idempotency_key`. Absence is `serde(default)`'s
/// `None`; an explicit `null` is refused, because the published schema types the
/// property `string` and forbids it outright on an invalidation.
fn present_idempotency_key<'de, D>(deserializer: D) -> Result<Option<IdempotencyKey>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    IdempotencyKey::deserialize(deserializer).map(Some)
}
```

- [ ] **Step 5: Refuse `null` on REST.** In `handlers/usage_records.rs`:
  - change `decode_record_entry` to check before decoding:

```rust
fn decode_record_entry(raw: serde_json::Value) -> Result<CreateUsageRecordRequest, Problem> {
    if let Some(problem) = explicit_null_idempotency_key(&raw) {
        return Err(problem);
    }
    serde_json::from_value::<CreateUsageRecordRequest>(raw).map_err(|err| {
        let message = err.to_string();
        let field = serde_field_name(&message).unwrap_or("records");
        Problem::from(
            UsageRecordResource::invalid_argument()
                .with_field_violation(field, message.as_str(), "VALIDATION")
                .create(),
        )
    })
}

/// An explicit `"idempotency_key": null` is refused, not read as an absent key:
/// the published schema types the property `string`. A record answers as a
/// missing key does; an invalidation answers `KEY_ON_INVALIDATION`, as a
/// supplied key does, because the property is forbidden there.
fn explicit_null_idempotency_key(raw: &serde_json::Value) -> Option<Problem> {
    if !raw
        .get("idempotency_key")
        .is_some_and(serde_json::Value::is_null)
    {
        return None;
    }
    let is_invalidation = raw
        .get("invalidates")
        .is_some_and(|value| !value.is_null());
    let err = if is_invalidation {
        UsageCollectorError::idempotency_key_on_invalidation()
    } else {
        UsageCollectorError::missing_idempotency_key()
    };
    Some(Problem::from(usage_collector_error_to_canonical(err)))
}
```

  - in `dto.rs`, add `#[schema(nullable = false)]` on
    `CreateUsageRecordRequest.idempotency_key`, right under its
    `#[serde(default, …)]` line, and append to its doc:
    `/// An explicit \`null\` is refused.`

- [ ] **Step 6: Regenerate OpenAPI.**
  - Run: `make openapi`, then `grep -n '"idempotency_key"' -A6 docs/api/api.json`.
  - Expected: the `CreateUsageRecordRequest` property reads `"type": "string"`,
    not `["string","null"]`. The `file-storage` entry keeps its nullable type.
  - If utoipa ignores `nullable = false`, replace it with
    `#[schema(value_type = String)]`, regenerate, and check again.
  - Run `cargo nextest run -p cf-gears-usage-collector -E 'test(openapi)'`. If
    `openapi_contract_tests.rs` lists an exception this change resolves, remove
    it.

- [ ] **Step 7: Tighten the E2E seam.** In `testing/e2e/suites/usage_collector/test_integration_seams.py`,
  replace `test_invalidation_is_admitted_at_most_once` with:

```python
async def test_invalidation_is_admitted_at_most_once(api, make_meter):
    """Seam: at most one invalidation per target, as a dedup outcome.

    Every withdrawal of one target derives the same `inv:<target_id>` key, so a
    second withdrawal is an ordinary collision on that identity (DESIGN §3.1
    "At most one invalidation"). Under the same reason code it is an exact
    retry: accepted, answering with the stored invalidation. Under another
    reason code it is refused per record inside a 207 as `ALREADY_INVALIDATED`,
    whose context names the invalidation in place and its reason code.
    """
    meter_id = await make_meter()
    period = covered_period()
    async with api() as client:
        created = accepted_records(await client.post("/records", json={
            "records": [record_payload(meter_id, quantity="3", period=period)]
        }))
        target_id = created[0]["id"]

        withdrawal = accepted_records(await client.post("/records", json={
            "records": [record_payload(
                meter_id, quantity="3", period=period, invalidates=target_id,
            )]
        }))
        assert withdrawal[0]["entry_type"] == "invalidation"
        assert withdrawal[0]["invalidates"] == target_id
        assert withdrawal[0]["reason_code"] == "E2E_CORRECTION"

        retried = accepted_records(await client.post("/records", json={
            "records": [record_payload(
                meter_id, quantity="3", period=period, invalidates=target_id,
            )]
        }))

        second = await client.post("/records", json={
            "records": [record_payload(
                meter_id, quantity="3", period=period, invalidates=target_id,
                reason_code="E2E_CORRECTION_SECOND",
            )]
        })

        # The target is untouched by any attempt: an invalidation appends.
        target = await client.get(f"/records/{target_id}")

    assert retried[0]["id"] == withdrawal[0]["id"], retried
    assert retried[0]["accepted_at"] == withdrawal[0]["accepted_at"], (
        "an absorbed retry answers with the stored invalidation"
    )

    rejected = rejected_records(second)
    assert len(rejected) == 1, rejected
    problem = rejected[0]
    assert problem["status"] == 409, problem
    assert problem["context"]["reason"] == "ALREADY_INVALIDATED", problem
    assert problem["context"]["invalidated_by"] == withdrawal[0]["id"], problem
    assert problem["context"]["reason_code"] == "E2E_CORRECTION", problem

    assert target.status_code == 200, target.text
    assert target.json()["entry_type"] == "record"
```

  Run `grep -rn 'ALREADY_INVALIDATED\|IDEMPOTENCY_CONFLICT' testing/e2e/suites/usage_collector`.
  Any other assertion accepting either reason becomes the single expected one.

- [ ] **Step 8: Verify the whole slice.**
  - Run: `$UNIT`, `$LINT`, `$PG`, and `make openapi` followed by
    `git diff --exit-code docs/api/api.json` (unchanged after regeneration).
  - Run the usage-collector e2e suite (`make e2e-usage-collector`) if it runs
    locally; otherwise report that it was not run.
  - Expected: all PASS.

- [ ] **Step 9: Update the roadmap.** In
  `docs/superpowers/specs/2026-09-15-usage-collector-spec-diff-roadmap.md`, set
  slice B's Status to `Implemented (commits <first>..<last>)`, filling in the
  short hashes of Tasks 1–6. Under "Hand-offs slice B leaves", add any
  divergence found during execution (for example a fixture changed in Task 4,
  Step 8).

- [ ] **Step 10: Commit.**

```bash
git add -A gears/system/usage-collector docs/api/api.json testing/e2e/suites/usage_collector docs/superpowers/specs/2026-09-15-usage-collector-spec-diff-roadmap.md
git commit -s -m "fix(usage-collector)!: refuse an explicit null idempotency key

An explicit \"idempotency_key\": null is refused: KEY_ON_INVALIDATION on an
invalidation and VALIDATION on a record, on REST and in the SDK create decoder.
The OpenAPI property is no longer nullable, and the e2e at-most-once seam
asserts ALREADY_INVALIDATED with its context.

BREAKING CHANGE: a create body carrying \"idempotency_key\": null is rejected
instead of being read as an absent key."
```
