# Usage Collector — SPEC-DIFF roadmap

**Started**: 2026-09-15
**Input**: `SPEC-DIFF.md` (repo root), in particular its **Decisions** table
**Process**: superpowers, one cycle per slice:
`brainstorming` → spec in `docs/superpowers/specs/` → `writing-plans` → plan in
`docs/superpowers/plans/` → `subagent-driven-development` or `executing-plans`.

This file is the entry point for resuming the work in a new session. Update the
status table whenever a slice's spec, plan or implementation lands.

## Cross-slice decisions

These are settled, so do not re-ask them.

- **Code only.** The Decisions table in `SPEC-DIFF.md` overrides the spec docs
  where they disagree. Its "Spec edit" column is applied separately by the
  owner, so slices do not edit `DESIGN.md`, `PRD.md`, `usage-collector-v1.yaml`,
  `ADR/*` or `schemas/*`.
- **Slice by slice.** Each slice gets its own spec, plan and execution, and
  builds on the code the previous slice landed.
- **Stale docs.** `DECOMPOSITION.md` and `docs/features/*` are stale. Never use
  them as a basis.
- **Item 4.3 belongs to slice C.** Item 4.3 moves the PDP call ahead of
  validation. Slice C does it together with removing the `BACKFILL` action
  (item 3.1), so the attribution tuple key changes once.
- **Negative zero quantity is rejected.** It cannot round-trip digit for digit:
  `rust_decimal` renders it as `0` and Postgres has none. This was decided in
  slice A and still needs a spec amendment.
- **Migration.** The TimescaleDB `0001_init.sql` is edited in place, because the
  gear is unreleased.
- **Commits.** Commit with `git commit -s`. A wire-breaking commit uses `!` and
  a `BREAKING CHANGE:` trailer.

## Slices

| Slice | Scope (SPEC-DIFF items) | Spec | Plan | Status |
| --- | --- | --- | --- | --- |
| **A** Record and ingestion wire shape | 1.1–1.4, 1.7, 1.8, 4.1 (+11 touched) | `2026-09-15-usage-collector-record-shape-design.md` | `2026-09-15-usage-collector-record-shape.md` | Implemented (commits 06196bd1b..HEAD), final review fixes applied |
| **B** Invalidation and dedup model | 2.1–2.7, 8.1, 8.2 | `2026-09-15-usage-collector-invalidation-dedup-design.md` | `2026-09-15-usage-collector-invalidation-dedup.md` | Implemented (commits 2a49d066d..68620a391), final review fixes applied |
| **C** Backfill, quotas, PDP order | 3.1, 3.2, 4.2, 4.3, 10.1 | — | — | Not started |
| **D** Raw read and query rules | 8.6, 6.1, 7.1–7.8, 1.5, 1.6 | — | — | Not started |
| **E** Contract suite and plugin README | 8.3–8.5 | — | — | Not started |
| **F** Authz, types and registry | 5.1, 9.2–9.6 | — | — | Not started |
| **G** Errors, metrics, observability, docs | 10.2–10.7, 11 | — | — | Not started |

Item 9.1 (the declaration mirror) is deferred by decision S-T1. It is not part
of any slice.

## Hand-offs slice A leaves for later slices

- **For B:**
  - The reference plugin and TimescaleDB still compare `origin` on a dedup
    collision (item 2.5).
  - The faithful-copy check and `UsageQuantity` equality are still numeric,
    while decision S-B7 requires a textual comparison.
  - `AlreadyInvalidated` and the store-side at-most-one index still exist (2.1).
  - Some tests accept `AlreadyInvalidated | IdempotencyConflict`; tighten them.
  - An explicit `"idempotency_key": null` is accepted on an invalidation (REST
    DTO `Option<String>` with `serde(default)`, api.json type
    `["string","null"]`); the YAML forbids the property on the invalidation
    branch — reject an explicit null.
- **For B/E:** `at_most_one_invalidation` (contract), three PG ingest tests and
  the e2e `test_invalidation_is_admitted_at_most_once` currently prove a
  second withdrawal is refused but accept
  `ALREADY_INVALIDATED | IDEMPOTENCY_CONFLICT`; tighten to `ALREADY_INVALIDATED`
  once the host lifts by entry kind.
- **For C:**
  - The pipeline order is unchanged (4.3).
  - The service stamps `accepted_at` from the same per-request `now` the
    covered-period bounds use.
- **For E:** `at_most_one_invalidation` accepts either error under derived
  `inv:` keys and carries a comment. Rewrite it (8.3).
- **For G:** handler-level per-entry decode rejections are not counted in
  `uc_ingestion_records_total` (10.4).

## Hand-offs slice B leaves for later slices (from its spec)

- **For E:**
  - No contract check catches a backend that still compares `origin` on a
    collision. Add that mutant with `dedup-floor`.
  - `at-most-one-invalidation`'s `Eventual` branch runs against no real
    eventual backend.
  - `run_all` takes a `DedupLevel`, but only `at-most-one-invalidation` reads
    it. `dedup-concurrent` should read it too.
  - The TimescaleDB README declares only the dedup level (§3.10 item 9). The
    other 8.4 items remain.
- **For G:** `AlreadyInvalidated` and `TargetNotConverged` are counted as
  `invalidation_rule` provisionally. Reconcile them with the §3.11.5 labels.

Added when slice B landed:

- **For E:**
  - `at-most-one-invalidation`'s `Eventual` branch now runs against the
    reference backend at `Eventual { convergence_bound: ZERO }`. No real
    eventual backend exercises it yet.
  - The pg test `two_concurrent_withdrawals_of_one_target_admit_exactly_one`
    is effectively sequential: the per-scope acceptance-sequence row lock
    serialises both writers. Its doc says `ON CONFLICT` does. A real race
    needs the concurrent harness.
- **For G (errors):**
  - `UsageCollectorError` is 144 bytes because `Conflict` carries
    `invalidated_by` and `reason_code`, so 32 items carry a
    `clippy::result_large_err` allow. Moving the two fields into one boxed
    detail would remove all 32. Weigh that with the 10.1 variant work.
  - An explicit `"idempotency_key": null` is refused before serde decoding,
    so a body that is also malformed elsewhere can get a less precise
    diagnostic. The SDK decoder refuses the null with an untyped serde error.
- **Parked defence in depth:** `lift_dispatch_error` does not check that the
  stored invalidation's target equals the dispatched one. A conforming plugin
  cannot violate this, because both share one `inv:<target>` identity.
- **Spec amendments:**
  - Spec §5.3 claimed `DIVERGENCES.md` does not exist; it does.
  - `DIVERGENCES.md` entries 21 (restated) and 22 (resolved) were updated in
    code.
  - A withdrawal naming another entry of the same batch answers 409
    `TARGET_NOT_CONVERGED` instead of 404. Add this to DESIGN §3.1 / S-B6.
- **Merge message:** the slice B commit trailers do not list the full breaking
  surface. The merge message should:
  - `UsageCollectorPluginV1::get_usage_record` takes `converged_only`;
  - the plugin `AlreadyInvalidated` variant is removed;
  - `IdempotencyConflict` carries `existing: Box<UsageRecord>`;
  - `UsageCollectorError::Conflict` gains `invalidated_by` and `reason_code`;
  - `already_invalidated` takes three arguments;
  - `contract::run_all` takes a `DedupLevel`;
  - `UsageQuantity` equality is textual;
  - a same-reason second withdrawal is absorbed;
  - an out-of-scope target answers 404;
  - a same-batch target answers 409 `TARGET_NOT_CONVERGED`;
  - an explicit null key is refused.
- **Tooling (outside the slices):** `make openapi GEAR=<gear>` always skips,
  because `GEAR_HAS_SERVER_FEATURE` is used in an `ifeq` before it is defined
  in the Makefile. Plain `make openapi` works.

## Resuming

In a new session:

1. Read this file, then `SPEC-DIFF.md`, then the spec and plan of the slice to
   work on.
2. **A slice with a plan but not executed:**

   > Execute `docs/superpowers/plans/<plan>.md` with superpowers
   > subagent-driven-development. Roadmap:
   > `docs/superpowers/specs/2026-09-15-usage-collector-spec-diff-roadmap.md`.

3. **The next slice without a spec:**

   > Let's brainstorm slice <X> of the usage-collector SPEC-DIFF work with
   > superpowers. Roadmap:
   > `docs/superpowers/specs/2026-09-15-usage-collector-spec-diff-roadmap.md`.

4. When a slice lands, update its row and add its hand-offs here.
