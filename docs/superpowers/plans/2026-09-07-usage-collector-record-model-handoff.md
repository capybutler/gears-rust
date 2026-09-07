# Handoff: usage-collector record model (slices 3-6)

Paste this whole file as the opening prompt of a new session, or say
"read `docs/superpowers/plans/2026-09-07-usage-collector-record-model-handoff.md`
and start on slice 3".

---

## What I want you to do

Implement the record-model half of the usage-collector rework: slices 3, 4, 5
and 6 of the plan named below. Slice 3 first.

Work in `/Users/binarycode/code/virtuozzo/gears-rust`, on the branch
`usage-collector/implementation-change` (or a branch from it — do not work on
`main`).

Use the superpowers skills. The sequence that worked for slices 1-2 was
`writing-plans` to expand one slice into detailed TDD tasks, then
`subagent-driven-development` to execute them with a two-stage review after
each task. Expand **one slice at a time**: writing detailed steps for slice 5
before slice 3 lands means guessing at a codebase that does not exist yet.

## Authoritative documents

Read these. They carry the target model:

- `gears/system/usage-collector/docs/DESIGN.md`
- `gears/system/usage-collector/docs/ADR/*.md` — 15 ADRs
- `gears/system/usage-collector/docs/PRD.md`
- `gears/system/usage-collector/docs/usage-collector-v1.yaml` — the REST contract
- `gears/system/usage-collector/docs/schemas/usage_record.v1.schema.json` — the
  GTS base type every meter derives from
- `gears/system/usage-collector/docs/schemas/example.stored_volume.v1.schema.json`

**Do NOT implement from these. They are stale:**

- `gears/system/usage-collector/docs/DECOMPOSITION.md`
- `gears/system/usage-collector/docs/features/*.md`

Both still describe the usage-type catalog, event deactivation and
compensation, which the current model deleted. A prior commit refreshed
DESIGN, the ADRs, the PRD, the OpenAPI file and the schemas, and left those
two sets behind. One paragraph of `usage-emission.md` was corrected
deliberately; the rest of that file is obsolete.

## Prior work

- Spec: `docs/superpowers/specs/2026-09-06-usage-collector-gateway-registry-owned-typing-design.md`
- Plan: `docs/superpowers/plans/2026-09-06-usage-collector-type-plane.md`

The plan's "Remaining slices" section outlines slices 3-6 at task level. Start
there and expand.

Slices 1-2 are complete: 27 commits, 485 tests passing, clippy clean across the
workspace. They moved usage-type ownership to the `types-registry` gear. A
meter is now a GTS type declaration whose `x-gts-traits` carry its aggregation
fold, canonical unit and closed metadata surface.

What exists now that you will build on:

| Thing | Where |
| --- | --- |
| `MeterTypeId` — a meter reference, wraps `gts::GtsTypeId` | `usage-collector-sdk/src/models.rs` |
| `AggregationFold` — `SUM`/`COUNT`/`MAX`/`MIN`/`LATEST` | `usage-collector-sdk/src/models.rs` |
| `DeclarationSource` port, plus `UnavailableDeclarationSource` | `usage-collector/src/domain/ports/declarations.rs` |
| `ResolvedDeclaration`, `CompiledMetadataSchema` | `usage-collector/src/domain/type_resolver/` |
| `TypeResolver` — TTL cache, single-flight, stale-on-error, fail-closed | `usage-collector/src/domain/type_resolver/mod.rs` |
| `TypesRegistryDeclarationSource` adapter | `usage-collector/src/infra/types_registry_source.rs` |
| `ServiceFixture` test builder | `usage-collector/src/domain/test_support.rs` |

`ServiceFixture` is the fixture shape: `.with_source(..)`, `.with_cap(..)`,
`.with_resolver(..)`, then a terminal `.build(..)`,
`.build_with_default_resolver_handle(..)` or `.build_with_metrics(..)`. Use it.
Do not reintroduce a family of `service_with_*_and_*` functions.

## What each slice does

### Slice 3 — time model

A record carries one instant, `created_at`. It must carry a period.

- `created_at` becomes `window_start` and `window_end`.
- Read paths select by the period end: `from <= window_end < to`. No path
  reads `window_start` to select. No path matches by overlap or containment.
- `TimeRange` becomes a typed parameter on both read paths, and stops being a
  `$filter` conjunct. `require_bounded_time_window` and
  `ValidationReason::MissingTimeWindow` come out with it.
- The page cursor keyset moves from `created_at` to `(window_end, id)`.
- Equal bounds mark a point event. That is valid input, not an error.
- Identity derivation moves to ADR-0007's 5-tuple: tenant, type, key,
  `window_start`, `window_end`. Read ADR-0007's "The canonical pre-image"
  section and follow it exactly: the fixed 27-character
  `YYYY-MM-DDTHH:MM:SS.ffffffZ` timestamp form, the `0x1F` separator, the
  namespace constant unchanged.
- Two preconditions are **validation errors, not truncation**: a bound with
  finer than microsecond precision is rejected, and a second value of `60` is
  rejected. Truncating would make a read-back entry derive a different id than
  it carries.

**The record `id` changes again in this slice.** Slice 2 already changed it
once, correcting the type reference to include its `~` terminator per ADR-0007.
Slice 3 replaces the `created_at` input with the two window bounds. Regenerate
the golden vectors in `usage-collector-sdk/src/id_tests.rs` and verify the new
values against the ADR by recomputing them independently — do not simply
accept whatever the code produces. The gear is pre-release with no
installations, so no migration is needed.

### Slice 4 — correction model

Corrections become appended invalidation entries. Nothing changes in place.

Remove: `status`, `UsageRecordStatus`, `corrects_id`,
`deactivate_usage_record`, and the compensation `ConflictReason` variants
(`AlreadyInactive`, `CorrectsIdTargetsCompensation`, `CorrectsIdWrongScope`,
`CorrectsIdInactive`).

Add: `invalidates`, `reason_code`, and `entry_type` **derived** from
`invalidates.is_some()` — never a stored field and never submitted, so a
marker cannot disagree with the payload it marks. Add `AlreadyInvalidated` to
`ConflictReason`.

The rules the ingestion gateway enforces:

- `invalidates` and `reason_code` appear together or not at all.
- The target resolves, is itself an ordinary record, and is not already
  invalidated.
- **Faithful copy**: every caller-supplied field equals the target's, apart
  from the three permitted departures — the entry's own idempotency key,
  `invalidates`, and `reason_code`. For `subject_ref`, presence against
  absence is a mismatch. A rejection names the field that differs.
- The quantity is echoed, never negated.
- At most one invalidation per record. The plugin enforces this atomically;
  the gateway pre-checks.

Withdrawal takes effect in the fold: the aggregate path excludes both entries
of a withdrawn pair. Ledger reads return both, as persisted.

### Slice 5 — origin and backfill

- `RecordOrigin` (`live` / `backfill`), stamped by the gateway from the path
  the entry arrived on, never caller-supplied.
- `backfill_usage_records` on the SDK trait, and `POST /records/backfill`.
- The live path bounds the covered period on both sides: a future tolerance
  (5 minutes by default) and a past tolerance (48 hours by default). The
  past-tolerance rejection must name the backfill route in its message.
- The backfill path has its own window (90 days by default) and requires
  elevated authorization beyond it.
- Both bounds apply to every entry the live path admits, an invalidation
  included, over the period it copies.

### Slice 6 — errors and the contract gate

- Final reason-vocabulary pass. `UsageCollectorPluginError` narrowed to the six
  variants of DESIGN §3.3.
- Re-enable the six `#[ignore]`d drift tests in
  `usage-collector/src/api/rest/routes/openapi_contract_tests.rs`, against an
  explicit implemented-operation set, with `/feed` and `/reconciliation` named
  in a not-yet-implemented list.
- Scaffold the DESIGN §3.3 plugin contract suite in `usage-collector-sdk`
  against the noop plugin, without `feed-snapshot-and-replay`.
- **Include a scope-enforcement case.** No test anywhere currently exercises a
  plugin filtering out a real stored row that fails a non-trivial compiled PDP
  scope; every test double either ignores the `scope` argument or is not-found
  by construction. This matters because slice 2 retired the point lookup's
  in-process per-record attribution check, per DESIGN §3.2 and §3.3, so the
  "exists but not yours reads as `NotFound`" guarantee now rests entirely on
  each plugin intersecting the filter it is handed. One case closes the gap for
  all three read paths.

## Out of scope

Do not build these:

- **The usage feed** — `read_usage_feed`, `read_feed_page`, `FeedPage`,
  `FeedSubscription`, `FeedKeyset`, the watermark, `GET /feed`.
- **Reconciliation** — `get_reconciliation_metadata`, `GET /reconciliation`.
- **The ADR-0015 declaration mirror.** It needs a durable table and the gear
  owns no database. The ADR calls it temporary, and the types-registry rewrite
  already implements persistent storage on its internal v2 routes, which
  retires the mirror's premise.
- **Ingestion quotas** (`fr-rate-limiting`). Not implemented before this work
  either.
- **The TimescaleDB plugin.** It is unwired from the build — not a workspace
  member, invisible to `cargo check --workspace`. Its production source still
  imports types that slice 2 deleted, so it will not compile the moment anyone
  rewires it. Leave every file in
  `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/`
  untouched. Porting it is separate work.
- **Refreshing `DECOMPOSITION.md` and `docs/features/*.md`.**

## Traps found the hard way in slices 1-2

Every one of these cost a review cycle. Check them rather than rediscovering
them.

**API facts that plan sketches got wrong:**

- `gts::GtsTypeId` has no `FromStr`. Use `GtsTypeId::try_new(&str)`.
- `DomainError::internal(...)` does not exist. The bare `Internal(String)`
  variant does. (`UsageCollectorError::internal(...)` is a different type and
  does exist.)
- `toolkit_canonical_errors::CanonicalError` has no `is_not_found()`. Match the
  `CanonicalError::NotFound { .. }` variant. Eight other workspace gears do it
  that way.
- `toml` is not a dependency of the gear. Config tests use
  `serde_json::from_str`.
- The orphan rule forbids `TryFrom<LocalRequest> for Vec<ForeignType>`. Use an
  inherent method.
- `types_registry_sdk::GtsTypeSchema::try_new` enforces strict parent-chain
  rules. A derived id must carry its parent; a root id must not.

**The `x-gts-traits` placement trap.** `GtsTypeSchema::extract_traits` reads
`x-gts-traits` only from the **top level** of a schema. The `gts` crate's own
validator recurses into `allOf`. So a document with traits nested in `allOf`
registers successfully and then reads back carrying no traits at all. Put
traits at the top level in every test fixture. All nine other derived GTS
schemas in this repo do.

**`GtsTypeSchema::effective_properties` resolves a key by override, not
intersection.** Its doc says "this schema wins on key collisions; parent fills
in inherited keys". The base type always declares an open `metadata`, so a
meter that supplies no closing override inherits *open*. This is why metadata
closure is enforced in code rather than delegated to the subschema's
`additionalProperties: false`. Do not "simplify" that back.

**Test-double signals.** `HappyPathPlugin::calls()` and `last_fold()` count
only `query_aggregated_usage_records` dispatches. For the create path use
`last_create_record_input()`. Asserting `calls() == 1` in an ingestion test
fails permanently.

**`ODataQuery::default()` has no bounded time window**, so it trips
`require_bounded_time_window` before reaching whatever you meant to test.
Note slice 3 removes that check — once the time range is a typed parameter this
trap disappears.

**Timing tests.** `tokio::time::pause()` and `#[tokio::test(start_paused =
true)]` virtualize `tokio::time::Instant` only, **not**
`std::time::Instant`. A TTL stored as `std::time::Instant` never expires under
a paused clock. Also: back-to-back operations under a paused clock receive
**identical** `Instant` values, so any oldest-wins tiebreak is undefined —
insert `tokio::time::advance(..)` to force an ordering.

**Falsification and stale builds.** Restoring a file with `mv` preserves its
mtime, so cargo skips the rebuild and your falsification appears to pass. Use
`cp`, then `touch` the file, and confirm a `Compiling` line before trusting the
result. This produced one false pass in slice 2.

## The process that worked

**Distrust the plan's code sketches.** Three of the defects found in slices 1-2
originated in the plan rather than the implementation, one of them a silently
open "closed" metadata surface. Tell each implementer explicitly to verify
sketches against the real APIs and to push back rather than making something
compile.

**Require a falsification step per task.** Before reporting done: deliberately
weaken the branch the task exists to protect, confirm the corresponding test
fails, restore, force a genuine rebuild, re-verify. Report it honestly. This
caught two tests that passed against broken code, and one implementer
correctly reported that a test did *not* pin its half of an invariant.

**Two-stage review, spec compliance first.** Never start the quality review
before spec compliance passes.

**Demand per-test verdicts on deletions and repointings.** These slices delete
a lot — `status`, `corrects_id`, `deactivate_usage_record` and everything built
on them. "Deleted the failing test" and "deleted the test whose question no
longer exists" look identical in a diff. Have the reviewer read each deleted
test in the parent commit and rule on it individually. In slice 2 this
validated 106 deletions and 35 repointings and found no lost coverage — but
only because it was checked.

**Watch for comments that a reader can falsify by grep.** Three separate
reviews caught one: a doc naming a cache renamed twelve lines above, a clippy
justification citing sibling functions that do not exist, a traceability note
pointing at a marker that had moved files. Renaming across a large diff
produces these reliably.

**Do not commit to the branch while an implementer subagent is working.** An
amend will land on the wrong commit. This happened once.

## Verification bar

```
cargo check --workspace --all-targets
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk -p cf-gears-noop-usage-collector-plugin
cargo clippy --workspace --all-targets --all-features
cargo +nightly fmt
```

Clippy is deny-warnings in CI. Tests live in a sibling `*_tests.rs` file with a
`#[cfg(test)] #[cfg_attr(coverage_nightly, coverage(off))] #[path = "..."]`
hook, never an inline `mod tests`. Commits are Conventional Commits with a
`Signed-off-by` trailer; a breaking change takes a `!` and a
`BREAKING CHANGE:` trailer.

Current baseline: **485 tests passing, 6 skipped.** The 6 are the `#[ignore]`d
OpenAPI drift tests that slice 6 re-enables.

## One open question that is not code

The prior DESIGN rework dropped whole categories of traceability identifier —
`flow`, `algo`, `dod`, `state`, `component`. `DESIGN.md` and `PRD.md` now
carry only `adr`, `constraint`, `contract`, `fr`, `nfr` and `principle` ids.
The gear's source is dense with `@cpt-flow:`, `@cpt-algo:`, `@cpt-dod:` and
`@cpt-state:` markers that name ids in the dropped categories, so those markers
point at nothing.

That is pre-existing and out of scope, but it means a "does every marker
resolve" gate would fail across the whole crate today. It needs a decision:
regenerate those id categories in the docs, or strip the marker classes the
rework retired. Ask before doing either.
