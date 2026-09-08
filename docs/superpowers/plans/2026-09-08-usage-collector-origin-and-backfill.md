# Usage Collector — Origin and Backfill (slice 5) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An accepted entry records which ingestion path admitted it, and a
dedicated backfill route exists for the covered periods the live path refuses.

**Architecture:** `RecordOrigin` (`live` / `backfill`) becomes a server-assigned
field on `UsageRecord`, stamped by the projection that already derives `id`. The
live path gains a two-sided bound on the covered period's end — a future
tolerance and a past tolerance whose rejection names the backfill route. The
backfill route keeps the future tolerance, drops the past tolerance, and uses its
own configured window to decide which PDP action each entry is authorized
against. Three config keys carry the three durations.

**Tech Stack:** Rust 2024, `time`, `serde`, `async_trait`, ToolKit
(`OperationBuilder`, `PolicyEnforcer`, `toolkit-gts`), OpenTelemetry metrics,
`cargo nextest`.

---

## Ground rules

These are carried forward from slice 4's plan (`docs/superpowers/plans/2026-09-08-usage-collector-correction-model.md`),
where they were paid for. They are current.

1. **Distrust every code sketch in this plan.** The snippets below were written
   against the tree at `d0315e328` and are a starting point, not a spec. Read the
   real file before editing it. The one exception is the *Verified API facts*
   table — every count in it was produced by a `grep` in this session, and the
   command is given so you can re-run it.
2. **That exemption has failed before.** In slice 4 a facts-table row miscounted
   a function's call sites, an implementer correctly trusted it, and the wrong
   number reached shipped code. **Any claim in this document stating a count
   deserves one `grep -c` before you rely on it.**
3. **Distrust prescribed falsifications as hard as prescribed code.** Slice 4
   prescribed two mutations that could not fail. If a mutation you were told to
   run survives, first ask whether it is an equivalent mutant, and say so —
   never manufacture a kill, and never delete a working test to make a number
   look better.
4. **Tests live in a sibling `*_tests.rs` file** with a
   `#[cfg(test)] #[cfg_attr(coverage_nightly, coverage(off))] #[path = "..."]`
   hook. Never an inline `mod tests`. `src/gts/permissions.rs` has a
   pre-existing inline one — do not copy it and do not restructure it.
5. **Cite ADRs by `cpt-cf-usage-collector-adr-*` id, never by number.** Four
   surviving `ADR-0012` citations in `lib.rs`, `config.rs` and `config_tests.rs`
   now point at *backfill-isolation* — this slice's ADR — while claiming the
   usage-type catalog is plugin-owned. Do not add a fifth. Task 14 addresses the
   existing four.
6. **Do not run a workspace-wide test build.** `target/` reaches ~110 GB and
   fills the disk. Scope every run to the three usage-collector packages.
7. **One worker mutation-tests the tree at a time.** Two concurrent reviewers
   contaminated each other's runs in slice 4.
8. **Do not commit while an implementer subagent is working** — an amend lands
   on the wrong commit.
9. **Demand per-test verdicts on deletions and repointings.** "Deleted the
   failing test" and "deleted the test whose question no longer exists" look
   identical in a diff.
10. **Enumerate input shapes, not just mutations.** Mutation coverage inherits
    the blind spots of input coverage.
11. Commits are Conventional Commits with a `Signed-off-by` trailer. A breaking
    change takes a `!` and a `BREAKING CHANGE:` trailer. No attribution lines.

### Five ways a falsification lies to you

All five produce a "mutation survived" result, which is the most misleading
output available — it argues for deleting a test that works.

1. **Restoring with `mv` preserves mtime**, so cargo skips the rebuild. Use
   `cp`, then `touch`, and confirm a `Compiling cf-gears-…` line.
2. **A relative path in a mutation script edits nothing** after a
   working-directory reset — and the `Compiling` line still appears, because the
   preceding `touch` already invalidated the artifact. Use absolute paths, and
   **grep the mutated line to confirm the mutation is in the file** before
   believing any pass.
3. **A count taken under a nextest `-E` filter is a lower bound.** One slice-4
   mutation measured 13 failures filtered and 21 unfiltered. Use
   `--no-fail-fast`, unfiltered, for any number you report.
4. **`git checkout` restores from HEAD**, silently discarding uncommitted work on
   the function under test. **Restore from a `cp` snapshot, never from git.**
5. **Someone else's restore script.** Put scratch in the session scratchpad, not
   the repository. Scope every restore to the files your own patch touched.

---

## The normative model this slice implements

Read this table before writing any code. It is the whole slice, and three of its
five cells are easy to get backwards.

| | future tolerance (5 min) | past tolerance (48 h) | backfill window (90 d) |
| --- | --- | --- | --- |
| **live path** | **rejects** | **rejects**, and the message names the backfill route | not read |
| **backfill path** | **rejects** — still applies | **does not apply** | selects the PDP action: inside → `create`, beyond → `backfill` |

Sources, quoted rather than paraphrased:

`usage-collector-v1.yaml`, `POST /usage-collector/v1/records/backfill` (~line 242) — the
tightest statement anywhere, and the authority for the *future* row:

> Identical validation and request shape to `POST /records`, differing only in
> four respects: the workload is isolated from live ingestion so it cannot
> breach live-path SLOs, every accepted entry is stamped `origin: backfill`, the
> live path's past bound on the covered period (default 48 hours) does not apply
> because this route exists for exactly the periods that bound rejects, and
> submissions whose covered period ends further back than the configured
> backfill window (default 90 days) require elevated authorization.

Four respects. The future tolerance is **not** one of them, so it governs both
routes. Only the *past* bound is lifted.

`DESIGN.md` §1.2, `cpt-cf-usage-collector-fr-live-future-time-bound` (~line 65),
verbatim:

> The live path bounds the covered period on both sides. It rejects a period
> that ends further into the future than a tolerance, 5 minutes by default. It
> also rejects one that ends further into the past than a second tolerance, 48
> hours by default and configurable. Anything older must use the dedicated
> backfill route, which the rejection names. Both bounds govern every entry the
> path admits, an invalidation included, over the period it copies. The backfill
> path carries its own window. All of these are configuration, enforced in the
> Ingestion Gateway before dispatch.

`cpt-cf-usage-collector-adr-backfill-isolation`, Decision Outcome:

> **The covered-period bounds are a property of the path, not of the entry
> kind.** An invalidation copies its target's period, and that period is checked
> exactly as a measurement's own is. A withdrawal on the live path reaches 48
> hours back, and one reaching further travels the backfill route for its 90
> days.

Every bound reads **`window_end`** — the end of the covered period — and nothing
else. Not `window_start`, not the arrival instant, not the entry kind.

---

## Decisions taken before implementation

The handoff left five things open. Four are settled here and one was settled by
the spec owner. **Do not re-open any of them in code.** If you believe one is
wrong, say so in your report and implement it as written anyway.

### D1 — The projection learns the origin through a parameter

`CreateUsageRecord::try_into_usage_record` becomes
`try_into_usage_record(self, origin: RecordOrigin)`.

**Property traded away:** the projection is no longer a pure projection of
*caller-supplied* fields. It is still a pure total function; it now takes one
server-assigned input alongside the submission.

**Property kept:** `UsageRecord` is still *constructed* in exactly two places
(`try_into_usage_record` and `TryFrom<UsageRecordWire>`), so there is one site
that decides an entry's origin rather than a construct-then-stamp pair whose
halves can drift apart or be reordered.

**Corrected during execution — this decision originally claimed more than it
could deliver.** It said "there is still no struct-update path and no
`&mut UsageRecord` anywhere. That is the append-only invariant holding at the
type level." Both halves are false, and Task 3's code-quality review falsified
them:

- `UsageRecord` has 13 `pub` fields, derives `Clone`, and is not
  `#[non_exhaustive]`, so `UsageRecord { origin: …, ..other }` compiles from
  anywhere. The workspace already does it at
  `usage-collector/src/domain/test_support.rs:1592` and
  `usage-collector/src/domain/authz_tests.rs:70`, and mutates a field directly
  at `usage-collector-sdk/src/models_tests.rs:586`.
- `cpt-cf-usage-collector-adr-append-only-invalidation` is a *ledger* property —
  withdrawals are appended entries and a plugin returns a withdrawn pair as
  persisted. Immutability of an in-memory Rust value is a different claim, and
  borrowing the ADR's name for it misleads.

The claim came from the slice-5 handoff and was carried in here unverified, then
written verbatim into a shipped doc comment. It is the third handoff claim to
fall, after the marker categories and the config-key count. **Ground rule 2 says
any claim stating a count deserves a `grep -c`; extend that to any claim stating
a structural guarantee.** Passing the origin in is still the right call — but for
the weaker, true reason above, not for a type-level guarantee that does not
exist.

`origin` is **not** an input to the `id` derivation. The 5-tuple is unchanged:
`(tenant_id, gts_type_id, idempotency_key, window_start, window_end)`. An entry
imported on the backfill route and the same entry emitted live derive the *same*
`id` — which is what makes a re-import deduplicate rather than duplicate.

### D2 — The bounds read the copied period, not the arrival

Because an invalidation is a faithful copy, its `window_end` **is** its target's
`window_end`. So the bound needs no invalidation case at all: checking the
projected entry's `window_end` covers both kinds identically. This is not a
simplification of the rule, it is the rule (`fr-live-future-time-bound`: "Both
bounds govern every entry the path admits, an invalidation included, over the
period it copies").

**The consequence is load-bearing and counter-intuitive:** withdrawing a record
whose period closed more than 48 hours ago is **rejected on the live path** and
belongs on backfill. `origin = backfill` on a correction is therefore the normal
case, not the odd one. Task 9 pins this with a test; no test in the tree pins it
today.

### D3 — Elevated authorization is one new action, gating beyond-window only

`authz::usage_record::actions` gains exactly one constant, `BACKFILL`.

| route | `now - window_end` | action authorized |
| --- | --- | --- |
| live | (any admitted) | `create` |
| backfill | `<= backfill_window` | `create` |
| backfill | `> backfill_window` | `backfill` |

This is the spec's wording read literally: "The backfill path carries its own
action **so that** a submission beyond the configured backfill window can require
elevated authorization" (spec §3.12). The action exists *for* the beyond-window
case; inside the window a backfill entry needs no grant a live entry does not.

It also means **one batch can carry both actions**, and that is safe by
construction rather than by care: `AttributionTupleKey` already includes `action`
in its hash/eq, and its own doc says why — "a future caller that mixes actions in
one batch cannot collapse onto a single PDP decision"
(`usage-collector/src/domain/authz.rs:149-153`). This slice is that future
caller. Task 11 adds the first test that exercises it.

`PdpOp` gains `Backfill` (`operation` label value `"backfill"`), per DESIGN
§3.11.5's operation vocabulary.

### D4 — Workload isolation is deferred; there are three new config keys, not four

The handoff says "**Four config keys**". The spec's §3.10 table lists six, and
three of them (`metadata_size_cap`, `type_cache_ttl`, `type_cache_capacity`)
already exist on `UsageCollectorConfig`. **Three are new**:
`live_future_tolerance`, `live_past_tolerance`, `backfill_window`. There is no
fourth. This is exactly the wrong-cardinality residue the handoff warns about,
found in the handoff itself; Task 14 records it.

Gear-level workload isolation (`cpt-cf-usage-collector-nfr-workload-isolation`)
is **not implemented in this slice**. The ADR calls it a gear-level obligation
and its confirmation is a concurrent load test, which is out of scope here. Task
11 leaves a `TODO` naming the NFR at the backfill entry point, and Task 14
records the unmet obligation. Do not invent a semaphore, a thread pool, or a
concurrency knob.

### D5 — Only the compensation-named markers are stripped

The handoff asserts the `@cpt-flow:` / `@cpt-algo:` / `@cpt-dod:` /
`@cpt-state:` marker ids name categories "the DESIGN rework dropped" and that a
"does every marker resolve" gate would fail across the crate.

**That was verified and is false.** Those ids resolve — in
`gears/system/usage-collector/docs/features/*.md` and `DECOMPOSITION.md`, which
is where this convention's ids live (`cfs validate --artifact` checks code
against a FEATURE, not against DESIGN). Every id sampled resolves there and
appears in `DESIGN.md` zero times. The handoff also listed `component` among the
dropped categories; `component` ids still exist in DESIGN (25 references) and are
not a marker category in this code at all.

So: **do not strip markers wholesale.** Task 1 removes only the 36 marker lines
naming the two ids for the correction model slice 4 deleted —
`cpt-cf-usage-collector-flow-usage-emission-compensation` and
`cpt-cf-usage-collector-dod-usage-emission-compensation-flow` — including the
region that currently wraps `entry_type_of` under an id naming compensation and a
record kind. The other 467 marker lines resolve and stay untouched.

**New code in this slice gets `@cpt-*` markers only where an existing, resolving
id genuinely covers it.** Invent no new ids.

### D6 — `origin` joins both the filterable schema and the keyset-safe set

DESIGN §3.1 puts `origin` in `UsageRecordFilterField`. That is not in question.

The second half is: `KEYSET_SAFE_RECORD_FIELDS` currently holds six names, and
the doc comment beside it states the admission rule — a keyset key "has to be an
attribute this SDK guarantees is present on every entry", excluding
domain-optional attributes (`subject_id`, `subject_type`, `invalidates`) and
attributes derived from an optional one (`entry_type`). `origin` is mandatory,
stored, non-null on every entry, and every plugin must persist it because it is a
field of `UsageRecord`. It satisfies the stated rule exactly.

Leaving it out would make it the only mandatory, stored, filterable field
excluded, and would need an exception the doc does not have. **It goes in**, and
the doc's "six names is short enough to be actionable" becomes seven. That
sentence is a count, and counts are this branch's most persistent residue — Task
6 changes it in the same edit.

---

## Verified API facts

Every number here came from a command run in this session against `d0315e328`.
Re-run any you are about to depend on. `$PKGS` below means
`usage-collector/src usage-collector-sdk/src plugins/noop-usage-collector-plugin/src`,
relative to `gears/system/usage-collector`.

| Fact | Value | Command |
| --- | --- | --- |
| Baseline test result | **648 passed, 6 skipped** | `cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk -p cf-gears-noop-usage-collector-plugin --no-fail-fast` |
| `try_into_usage_record` mentions | **47** across 10 files (29 of them in `usage-collector-sdk/src/models_tests.rs`; 1 in `handlers/usage_records.rs` is prose in a doc comment, not a call) | `grep -rn --include='*.rs' -F try_into_usage_record $PKGS` |
| `Service::new_with_metrics` mentions | **15** across 9 files (4 in `service.rs`, 2 in `test_support.rs`, 2 in `service_tests.rs`, 2 in `infra/types_registry_source.rs`, 1 each in `config.rs`, `ports/declarations.rs`, `service_metrics_tests.rs`, `validation.rs`, `module.rs`; several are doc-comment prose) | `grep -rn --include='*.rs' -F 'Service::new_with_metrics' $PKGS` |
| `impl UsageCollectorMetrics` blocks | **3** — `infra/metrics.rs:212` (`UcMetricsMeter`), `domain/ports/metrics.rs:633` (`NoopMetrics`), `domain/type_resolver/resolver_tests.rs:43` (`RecordingMetrics`) | `grep -rn --include='*.rs' -F 'impl UsageCollectorMetrics' $PKGS` |
| `record_ingestion_record` sites | **8** — 3 definitions + 1 noop stub + 2 calls in `service.rs` (`:1066`, `:1136`) + 2 calls in `infra/metrics_tests.rs` | `grep -rn --include='*.rs' -F record_ingestion_record usage-collector/src` |
| `observe_ingestion_duration` sites | **7** | `grep -rn --include='*.rs' -F observe_ingestion_duration $PKGS` |
| `actions::CREATE` mentions | **17** | `grep -rn --include='*.rs' -F 'actions::CREATE' $PKGS` |
| `@cpt-*` marker lines, three packages | **503** — every one names a `flow` / `algo` / `dod` / `state` id | `grep -rn --include='*.rs' '@cpt-' $PKGS` |
| …of those, naming a compensation id | **36**, in `domain/service.rs` and `infra/sdk_error_mapping.rs` only | see Task 1 |
| `@cpt-*` lines in the TimescaleDB plugin | **8** — **out of scope, must not be touched** | `grep -rn --include='*.rs' '@cpt-' plugins/timescaledb-usage-collector-plugin/src` |
| `authz::usage_record::actions` constants | **3** — `CREATE`, `GET`, `LIST` (`domain/authz.rs:231-235`) | read the file |
| `gts_instance!` permission blocks | **3**, and `EXPECTED_PERMISSION_IDS` has **3** entries (`gts/permissions.rs`) | read the file |
| `PdpOp` variants | **4** — `Ingest`, `QueryRaw`, `QueryAggregated`, `GetRecord` (`domain/ports/metrics.rs:56-65`) | read the file |
| `key::` label constants | **8** (`domain/ports/metrics.rs:29-46`) | read the file |
| `UsageRecord` struct-literal sites | **~48**, nearly all in `*_tests.rs`; also `models.rs` (2 real constructions + destructures), `test_support.rs`, `domain/invalidation.rs` | `grep -rn --include='*.rs' 'UsageRecord {' $PKGS` |
| Host fixtures using a **1970** covered period | **23** in `service_tests.rs`, **28** in `handlers/usage_records_tests.rs`, **6** in `authz_tests.rs`, **4** in `service_metrics_tests.rs`, **2** each in `test_support.rs` and `invalidation_tests.rs` | `grep -c UNIX_EPOCH <file>` |
| Host tests driving the ingestion path | **44** in `service_tests.rs`, **16** in `service_metrics_tests.rs`, **15** in `handlers/usage_records_tests.rs` | `grep -c '\.create_usage_record\|handle_create_usage_records' <file>` |

**The last two rows are the biggest landmine in this slice.** Every
ingestion-path fixture submits a covered period ending in 1970, so the moment the
48-hour past bound is enforced they are all rejected at once. Task 9 re-bases
them in its own commit before enforcing anything. Read that task's opening
before you start Task 9 — meeting ~75 simultaneous failures unprepared is what
would tempt someone into widening a published default to make the suite green.

**Two more facts worth internalising before Task 3.**

*The wire encoding is held by four hand-written shadow structs*, under the
`// Wire codecs` banner at `usage-collector-sdk/src/models.rs:1080`:
`UsageRecordWire` / `UsageRecordWireRef` and the `CreateUsageRecord` pair.
`#[serde(flatten)]` does not compose with `deny_unknown_fields`, which is why
they exist. A field added to either domain type is a compile error in all the
relevant shadows — that is deliberate, and it is the guard that makes adding
`origin` safe. **A round-trip test proves the two halves of a codec agree with
each other, never that either agrees with the contract**, so Task 4 adds literal
key-set assertions, not just round-trips.

*A serde attribute can only ever be live in one direction on a DTO.*
`libs/toolkit-macros/src/api_dto.rs:53` — `api_dto(request)` derives
`Deserialize` only, `api_dto(response)` derives `Serialize` only. So
`skip_serializing_if` on a request struct is inert and `#[serde(default)]` on a
response struct is inert. Slice 4 wasted a mutation discovering this.

---

## Compilation-unit warning — read before sequencing work

`cf-gears-usage-collector` is one compilation unit: its test binary builds
nothing while any compile error remains anywhere in the crate. In slice 4 that
meant **no host test ran in place for six of eight tasks**, and defects sat
invisible the whole time.

In this plan:

- **Tasks 1 and 2 leave every package compiling.** Full verification bar applies.
- **Task 3 breaks the host crate deliberately** (it adds a field to `UsageRecord`
  and a parameter to `try_into_usage_record`). At the end of Task 3 the SDK
  package compiles and its own tests pass; `cf-gears-usage-collector` does not
  compile. That is expected and is the compile-error guard doing its job.
- **Task 4 owes the first in-place host test run.** It is not finished until
  `cargo nextest run -p cf-gears-usage-collector` reports the full baseline
  again. Do not start Task 5 before that happens.
- Tasks 5 onward each leave every package compiling and every test passing.

---

## File structure

| File | Change | Task |
| --- | --- | --- |
| `usage-collector/src/domain/service.rs` | strip 34 compensation marker lines | 1 |
| `usage-collector/src/infra/sdk_error_mapping.rs` | strip 2 compensation marker lines | 1 |
| `usage-collector-sdk/src/models.rs` | `RecordOrigin`; `origin` on `UsageRecord`; both `UsageRecord` shadows; `try_into_usage_record` parameter; `origin` on `UsageRecordQuery` + `KEYSET_SAFE_RECORD_FIELDS` | 2, 3, 6 |
| `usage-collector-sdk/src/models_tests.rs` | 29 call-site updates; new origin tests | 2, 3, 6 |
| `usage-collector-sdk/src/lib.rs` | re-export `RecordOrigin`, `BACKFILL_ROUTE_PATH` | 2, 7 |
| `usage-collector-sdk/src/reason.rs` (+ `_tests`) | `FUTURE_WINDOW`, `PAST_WINDOW` codes and variants | 7 |
| `usage-collector-sdk/src/error.rs` (+ tests in `models_tests.rs`) | two constructors naming the backfill route | 7 |
| `usage-collector-sdk/src/api.rs` | `backfill_usage_records` on the trait | 12 |
| **`usage-collector/src/domain/covered_period.rs`** (new) | `CoveredPeriodBounds` + the two free functions | 9 |
| **`usage-collector/src/domain/covered_period_tests.rs`** (new) | their unit tests | 9 |
| `usage-collector/src/domain/mod.rs` | declare the new module | 9 |
| `usage-collector/src/config.rs` (+ `_tests`) | three keys, defaults, validation, `covered_period_bounds()` | 8 |
| `usage-collector/src/domain/service.rs` | `origin` parameter through both inner paths; bound enforcement; action selection; `backfill_usage_records` | 4, 9, 11 |
| `usage-collector/src/domain/authz.rs` (+ `_tests`) | `actions::BACKFILL` | 10 |
| `usage-collector/src/gts/permissions.rs` | 4th `gts_instance!` + `EXPECTED_PERMISSION_IDS` entry | 10 |
| `usage-collector/src/domain/ports/metrics.rs` | `PdpOp::Backfill`; `key::ORIGIN`; two trait signatures; `NoopMetrics` | 5, 10 |
| `usage-collector/src/infra/metrics.rs` (+ `_tests`) | emit the `origin` label on two instruments | 5 |
| `usage-collector/src/domain/type_resolver/resolver_tests.rs` | `RecordingMetrics` signature update | 5 |
| `usage-collector/src/domain/local_client.rs` (+ `_tests`) | `backfill_usage_records` | 12 |
| `usage-collector/src/api/rest/dto.rs` (+ `_tests`) | `origin` on `UsageRecordDto`; key-set pins | 4 |
| `usage-collector/src/api/rest/handlers/usage_records.rs` (+ `_tests`) | backfill handler | 13 |
| `usage-collector/src/api/rest/routes/usage_records.rs` (+ `_tests`) | backfill route registration | 13 |
| `usage-collector/src/module.rs` | thread the three config values | 8 |
| `usage-collector/src/domain/test_support.rs` | fixtures gain `origin` | 4 |
| `DIVERGENCES.md` | entry 9(c) struck; entry 10 narrowed; new entries | 14 |

**Out of scope, and `git diff --stat` must show zero files from any of them:**
`plugins/timescaledb-usage-collector-plugin/**` (not a workspace member, still on
the pre-slice-2 model), `usage-collector-v1.yaml`, `DESIGN.md`,
`DECOMPOSITION.md`, `docs/features/*`, `docs/api/api.json`, any crate version
bump, the six `#[ignore]`d OpenAPI drift tests (slice 6 re-enables them; leave
them skipped), and everything in slice 6's list.

---

## Task 1: Strip the markers naming the retired correction model

Slice 4 deleted event deactivation and compensation. Two traceability ids
describing that model still wrap live code — including a region around
`entry_type_of`, whose id names compensation and a record kind, and which
describes neither. `service.rs` already says so at the site.

**Files:**
- Modify: `gears/system/usage-collector/usage-collector/src/domain/service.rs`
- Modify: `gears/system/usage-collector/usage-collector/src/infra/sdk_error_mapping.rs`

- [ ] **Step 1: Confirm the blast radius before deleting anything**

Run, from `gears/system/usage-collector`:

```bash
grep -rn --include='*.rs' -e 'usage-emission-compensation' \
  usage-collector/src usage-collector-sdk/src plugins/noop-usage-collector-plugin/src \
  | grep '@cpt-' | wc -l
grep -rln --include='*.rs' -e 'usage-emission-compensation' \
  usage-collector/src usage-collector-sdk/src plugins/noop-usage-collector-plugin/src
```

Expected: `36`, and exactly two files —
`usage-collector/src/domain/service.rs` and
`usage-collector/src/infra/sdk_error_mapping.rs`.

If either number differs, **stop and report** rather than adapting. The two ids
in play are `cpt-cf-usage-collector-flow-usage-emission-compensation` and
`cpt-cf-usage-collector-dod-usage-emission-compensation-flow`, and no other
marker in the tree may be touched by this task.

- [ ] **Step 2: Delete only those marker lines**

Delete every whole line that is a `@cpt-flow:`, `@cpt-dod:`, `@cpt-begin:` or
`@cpt-end:` marker naming one of those two ids. Do not delete a line that merely
*mentions* compensation in prose — several doc comments explain why the
compensation model is gone, and those sentences are the record of the decision.
Do not touch any marker naming any other id, and do not reflow the surrounding
comments.

The `entry_type_of` region at `service.rs:268-276` loses its
`@cpt-begin` / `@cpt-end` pair. **Delete** the stale explanatory paragraph at
that site — the one saying the marker names compensation and a record kind —
rather than replacing it. It existed only to explain that the marker id was
misleading, so with the marker gone it has no referent, and the comment's own
first paragraph ("`invalidation` iff it names the entry it withdraws, else
`record`") already states the function's behaviour. Do not write a replacement
sentence: restating the three lines of code below it is a style this crate uses
nowhere else, and the comment is complete at two paragraphs.

*(This instruction originally said "replace … with a plain statement of what the
function does". That was wrong — it assumed a gap the first paragraph already
filled — and the code-quality review caught the redundant comment it produced.
Corrected here rather than left standing beside the fix.)*

- [ ] **Step 3: Verify nothing but comments changed**

```bash
git diff --stat
git diff -U0 | grep '^[-+]' | grep -v '^[-+][-+]' | grep -vc '^\s*[-+]\s*//'
```

Expected: two files changed, ~36 deletions, and the second command prints `0` —
every changed line is a comment. If it prints anything else, you deleted code.

- [ ] **Step 4: Run the full bar**

```bash
cargo check --workspace --all-targets
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk \
  -p cf-gears-noop-usage-collector-plugin --no-fail-fast
cargo clippy --workspace --all-targets --all-features
cargo +nightly fmt
cargo doc --no-deps -p cf-gears-usage-collector-sdk -p cf-gears-usage-collector
```

Expected: `648 passed, 6 skipped`. `cargo doc` emits **35** pre-existing host
warnings; confirm the count did not change.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -s -m "refactor(usage-collector): drop the traceability markers naming the retired compensation flow

Slice 4 replaced compensation with append-only invalidation. Two ids kept
describing it from live code, one of them wrapping entry_type_of under a
coordinate naming compensation and a record kind — neither of which the
function has anything to do with.

Only these two ids are removed. The other 467 marker lines in the crate
resolve against docs/features/*.md and DECOMPOSITION.md and are untouched."
```

---

## Task 2: `RecordOrigin` in the SDK

The closed marker itself, with nothing consuming it yet. It mirrors `EntryType`
deliberately: both are closed, lowercase on the wire, and both are reused as
bounded metric labels, so they should be spelled the same way.

**Files:**
- Modify: `gears/system/usage-collector/usage-collector-sdk/src/models.rs`
- Modify: `gears/system/usage-collector/usage-collector-sdk/src/lib.rs`
- Test: `gears/system/usage-collector/usage-collector-sdk/src/models_tests.rs`

- [ ] **Step 1: Write the failing tests**

Append to `models_tests.rs`:

```rust
#[test]
fn record_origin_wire_spellings_are_the_two_the_contract_declares() {
    // `RecordOrigin` in usage-collector-v1.yaml is `enum: [live, backfill]`.
    // These strings are a wire contract and a bounded metric-label
    // vocabulary at once, so they are asserted against literals rather
    // than against the enum.
    assert_eq!(RecordOrigin::Live.as_str(), "live");
    assert_eq!(RecordOrigin::Backfill.as_str(), "backfill");
}

#[test]
fn record_origin_serialises_to_its_wire_spelling() {
    assert_eq!(
        serde_json::to_value(RecordOrigin::Live).expect("serializes"),
        serde_json::json!("live"),
    );
    assert_eq!(
        serde_json::to_value(RecordOrigin::Backfill).expect("serializes"),
        serde_json::json!("backfill"),
    );
}

#[test]
fn record_origin_deserialises_from_its_wire_spelling_and_refuses_anything_else() {
    assert_eq!(
        serde_json::from_value::<RecordOrigin>(serde_json::json!("backfill"))
            .expect("declared value"),
        RecordOrigin::Backfill,
    );
    // The marker is closed. A third value is a contract violation, not a
    // forward-compatible extension: a consumer that cannot tell imported
    // history from live consumption is the gap the marker exists to close.
    serde_json::from_value::<RecordOrigin>(serde_json::json!("imported"))
        .expect_err("RecordOrigin is closed");
    serde_json::from_value::<RecordOrigin>(serde_json::json!("Live"))
        .expect_err("the wire spelling is lowercase");
}
```

Add `RecordOrigin` to the `use` list at the top of `models_tests.rs`.

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo nextest run -p cf-gears-usage-collector-sdk -E 'test(record_origin)'
```

Expected: compile failure — `cannot find type RecordOrigin`.

- [ ] **Step 3: Add the type**

In `models.rs`, immediately after the `impl EntryType` block (so the two closed
discriminators sit together):

```rust
/// Which ingestion path admitted an entry.
///
/// **Server-assigned, never caller-supplied.** The Ingestion Gateway stamps
/// it from the route the entry arrived on
/// (`cpt-cf-usage-collector-adr-backfill-isolation`), which is why it is
/// absent from [`CreateUsageRecord`] and refused by that type's
/// `deny_unknown_fields` wire shadow. It joins `id` and `accepted_at` in
/// DESIGN §3.1's server-assigned group.
///
/// It applies to invalidation entries exactly as to measurements. The
/// covered-period bounds belong to the path rather than to the entry kind,
/// so a withdrawal of a period older than the live past tolerance travels
/// the backfill route and reads `backfill` — which makes that the normal
/// origin for a correction of closed history, not an unusual one.
///
/// The value lets a consumer separate imported history from current
/// consumption. That distinction matters once a charge has already been
/// raised for a period: a consumer rating the feed handles a backfilled
/// entry as batch catch-up rather than as current consumption.
///
/// Closed, and deliberately so. It is also a bounded metric label on
/// `uc_ingestion_records_total` and `uc_ingestion_duration_seconds`
/// (DESIGN §3.11.5), so an open vocabulary here would be unbounded
/// cardinality there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordOrigin {
    /// Admitted by the live ingestion path — `POST /records` or
    /// `create_usage_record` / `create_usage_records`.
    Live,
    /// Admitted by the dedicated bulk-import route — `POST /records/backfill`
    /// or `backfill_usage_records`.
    Backfill,
}

impl RecordOrigin {
    /// The wire spelling, shared by the REST projection, the `$filter`
    /// surface and the metric label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Backfill => "backfill",
        }
    }
}
```

Re-export it from `lib.rs` beside `EntryType`.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo nextest run -p cf-gears-usage-collector-sdk -E 'test(record_origin)'
```

Expected: 3 passed.

- [ ] **Step 5: Full bar and commit**

Run the full verification bar. Expected `651 passed, 6 skipped`.

```bash
git add -A
git commit -s -m "feat(usage-collector-sdk): add the RecordOrigin marker

Closed live/backfill discriminator, spelled as the OpenAPI RecordOrigin
schema declares it. Server-assigned by the Ingestion Gateway from the path
an entry arrived on; nothing consumes it yet."
```

---

## Task 3: `origin` on `UsageRecord`, through the projection

This is the field addition, and it deliberately breaks the host crate. See the
compilation-unit warning above: at the end of this task the SDK package compiles
and its tests pass, and `cf-gears-usage-collector` does not compile. Task 4 fixes
that and owes the first in-place host run.

**Files:**
- Modify: `gears/system/usage-collector/usage-collector-sdk/src/models.rs`
- Test: `gears/system/usage-collector/usage-collector-sdk/src/models_tests.rs`

- [ ] **Step 1: Write the failing tests**

Append to `models_tests.rs`:

These use `sample_create_usage_record(subject_ref, invalidates)` and
`sample_usage_record(subject_ref, invalidates)`, the two fixture builders
`models_tests.rs` already has at lines 67 and 83. Do not add near-duplicates of
them.

```rust
#[test]
fn the_projection_stamps_the_origin_it_is_handed() {
    let submission = sample_create_usage_record(None, None);
    let live = submission
        .clone()
        .try_into_usage_record(RecordOrigin::Live)
        .expect("valid submission");
    let backfilled = submission
        .try_into_usage_record(RecordOrigin::Backfill)
        .expect("valid submission");

    assert_eq!(live.origin, RecordOrigin::Live);
    assert_eq!(backfilled.origin, RecordOrigin::Backfill);
}

#[test]
fn origin_is_not_an_input_to_the_derived_identity() {
    // The dedup identity is the 5-tuple
    // (tenant, gts_type, key, window_start, window_end) and `origin` is not
    // one of its five members
    // (`cpt-cf-usage-collector-adr-record-identity-derivation`). This is
    // load-bearing rather than incidental: re-importing history that was
    // once emitted live has to collide with the entry it re-creates so the
    // store can absorb it as a duplicate, and it can only collide if the
    // identifier ignores the path.
    let submission = sample_create_usage_record(None, None);
    let live = submission
        .clone()
        .try_into_usage_record(RecordOrigin::Live)
        .expect("valid submission");
    let backfilled = submission
        .try_into_usage_record(RecordOrigin::Backfill)
        .expect("valid submission");

    assert_eq!(live.id, backfilled.id);
}

#[test]
fn a_create_submission_cannot_carry_an_origin() {
    // `origin` is server-assigned, so the ingestion shape has no such
    // property and its `deny_unknown_fields` shadow refuses one. A caller
    // that could name its own path could label imported history as live
    // consumption, which is the distinction the marker exists to make.
    let mut json = serde_json::to_value(sample_create_usage_record(None, None))
        .expect("the submission serializes through its own codec");
    json.as_object_mut()
        .expect("object")
        .insert("origin".to_owned(), json!("live"));

    serde_json::from_value::<CreateUsageRecord>(json)
        .expect_err("origin is server-assigned and must be refused on the create shape");
}

#[test]
fn the_persisted_wire_shape_carries_origin_and_requires_it() {
    let record = sample_create_usage_record(None, None)
        .try_into_usage_record(RecordOrigin::Backfill)
        .expect("valid submission");
    let json = serde_json::to_value(&record).expect("serializes");

    assert_eq!(
        json.get("origin"),
        Some(&json!("backfill")),
        "every persisted entry carries its origin on the wire",
    );

    // Required, not defaulted. An entry decoded from a body with no
    // `origin` has no truthful value to fall back on, and defaulting to
    // `live` would silently relabel imported history as current
    // consumption.
    let mut without = json.clone();
    without.as_object_mut().expect("object").remove("origin");
    serde_json::from_value::<UsageRecord>(without)
        .expect_err("origin is mandatory on the persisted shape");

    // And the two halves of the codec agree about it.
    assert_eq!(
        serde_json::from_value::<UsageRecord>(json).expect("round-trips"),
        record,
    );
}
```

Note the third test builds its wire object by serializing a real
`CreateUsageRecord` rather than hand-writing JSON, so it cannot drift from the
shape the codec actually emits. The existing `minimal_create_record_json()` in
`dto_tests.rs` is the REST DTO's fixture and is a different shape — do not reach
for it from the SDK crate.

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo nextest run -p cf-gears-usage-collector-sdk -E 'test(origin)'
```

Expected: compile failure — `try_into_usage_record` takes no argument and
`UsageRecord` has no field `origin`.

- [ ] **Step 3: Add the field to the domain type**

In `UsageRecord`, between `idempotency_key` and `invalidation` (matching the
property order of the OpenAPI `UsageRecord` schema, where `origin` sits directly
before `invalidates`):

```rust
    /// Which path admitted this entry — server-assigned by the Ingestion
    /// Gateway from the route it arrived on, never caller-supplied. See
    /// [`RecordOrigin`]. Not an input to [`Self::id`]'s derivation.
    pub origin: RecordOrigin,
```

- [ ] **Step 4: Give the projection its parameter**

Change the signature and the construction:

```rust
    pub fn try_into_usage_record(
        self,
        origin: RecordOrigin,
    ) -> Result<UsageRecord, UsageCollectorError> {
```

and add `origin,` to the `Ok(UsageRecord { … })` literal, next to `id`, since the
two are the server-assigned pair this function stamps.

Update the doc comment. Two sentences currently claim a property this change
retires; both must be replaced, not softened — a retracted claim that is merely
reworded ships anyway:

- "Because the identity is a pure projection of caller-supplied fields it cannot
  be supplied independently" — the identity is still a pure projection of
  caller-supplied fields, but the *function* is no longer one. Say precisely
  that: the derivation reads only the five caller-supplied members of the dedup
  identity, and `origin` reaches the record beside it without entering it.
- "Every other field is forwarded verbatim, the invalidation reference included
  — it is not an input to the derivation." — extend to name `origin` as the one
  field that is *not* forwarded from the submission but stamped from the
  argument.

Add, in the same doc comment:

```
/// `origin` is server-assigned
/// (`cpt-cf-usage-collector-adr-backfill-isolation`), so it arrives as an
/// argument rather than being stamped onto an already-constructed record.
/// That keeps one site deciding an entry's origin, instead of a
/// construct-then-stamp pair whose halves can drift apart or be reordered.
///
/// It is a convention rather than a guarantee: this type's fields are
/// public and it is not `#[non_exhaustive]`, so any holder can build a
/// modified copy with a struct update, and this crate's own fixtures do.
/// What the argument buys is that no such copy sits on the ingestion path.
```

*(Corrected during execution. This block originally claimed the absence of a
`&mut UsageRecord` and of a struct-update path made "the append-only invariant
checkable at the type level". See D1 — both halves are false, and the shipped
version of this comment had to be rewritten.)*

- [ ] **Step 5: Add `origin` to both `UsageRecord` shadows**

In `UsageRecordWire`, between `idempotency_key` and `invalidates`:

```rust
    origin: RecordOrigin,
```

**No `#[serde(default)]`** — the OpenAPI `UsageRecord` schema lists `origin` in
`required`, and a default would invent a path for an entry that never named one.

In `UsageRecordWireRef`, at the same position:

```rust
    origin: RecordOrigin,
```

**No `skip_serializing_if`** — it is mandatory and always emitted.

Add `origin` to the `TryFrom<UsageRecordWire>` destructure and to the `Self { … }`
it builds; add it to the `Serialize for UsageRecord` destructure and pass
`origin: *origin` (it is `Copy`).

The compiler enforces all four edits: both `Self { … }` literals and both
exhaustive destructures fail to build until every one is done. That is the guard
described in `UsageRecordWire`'s own doc comment.

- [ ] **Step 6: Fix the call sites in `models_tests.rs` and `models.rs`**

*(Corrected during execution, twice. The facts table's 47 / 29 are `grep` line **mentions**, which include test names and doc prose; the actual calls are **18** in `models_tests.rs`. `models.rs` has 4 mentions of which 1 is the definition and 3 are doc links. And the step below originally said "two of the three are prose" of `id.rs` / `id_tests.rs` / `error.rs` — **all three** are prose, so none needed editing. Counting mentions as call sites, and then miscounting the exceptions, is the arithmetic residue this branch keeps producing; it took an implementer and a reviewer to catch both halves.)*

```bash
grep -rn --include='*.rs' -F 'try_into_usage_record' \
  usage-collector-sdk/src | grep -v '///'
```

Every call gains an argument. Pass `RecordOrigin::Live` at each existing site
unless the test's subject is the backfill path — none of them is today.
`usage-collector-sdk/src/id.rs`, `id_tests.rs` and `error.rs` each mention
`try_into_usage_record` in doc prose only, so none of them needs editing.

- [ ] **Step 7: Run the SDK tests**

```bash
cargo nextest run -p cf-gears-usage-collector-sdk --no-fail-fast
```

Expected: all SDK tests pass, including the four new ones.

**`cargo check --workspace` will fail here, in `cf-gears-usage-collector`.** That
is this task's expected end state. Do not paper over it by adding a `Default` for
`RecordOrigin` or a defaulted field — Task 4 threads the value properly.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -s -m "feat(usage-collector-sdk)!: carry the admitting path on every entry

UsageRecord gains a server-assigned origin, stamped by the projection that
already derives id. try_into_usage_record takes it as an argument rather
than the record being mutated afterwards, so one site decides an entry's
origin instead of a construct-then-stamp pair whose halves can drift.

origin is not one of the five dedup-identity members, so re-importing an
entry that was once emitted live still collides with it.

The host crate does not compile at this commit; the next one threads the
value through it.

BREAKING CHANGE: UsageRecord gains a mandatory origin field and
try_into_usage_record takes a RecordOrigin argument. A persisted-entry wire
body without an origin property is now refused."
```

---

## Task 4: Thread `origin` through the host, and pin the response key set

Restores the host crate to compiling and to the full baseline. **This task owes
the first in-place host test run.**

The live path stamps `RecordOrigin::Live`. The backfill path does not exist yet
— but the two inner service functions take the `origin` parameter *now*, so that
Task 9's bound enforcement and Task 11's public wrapper have somewhere to plug in
rather than rewriting each other's code.

**Files:**
- Modify: `usage-collector/src/domain/service.rs`
- Modify: `usage-collector/src/api/rest/dto.rs`
- Modify: `usage-collector/src/domain/test_support.rs`
- Modify: every `*_tests.rs` with a `UsageRecord` struct literal
- Test: `usage-collector/src/api/rest/dto_tests.rs`

- [ ] **Step 1: Write the failing tests**

In `dto_tests.rs`, extend the two existing literal key-set assertions. These are
the assertions DIVERGENCES entry 10 says "will fail the day either gap closes" —
this is that day, and updating them is the intended consequence, not a workaround.

In `usage_record_dto_serialises_exactly_the_declared_wire_keys`, add `"origin"`
to both expected vectors (alphabetically between `"idempotency_key"` and
`"resource_ref"` in the measurement case, and between `"invalidates"` and
`"reason_code"` in the withdrawal case), and rewrite the comment that currently
reads:

> This is what the gear emits, not what `usage-collector-v1.yaml`'s
> `UsageRecord` declares: the contract also requires `accepted_at`,
> `acceptance_sequence` and `origin`, and spells `value` as `quantity`.

to name only the gaps that are still open — `accepted_at`,
`acceptance_sequence`, and `value`/`quantity`. Leaving `origin` in that sentence
would be a claim the code has just falsified.

`dto_tests.rs` builds its fixtures through one private constructor,
`sample_persisted_entry(invalidation: Option<Invalidation>)` at line 67, with
`sample_persisted_record()` and `sample_persisted_invalidation()` as its two
callers. Give that constructor a second parameter rather than adding a third
fixture that duplicates it:

```rust
fn sample_persisted_entry(
    invalidation: Option<Invalidation>,
    origin: RecordOrigin,
) -> UsageRecord { … origin, … }

fn sample_persisted_record() -> UsageRecord {
    sample_persisted_entry(None, RecordOrigin::Live)
}

fn sample_persisted_invalidation() -> UsageRecord {
    sample_persisted_entry(Some(…), RecordOrigin::Live)
}

/// An entry admitted by the backfill route. Separate from
/// [`sample_persisted_record`] because `origin` is the one field of the
/// response projection whose two values are not interchangeable to a
/// consumer: one is current consumption and the other is imported history.
fn sample_backfilled_record() -> UsageRecord {
    sample_persisted_entry(None, RecordOrigin::Backfill)
}
```

Parameterising the constructor keeps both fixture shapes built one way, so a
future field lands in one place rather than two. **Do not reach for
`UsageRecord { origin: …, ..other }` here instead** — it does compile (the type's
fields are public and it is not `#[non_exhaustive]`; see D1), which is exactly
why it is a convention to hold rather than a rule the compiler enforces. A
struct-update site is also *invisible* to the next field addition: it inherits
the new field from its base silently instead of failing to build, which is how a
fixture ends up asserting against a value nobody chose.

Then add:

```rust
#[test]
fn the_response_projection_carries_the_origin_the_entry_was_admitted_under() {
    let live = serde_json::to_value(UsageRecordDto::from(sample_persisted_record()))
        .expect("serializes");
    assert_eq!(live.get("origin"), Some(&serde_json::json!("live")));

    let imported = serde_json::to_value(UsageRecordDto::from(sample_backfilled_record()))
        .expect("serializes");
    assert_eq!(imported.get("origin"), Some(&serde_json::json!("backfill")));
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo nextest run -p cf-gears-usage-collector -E 'test(wire_keys) or test(origin)'
```

Expected: the crate still does not compile (Task 3's end state). That is the
failure. Once Steps 3–6 land, these tests must pass on their own merits.

- [ ] **Step 3: Give both inner service paths the parameter**

In `service.rs`:

```rust
    async fn create_usage_record_inner(
        &self,
        ctx: &SecurityContext,
        record: CreateUsageRecord,
        origin: RecordOrigin,
    ) -> Result<UsageRecord, UsageCollectorError> {
```

and at the projection site:

```rust
        let record = record.try_into_usage_record(origin)?;
```

Same two edits in `create_usage_records_inner` (its projection is inside the
`for (index, submission) in records.into_iter().enumerate()` loop:
`submission.try_into_usage_record(origin)`).

The two public wrappers `create_usage_record` and `create_usage_records` pass
`RecordOrigin::Live`. Add a one-line comment at each saying the live route is
what makes it `Live` — the value comes from the path, not from a default.

- [ ] **Step 4: Add `origin` to `UsageRecordDto`**

Between `idempotency_key` and `invalidates`:

```rust
    /// Which ingestion path admitted the entry — `live` or `backfill`.
    /// Server-assigned, `required` on the OAS `UsageRecord`, so it is never
    /// omitted. Flattened to `String` for the same reason as
    /// [`Self::gts_type_id`]: to keep `utoipa` out of the SDK crate.
    pub origin: String,
```

and in `From<UsageRecord> for UsageRecordDto`, `origin: value.origin.as_str().to_owned(),`.
Read it before the destructure if the borrow checker requires, exactly as
`entry_type` already is.

- [ ] **Step 5: Fix every remaining construction site**

```bash
cargo check -p cf-gears-usage-collector -p cf-gears-noop-usage-collector-plugin \
  --all-targets 2>&1 | grep -c 'missing field `origin`'
```

**Include the noop plugin package** — it has two `UsageRecord` literals in
`plugin_tests.rs` and `cargo check -p cf-gears-usage-collector` alone will not
see them. Work through them. Every existing fixture takes `origin: RecordOrigin::Live`
unless the test is specifically about an imported entry — none is yet.
`test_support.rs`'s `projected()` helper and `withdrawal_of()` are the two that
most fixtures route through; fixing those first will collapse the count sharply.

`withdrawal_of` deserves a moment's thought rather than a mechanical `Live`: a
withdrawal copies its target faithfully, but `origin` is server-assigned and not
a copied field, so the withdrawal's origin is the path *it* travelled, which may
differ from its target's. Give the helper the target's origin as the default and
a way to override it — or take it as a parameter — and say why in a comment.
Task 11 needs a withdrawal with `origin = Backfill` whose target is `Live`.

- [ ] **Step 6: Run the full host test suite in place**

```bash
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk \
  -p cf-gears-noop-usage-collector-plugin --no-fail-fast
```

Expected: **the full baseline restored, plus the new tests** — no fewer than 651
plus this task's additions, 6 skipped. **This is the first host run since Task 2.
Read its output carefully**: any test that passes here for the first time in
three tasks has not been observed passing since, and slice 4 shipped two tests
that passed for the wrong reason in exactly this window.

- [ ] **Step 7: Full bar and commit**

Run the whole bar including `cargo doc`. Confirm the host's warning count is
still **35** and that none of the new warnings, if any, name a symbol this slice
created.

```bash
git add -A
git commit -s -m "feat(usage-collector)!: return the admitting path on every read

The live path stamps origin = live; the REST projection returns it. Both
inner ingestion paths take the origin as a parameter so the backfill route
has somewhere to plug in.

Closes the origin half of the response-shape gap DIVERGENCES entry 10
records; accepted_at, acceptance_sequence and value/quantity are still
open and the dto_tests comment now names only those.

BREAKING CHANGE: every UsageRecord response body gains a required origin
property."
```

---

## Task 5: The `origin` metric label

DESIGN §3.11.5 puts `origin` on `uc_ingestion_records_total` and on
`uc_ingestion_duration_seconds`. Neither carries it today; `ports/metrics.rs`
documents the gap at the point of absence, and DIVERGENCES entry 9(c) records it.

**A metric label is a wire contract and renaming one is silent.** Slice 4 renamed
`record_kind` → `entry_type` on this same counter, which leaves an operator's
`sum by (record_kind)` collapsing into an absent-label bucket while the series
keeps reporting. This task *adds* rather than renames, which is safer — but
adding a label to a previously label-free histogram splits its series, so it goes
in the commit message.

**Files:**
- Modify: `usage-collector/src/domain/ports/metrics.rs`
- Modify: `usage-collector/src/infra/metrics.rs`
- Modify: `usage-collector/src/domain/type_resolver/resolver_tests.rs`
- Modify: `usage-collector/src/domain/service.rs`
- Test: `usage-collector/src/infra/metrics_tests.rs`

- [ ] **Step 1: Write the failing test**

`metrics_tests.rs` already reads attributes back off an in-memory exporter:
`local_provider()` (line 22), `meter(&provider, TEST_PREFIX)` (line 30),
`counter_sum_with_label(&exporter, name, key, value)` (line 53) and
`histogram_count` (line 100). Use them; do not add a second way to read the
exporter.

First, update the existing `ingestion_instruments_render_names_labels_and_buckets`
(line 296) — its two `record_ingestion_record` calls and its one
`observe_ingestion_duration(0.05)` call all gain an origin argument. Give the two
counter calls **different** origins so the test proves the label is read from the
argument rather than pinned to a constant, and add the two assertions below to
it. Then add:

```rust
#[test]
fn the_ingestion_instruments_separate_live_from_backfilled_entries() {
    // DESIGN §3.11.5 puts `origin` on both ingestion families. The counter
    // carries the throughput NFR, and the backfill share is one of the
    // things it exists to make legible. The histogram carries the latency
    // budget, and a bulk import's latency profile is not the live path's —
    // averaging them together is what would hide a catch-up job degrading
    // live ingestion.
    let (provider, exporter) = local_provider();
    let m = meter(&provider, TEST_PREFIX);

    m.record_ingestion_record(
        RecordOutcome::Accepted,
        EntryType::Record,
        RecordOrigin::Live,
        RecordErrorCategory::None,
    );
    m.record_ingestion_record(
        RecordOutcome::Accepted,
        EntryType::Invalidation,
        RecordOrigin::Backfill,
        RecordErrorCategory::None,
    );
    m.observe_ingestion_duration(0.05, RecordOrigin::Live);
    m.observe_ingestion_duration(0.4, RecordOrigin::Backfill);
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(&exporter, "uc_ingestion_records_total", "origin", "live"),
        1,
    );
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_ingestion_records_total", "origin", "backfill"),
        1,
    );

    // A withdrawal of closed history is the entry that carries both new
    // label values at once, and it is the ordinary case rather than an
    // exotic one: the covered-period bounds belong to the path, so a
    // correction of a closed period travels the backfill route.
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_records_total",
            "entry_type",
            "invalidation",
        ),
        1,
    );

    // The histogram was label-free before this slice, so both observations
    // used to land in one series; they are now two.
    assert_eq!(histogram_count(&exporter, "uc_ingestion_duration_seconds"), 2);
}
```

`counter_sum_with_label` is the only reader here that takes a label; if the
histogram needs a per-label count, add a `histogram_count_with_label` beside
`histogram_count` following its shape, rather than reaching into the exporter
inline.

- [ ] **Step 2: Run to verify it fails**

```bash
cargo nextest run -p cf-gears-usage-collector -E 'test(origin_label)'
```

Expected: compile failure — the trait methods take no origin.

- [ ] **Step 3: Add the label key and widen the two trait methods**

In `ports/metrics.rs`, in `pub mod key`:

```rust
    /// `origin` — which ingestion path admitted the entry.
    pub const ORIGIN: &str = "origin";
```

Change the two trait signatures:

```rust
    /// Observe `uc_ingestion_duration_seconds{origin}` — one per completed
    /// ingestion request (single-emit call or batch submission). The label
    /// separates the bulk-import latency profile from the live path's,
    /// which the live-path p95 budget depends on not being averaged
    /// together with a catch-up job's.
    fn observe_ingestion_duration(&self, seconds: f64, origin: RecordOrigin);

    /// Increment
    /// `uc_ingestion_records_total{outcome, entry_type, origin, error_category}`
    /// once per entry in a batch acknowledgement (and once for a single
    /// emit).
    ///
    /// `entry_type` carries the correction share and `origin` the backfill
    /// share — between them, what makes a withdrawal of closed history
    /// visible in the ingestion profile at all.
    fn record_ingestion_record(
        &self,
        outcome: RecordOutcome,
        entry_type: EntryType,
        origin: RecordOrigin,
        error_category: RecordErrorCategory,
    );
```

**Delete** the two sentences in the existing doc comment that say the gear has no
`RecordOrigin` to populate the label from and that the series is emitted without
it. They are now false. Do not reword them — replace them.

Update `NoopMetrics`' two stubs, `UcMetricsMeter`'s two implementations (adding
`KeyValue::new(key::ORIGIN, origin.as_str())` to the counter's attribute array
and giving the histogram an attribute array where it currently has none), and
`RecordingMetrics` in `resolver_tests.rs`.

- [ ] **Step 4: Thread the value at the two call sites in `service.rs`**

`create_usage_record` (`:1066`) and `create_usage_records` (`:1136`) both need
the origin. Both are the public wrappers, which currently know it as `Live`
only — take the `origin` parameter on the wrappers as well now, defaulting
nothing:

```rust
    async fn create_usage_record_instrumented(
        &self,
        ctx: &SecurityContext,
        record: CreateUsageRecord,
        origin: RecordOrigin,
    ) -> Result<UsageRecord, UsageCollectorError> {
```

Prefer extracting the instrumented body into a private helper taking `origin`,
with `create_usage_record` / `create_usage_records` as thin `Live` wrappers, so
Task 11's backfill entry point reuses the identical telemetry rather than
copying it. **Copying it is how the two paths drift**, and the whole point of the
Ingestion Gateway being one component is that they do not.

- [ ] **Step 5: Run the tests**

Expected: all pass.

- [ ] **Step 6: Full bar and commit**

```bash
git add -A
git commit -s -m "feat(usage-collector)!: label the ingestion instruments with the admitting path

DESIGN §3.11.5 puts origin on uc_ingestion_records_total and
uc_ingestion_duration_seconds. Both now carry it, closing DIVERGENCES
entry 9(c).

BREAKING CHANGE: uc_ingestion_duration_seconds was label-free and now
carries origin, so its series splits in two. A query aggregating it
without 'by (origin)' is unaffected; one that assumed a single series per
instance now sees two. uc_ingestion_records_total gains a fourth label,
which is additive."
```

---

## Task 6: `origin` on the filter and order surfaces

DESIGN §3.1's `UsageRecordFilterField` row lists eight fixed fields;
`UsageRecordQuery` carries seven. `origin` is the missing one.

**Files:**
- Modify: `usage-collector-sdk/src/models.rs`
- Test: `usage-collector-sdk/src/models_tests.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_filter_surface_carries_every_fixed_field_design_declares() {
    // DESIGN §3.1's UsageRecordFilterField row: tenant_id, resource_id,
    // resource_type, subject_id, subject_type, entry_type, origin,
    // invalidates — plus the queried type's declared metadata keys,
    // resolved per request. `id`, `window_start` and `window_end` are also
    // on the schema for the keyset and the plugin column mapping; see the
    // module comment above `UsageRecordQuery`.
    for field in [
        "tenant_id", "resource_id", "resource_type", "subject_id",
        "subject_type", "entry_type", "origin", "invalidates",
    ] {
        assert!(
            UsageRecordFilterField::from_str(field).is_ok(),
            "{field} is a declared filterable field",
        );
    }
}

#[test]
fn origin_is_a_sound_keyset_order_key() {
    // The rule beside KEYSET_SAFE_RECORD_FIELDS is that a key has to be an
    // attribute the SDK guarantees on every entry, because a row-value
    // keyset over a nullable column silently drops rows. `origin` is
    // mandatory on UsageRecord, so every plugin persists it non-null —
    // unlike `entry_type`, which is derived from an optional field and
    // which the SDK obliges nobody to materialize.
    assert!(KEYSET_SAFE_RECORD_FIELDS.contains(&"origin"));
    assert!(is_keyset_safe_record_field("origin"));

    // The exclusions are unchanged.
    for field in ["subject_id", "subject_type", "invalidates", "entry_type"] {
        assert!(!is_keyset_safe_record_field(field), "{field} is not keyset-safe");
    }
}
```

Match the real names of `from_str` / `is_keyset_safe_record_field` against the
file — read it rather than trusting the sketch.

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Add the field**

In `UsageRecordQuery`, after `entry_type`:

```rust
    /// `usage_records.origin` — which ingestion path admitted the entry
    /// (`live` / `backfill`). On the filter surface because DESIGN §3.1
    /// lists it among the fixed `$filter` and `group_by` fields: a
    /// consumer that has already raised a charge for a period needs to
    /// separate imported history from current consumption
    /// (`cpt-cf-usage-collector-adr-backfill-isolation`).
    ///
    /// Declared `String` on the filter wire, like `entry_type` — but
    /// unlike `entry_type` it is a stored attribute of the entry rather
    /// than a function of an optional one, so a plugin maps it to a real
    /// non-null column and it **is** a sound order key. See
    /// [`is_keyset_safe_record_field`].
    #[odata(filter(kind = "String"))]
    pub origin: String,
```

Add `"origin"` to `KEYSET_SAFE_RECORD_FIELDS`.

- [ ] **Step 4: Fix the two cardinality claims in the surrounding prose**

Both are counts, and counts are this branch's most persistent residue.

- The doc comment above `KEYSET_SAFE_RECORD_FIELDS` says "**six names** is short
  enough to be actionable". It is now **seven**.
- Its closing paragraph enumerates the two excluded kinds and asserts "Every
  entry of [`KEYSET_SAFE_RECORD_FIELDS`] is an attribute the record itself
  carries on every entry". That stays true — but add a sentence saying why
  `origin` qualifies where `entry_type` does not, since a reader meeting two
  mandatory-on-every-entry fields with opposite verdicts will otherwise conclude
  the rule is arbitrary: `entry_type` is derived from an optional field and the
  SDK obliges no plugin to materialize a column for it; `origin` is a stored
  field of `UsageRecord` that every plugin must persist.

Then check the module-level comment above `UsageRecordQuery` for any count of
the schema's fields, and the `$orderby`-rejection error message for any
enumeration of the admissible set — if either states a number or lists the names,
it changes here too.

```bash
grep -rn --include='*.rs' -i 'six\|seven\|eight' usage-collector-sdk/src/models.rs
```

- [ ] **Step 5: Run the tests, then the full bar, then commit**

```bash
git add -A
git commit -s -m "feat(usage-collector-sdk): make origin filterable and orderable

DESIGN §3.1 lists origin among the eight fixed filter and group_by fields;
the schema carried seven. It is also a sound keyset order key — mandatory,
stored, non-null on every entry — unlike entry_type, which is derived from
an optional field. The keyset-safe set is now seven names and the doc
comment that counted six says so."
```

---

## Task 7: The two rejection reasons and their messages

The past-tolerance rejection naming the backfill route is normative in DESIGN,
not a nicety. The ADR adds that "both surfaces carry it" — so the message names
the REST path *and* the SDK trait method, because an in-process caller cannot act
on a URL.

**Files:**
- Modify: `usage-collector-sdk/src/reason.rs`
- Modify: `usage-collector-sdk/src/error.rs`
- Modify: `usage-collector-sdk/src/lib.rs`
- Test: `usage-collector-sdk/src/reason_tests.rs`, `models_tests.rs`

- [ ] **Step 1: Write the failing tests**

In `reason_tests.rs`, follow the existing round-trip pattern for the two new
codes. Then, in `models_tests.rs` (or wherever `error.rs`'s constructors are
tested — read first):

```rust
#[test]
fn the_past_tolerance_rejection_names_both_backfill_surfaces() {
    // `cpt-cf-usage-collector-adr-backfill-isolation`: "The rejection names
    // the route, and both surfaces carry it." A REST caller needs the path;
    // an in-process caller needs the trait method, and a URL tells it
    // nothing. Asserted against literals because this string is what turns
    // a rejection into an actionable instruction.
    let err = UsageCollectorError::covered_period_before_past_tolerance(
        window_end,
        now,
        time::Duration::hours(48),
    );
    let UsageCollectorError::InvalidArgument { reason, field, detail, .. } = &err else {
        panic!("expected InvalidArgument");
    };
    assert_eq!(*reason, ValidationReason::PastWindow);
    assert_eq!(field, WINDOW_END_FIELD);
    assert!(detail.contains("/usage-collector/v1/records/backfill"), "{detail}");
    assert!(detail.contains("backfill_usage_records"), "{detail}");
}

#[test]
fn the_future_tolerance_rejection_does_not_name_the_backfill_route() {
    // The backfill route lifts the past bound and nothing else. Pointing a
    // clock-skewed emitter at it would send a defect somewhere it is just
    // as invalid, and the route's own description says so: it exists "for
    // exactly the periods that bound rejects", meaning the past one.
    let err = UsageCollectorError::covered_period_beyond_future_tolerance(
        window_end,
        now,
        time::Duration::minutes(5),
    );
    let UsageCollectorError::InvalidArgument { reason, detail, .. } = &err else {
        panic!("expected InvalidArgument");
    };
    assert_eq!(*reason, ValidationReason::FutureWindow);
    assert!(!detail.contains("backfill"), "{detail}");
}
```

That second test is the one worth having. It is the asymmetry the whole slice
turns on, and it is the shape of mistake a reader of the ADR's summary would
make.

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Add the reason codes**

In `reason.rs`, beside the other constants:

```rust
/// The covered period ends further into the future than the live path's
/// configured future tolerance. Raised on **both** ingestion routes: the
/// bound guards against an emitter opening a period that does not yet
/// exist, and the backfill route lifts only the past bound.
pub const FUTURE_WINDOW: &str = "FUTURE_WINDOW";
/// The covered period ends further into the past than the live path's
/// configured past tolerance. Raised on the live path only; the detail
/// names the backfill route, which exists for exactly these periods
/// (`cpt-cf-usage-collector-adr-backfill-isolation`).
pub const PAST_WINDOW: &str = "PAST_WINDOW";
```

and the matching `ValidationReason::FutureWindow` / `PastWindow` variants with
their `from_wire` and `as_wire` arms. `ValidationReason` is `#[non_exhaustive]`,
so adding variants is additive for downstream matchers.

- [ ] **Step 4: Add the route constant and the two constructors**

In `lib.rs` (or `models.rs` beside the other wire-name constants — pick the one
the existing `USAGE_RECORD_RESOURCE` lives next to):

```rust
/// The REST path of the dedicated backfill route, named by the live path's
/// past-tolerance rejection.
///
/// A constant rather than a literal because the string appears in the
/// rejection message, in the route registration, and in the OpenAPI
/// document, and a rejection naming a path that has moved is worse than one
/// naming no path at all.
pub const BACKFILL_ROUTE_PATH: &str = "/usage-collector/v1/records/backfill";
```

In `error.rs`, following `inverted_covered_period`'s shape exactly (same
`resource_type`, `field: WINDOW_END_FIELD`, `rfc3339` rendering of both
instants):

```rust
    /// The covered period ends beyond the ingestion path's future tolerance.
    ///
    /// Raised on **both** routes: the backfill route lifts the past bound
    /// and nothing else, so the detail deliberately does not mention it.
    /// Pointing a clock-skewed emitter at the backfill route would send a
    /// defect somewhere it is just as invalid.
    #[must_use]
    pub fn covered_period_beyond_future_tolerance(
        window_end: OffsetDateTime,
        now: OffsetDateTime,
        tolerance: Duration,
    ) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: WINDOW_END_FIELD.to_owned(),
            reason: ValidationReason::FutureWindow,
            detail: format!(
                "covered period ends at {}, more than {} after now ({}); an \
                 ingestion path admits only a period ending within that \
                 tolerance of the present",
                rfc3339(window_end),
                tolerance,
                rfc3339(now),
            ),
        }
    }

    /// The covered period ends beyond the live path's past tolerance.
    ///
    /// The detail names the backfill route on both surfaces, per
    /// `cpt-cf-usage-collector-adr-backfill-isolation`: "The rejection names
    /// the route, and both surfaces carry it." A REST caller needs the path
    /// and an in-process caller needs the method, and a URL tells the
    /// latter nothing.
    ///
    /// It says "entry" rather than "record" because the same rejection
    /// meets an invalidation withdrawing a closed period — which is the
    /// ordinary case for a correction, not a rare one.
    #[must_use]
    pub fn covered_period_before_past_tolerance(
        window_end: OffsetDateTime,
        now: OffsetDateTime,
        tolerance: Duration,
    ) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: WINDOW_END_FIELD.to_owned(),
            reason: ValidationReason::PastWindow,
            detail: format!(
                "covered period ends at {}, more than {} before now ({}); the \
                 live path admits only a period ending within that tolerance. \
                 Submit this entry on the backfill route instead — \
                 `POST {}`, or `backfill_usage_records` on the SDK trait",
                rfc3339(window_end),
                tolerance,
                rfc3339(now),
                crate::BACKFILL_ROUTE_PATH,
            ),
        }
    }
```

`time::Duration`'s `Display` renders as e.g. `48h`; check what it actually
produces and, if it is unhelpful, render whole seconds explicitly rather than
leaving an operator to decode it. `rfc3339` is the private helper the neighbouring
constructors already use.

- [ ] **Step 5: Run the tests, full bar, commit**

```bash
git add -A
git commit -s -m "feat(usage-collector-sdk): add the two covered-period bound reasons

FUTURE_WINDOW and PAST_WINDOW, with constructors that render the tolerance
and both instants. The past-tolerance detail names the backfill route on
REST and on the SDK trait, per cpt-cf-usage-collector-adr-backfill-isolation;
the future-tolerance detail deliberately does not, because the backfill
route lifts the past bound only. Nothing raises either one yet."
```

---

## Task 8: The three configuration keys

**Files:**
- Modify: `usage-collector/src/config.rs`
- Test: `usage-collector/src/config_tests.rs`

- [ ] **Step 1: Write the failing tests**

Follow the existing `config_tests.rs` patterns for defaults, TOML round-trip and
`validate()` rejections. Cover:

```rust
#[test]
fn the_covered_period_defaults_are_the_ones_design_publishes() {
    let cfg = UsageCollectorConfig::default();
    assert_eq!(cfg.live_future_tolerance_secs, 300, "5 minutes");
    assert_eq!(cfg.live_past_tolerance_secs, 172_800, "48 hours");
    assert_eq!(cfg.backfill_window_secs, 7_776_000, "90 days");
}

#[test]
fn a_zero_bound_is_rejected() {
    // A zero future tolerance refuses a period ending a microsecond from
    // now, and a zero past tolerance refuses everything that is not in the
    // future. Both are configuration mistakes that present at runtime as
    // total ingestion failure with a per-entry validation error; failing at
    // Gear::init names the key instead.
    let cases: [(&str, fn(&mut UsageCollectorConfig)); 3] = [
        ("live_future_tolerance_secs", |c| c.live_future_tolerance_secs = 0),
        ("live_past_tolerance_secs", |c| c.live_past_tolerance_secs = 0),
        ("backfill_window_secs", |c| c.backfill_window_secs = 0),
    ];
    for (key, mutate) in cases {
        let mut cfg = UsageCollectorConfig::default();
        mutate(&mut cfg);
        let err = cfg
            .validate()
            .expect_err("a zero {key} must be rejected at init");
        assert!(
            err.to_string().contains(key),
            "the rejection must name the offending key; got: {err}",
        );
    }
}

#[test]
fn a_backfill_window_narrower_than_the_live_past_tolerance_is_rejected() {
    // The live rejection tells an emitter to resubmit on the backfill
    // route. If the window were the narrower of the two, entries the live
    // path refuses would need elevated authorization on the route it names
    // — so the message would be sending an ordinary emitter somewhere it
    // cannot succeed. The three bounds are asymmetric by design
    // (`cpt-cf-usage-collector-adr-backfill-isolation`, "Why the three
    // bounds are asymmetric"), and this is the one ordering among them
    // that is load-bearing.
    let mut cfg = UsageCollectorConfig::default();
    cfg.backfill_window_secs = cfg.live_past_tolerance_secs - 1;
    assert!(cfg.validate().is_err());
}

#[test]
fn the_bounds_project_to_durations() {
    let bounds = UsageCollectorConfig::default().covered_period_bounds();
    assert_eq!(bounds.future_tolerance, time::Duration::minutes(5));
    assert_eq!(bounds.live_past_tolerance, time::Duration::hours(48));
    assert_eq!(bounds.backfill_window, time::Duration::days(90));
}
```

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Add the keys**

```rust
    /// How far into the future a covered period may end, in seconds.
    ///
    /// The live path rejects a period ending further ahead than this, and
    /// **so does the backfill route** — the route lifts the past bound
    /// only. The bound protects against a defective or clock-skewed
    /// emitter opening a period that does not yet exist
    /// (`cpt-cf-usage-collector-adr-backfill-isolation`).
    ///
    /// Defaults to 300 (5 minutes), the value DESIGN
    /// `cpt-cf-usage-collector-fr-live-future-time-bound` publishes.
    pub live_future_tolerance_secs: u64,

    /// How far into the past a covered period may end on the **live** path,
    /// in seconds.
    ///
    /// A period ending further back is rejected with a message naming the
    /// backfill route, which is where such an entry belongs. The default
    /// covers emitter outage and retry lag, which is what genuinely late
    /// live data is; anything older is history, and history belongs on the
    /// route that marks it.
    ///
    /// The bound belongs to the path, not to the entry kind, so it governs
    /// an invalidation over the period it copies exactly as it governs a
    /// measurement. A withdrawal of a closed month therefore travels the
    /// backfill route.
    ///
    /// Defaults to 172_800 (48 hours).
    pub live_past_tolerance_secs: u64,

    /// How far back the backfill route reaches without elevated
    /// authorization, in seconds.
    ///
    /// The route admits any period the future bound allows. This window
    /// decides only *which* PDP action each entry is authorized against:
    /// inside it, `create`; beyond it, the `backfill` action. It bounds the
    /// recomputation obligation a materialised aggregate carries.
    ///
    /// A deployment must not admit a window wider than the raw retention
    /// its storage profile guarantees for the target GTS type. The
    /// retention floor is this window plus one replay horizon
    /// (`cpt-cf-usage-collector-fr-billing-retention-floor`), 125 days at
    /// the launch defaults — a plugin-readiness condition surfaced at
    /// review, not a gear-side sweep.
    ///
    /// Defaults to 7_776_000 (90 days).
    pub backfill_window_secs: u64,
```

Defaults in `impl Default`. In `validate()`, reject zero for each with a message
saying what a zero value would do, and reject
`backfill_window_secs < live_past_tolerance_secs` with the reason from the test
comment above. Also reject any value that does not fit `i64` (the `time::Duration`
constructor takes `i64` seconds); a `u64::MAX` tolerance is a configuration
mistake, not an infinite bound.

Add the projection:

```rust
    /// The three covered-period bounds as durations, for the Ingestion
    /// Gateway. Config carries seconds because that is what a TOML file
    /// spells; the gateway compares instants.
    ///
    /// Infallible because [`Self::validate`] has already rejected a value
    /// that does not fit an `i64` — so the saturating conversion below can
    /// only fire on a config that never reached `Gear::init`.
    #[must_use]
    pub fn covered_period_bounds(&self) -> CoveredPeriodBounds {
        let secs = |v: u64| time::Duration::seconds(i64::try_from(v).unwrap_or(i64::MAX));
        CoveredPeriodBounds {
            future_tolerance: secs(self.live_future_tolerance_secs),
            live_past_tolerance: secs(self.live_past_tolerance_secs),
            backfill_window: secs(self.backfill_window_secs),
        }
    }
```

`CoveredPeriodBounds` does not exist until Task 9. **Reverse Task 8 and Task 9
if you prefer**, or land the three keys and their validation here and add
`covered_period_bounds()` in Task 9 — but do not stub the type.

- [ ] **Step 4: Thread the values in `module.rs`**

`Service::new_with_metrics` grows one parameter, `CoveredPeriodBounds` (one
struct, not three durations — the signature already carries six arguments).
`module.rs` passes `cfg.covered_period_bounds()`. Update all construction sites
found by:

```bash
grep -rn --include='*.rs' -F 'Service::new_with_metrics' usage-collector/src
```

**15 mentions across 9 files, several of them doc-comment prose.** Count the
actual calls yourself before reporting how many you changed.

- [ ] **Step 5: Run the tests, full bar, commit**

```bash
git add -A
git commit -s -m "feat(usage-collector): configure the three covered-period bounds

live_future_tolerance_secs, live_past_tolerance_secs and
backfill_window_secs, at the defaults DESIGN publishes. validate() rejects
a zero bound and a backfill window narrower than the live past tolerance —
which would make the live rejection point an emitter at a route it cannot
use. Nothing reads them yet."
```

---

## Task 9: Enforce the live path's two-sided bound

### Read this before starting — the past bound invalidates most ingestion fixtures

**Every ingestion-path test fixture in the host crate uses a covered period in
1970.** `input_record` and `persisted_record` in `service_tests.rs` both build
`window_start: OffsetDateTime::UNIX_EPOCH, window_end: UNIX_EPOCH + 1h`, and the
handler tests build RFC 3339 strings around the same epoch.

The moment the 48-hour past tolerance is enforced, **all of them are rejected
with `PAST_WINDOW`** — a period ending in 1970 is 56 years beyond the bound.
Measured:

| File | ingestion call sites | `UNIX_EPOCH` sites |
| --- | --- | --- |
| `domain/service_tests.rs` | 44 | 23 |
| `domain/service_metrics_tests.rs` | 16 | 4 |
| `api/rest/handlers/usage_records_tests.rs` | 15 | 28 |
| `domain/test_support.rs` | — | 2 |

Command: `grep -c '\.create_usage_record\|handle_create_usage_records' <file>`
and `grep -c UNIX_EPOCH <file>`.

**Not affected, and do not touch them:** `usage-collector-sdk/src/models_tests.rs`
(it calls `try_into_usage_record` directly, and the bound lives in the Service,
not the projection), `api/rest/dto_tests.rs` (a pure projection test that runs no
service), `domain/invalidation_tests.rs` (the faithful-copy comparator takes two
entries and reads no clock), and every read-path test — the query fixtures build
their `TimeRange` around the epoch and would need re-basing for no reason.

**This is the failure that would tempt someone into widening the default past
tolerance to make the suite green.** Do not. The 48 hours is published in DESIGN
and in the ADR. The fixtures are what is wrong: a covered period from 1970 was
never realistic, it was merely unconstrained.

Step 1 re-bases them as a separate commit, so that the enforcement commit's diff
shows the enforcement rather than 75 fixture edits.

**Files:**
- Modify: `usage-collector/src/domain/test_support.rs`
- Modify: `usage-collector/src/domain/service_tests.rs`
- Modify: `usage-collector/src/domain/service_metrics_tests.rs`
- Modify: `usage-collector/src/api/rest/handlers/usage_records_tests.rs`
- Create: `usage-collector/src/domain/covered_period.rs`
- Create: `usage-collector/src/domain/covered_period_tests.rs`
- Modify: `usage-collector/src/domain/mod.rs`
- Modify: `usage-collector/src/domain/service.rs`
- Test: `usage-collector/src/domain/service_tests.rs`

- [ ] **Step 1: Re-base the ingestion fixtures onto a recent covered period, and commit that alone**

Add to `test_support.rs`:

```rust
/// A covered period a live emitter could plausibly have just closed: one
/// hour long, ending an hour ago.
///
/// Ingestion fixtures need this because the live path bounds the end of the
/// covered period against the wall clock, 48 hours by default. The epoch
/// period these fixtures used before this slice was not realistic, only
/// unconstrained — no emitter submits 1970 on the live path, and one that
/// tried would now be told to use the backfill route.
///
/// Deliberately an hour clear of both bounds rather than minutes: a test
/// that is 30 seconds from a boundary is a test that fails on a loaded CI
/// runner. Read paths keep their epoch fixtures — they read no clock.
pub(crate) fn recent_window() -> (time::OffsetDateTime, time::OffsetDateTime) {
    let end = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
    (end - time::Duration::hours(1), end)
}
```

Repoint the fixture builders to it — `input_record` and `persisted_record` in
`service_tests.rs`, the equivalents in `service_metrics_tests.rs`, the two sites
in `test_support.rs`, and the RFC 3339 strings in
`handlers/usage_records_tests.rs` (format `recent_window()`'s bounds with
`time::format_description::well_known::Rfc3339` rather than hard-coding a date —
a hard-coded 2026 date becomes stale and this bug recurs in a year).

**Two things to check while doing it, neither of which the compiler will catch:**

- Some tests assert a **derived `id`**, and `window_start` / `window_end` are two
  of the five dedup-identity members. Any test with a hard-coded UUID expectation
  will now fail on the id, not on the period. Those expectations must be computed
  from the fixture rather than re-hard-coded to a new constant — a constant would
  go stale the moment the window moves again.
- Some tests pair an `input_record` with a `persisted_record` and expect them to
  correspond. Both must move together, or the pairing silently stops meaning
  anything while still passing.

At this point **nothing enforces a bound yet**, so this step is a pure refactor:
the full suite must still report the same pass count it did before it.

```bash
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk \
  -p cf-gears-noop-usage-collector-plugin --no-fail-fast
git add -A
git commit -s -m "test(usage-collector): base the ingestion fixtures on a recent covered period

Every ingestion-path fixture submitted a period ending in 1970, which was
never realistic — only unconstrained. The live path is about to bound the
period end against the wall clock, and an epoch period is 56 years beyond
that bound.

Read-path and projection fixtures keep their epoch windows: they read no
clock. Pure refactor; no behaviour changes and the pass count is unmoved."
```

- [ ] **Step 2: Write the failing unit tests**

`covered_period_tests.rs` — these are pure and need no `Service`:

```rust
use time::{Duration, OffsetDateTime};
use usage_collector_sdk::{RecordOrigin, ValidationReason};

use super::{CoveredPeriodBounds, enforce_covered_period_bounds, ingestion_action};
use crate::domain::authz::usage_record::actions;

/// A fixed instant, supplied rather than read from the clock, so every
/// entry of a batch is judged against one `now` and no test races the
/// clock. `2026-06-11T12:00:00Z`.
fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_780_142_400).expect("valid instant")
}

/// The published defaults: 5 minutes, 48 hours, 90 days.
fn default_bounds() -> CoveredPeriodBounds {
    CoveredPeriodBounds {
        future_tolerance: Duration::minutes(5),
        live_past_tolerance: Duration::hours(48),
        backfill_window: Duration::days(90),
    }
}

/// A covered-period end `offset` from `now()`. Negative is in the past.
fn ending(offset: Duration) -> OffsetDateTime {
    now() + offset
}

fn reason_of(err: &UsageCollectorError) -> &ValidationReason {
    match err {
        UsageCollectorError::InvalidArgument { reason, .. } => reason,
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

#[test]
fn the_live_path_admits_a_period_ending_inside_both_tolerances() {
    assert!(
        enforce_covered_period_bounds(
            &default_bounds(),
            RecordOrigin::Live,
            now(),
            ending(Duration::hours(-1)),
        )
        .is_ok()
    );
}

#[test]
fn the_live_path_rejects_a_period_ending_beyond_the_future_tolerance() {
    let err = enforce_covered_period_bounds(
        &default_bounds(),
        RecordOrigin::Live,
        now(),
        ending(Duration::hours(1)),
    )
    .expect_err("an hour ahead is beyond a five-minute tolerance");
    assert_eq!(*reason_of(&err), ValidationReason::FutureWindow);
}

#[test]
fn the_live_path_rejects_a_period_ending_beyond_the_past_tolerance() {
    let err = enforce_covered_period_bounds(
        &default_bounds(),
        RecordOrigin::Live,
        now(),
        ending(Duration::hours(-72)),
    )
    .expect_err("72 hours back is beyond a 48-hour tolerance");
    assert_eq!(*reason_of(&err), ValidationReason::PastWindow);
}

#[test]
fn the_live_path_admits_a_period_longer_than_the_past_tolerance_that_ends_inside_it() {
    // ADR confirmation case 2, and the case a rule phrased about the
    // period's LENGTH rather than its END would break. A monthly accrual
    // meter emits as soon as its month closes: the period is 30 days long
    // and ended a minute ago, and it is ordinary live consumption.
    let month_long_but_just_closed = ending(Duration::minutes(-1));
    assert!(
        enforce_covered_period_bounds(
            &default_bounds(),
            RecordOrigin::Live,
            now(),
            month_long_but_just_closed,
        )
        .is_ok(),
        "only the end of the covered period is read",
    );
}

#[test]
fn the_backfill_route_admits_what_the_live_past_tolerance_rejects() {
    let a_year_ago = ending(Duration::days(-365));
    assert!(
        enforce_covered_period_bounds(
            &default_bounds(),
            RecordOrigin::Live,
            now(),
            a_year_ago,
        )
        .is_err()
    );
    assert!(
        enforce_covered_period_bounds(
            &default_bounds(),
            RecordOrigin::Backfill,
            now(),
            a_year_ago,
        )
        .is_ok(),
        "the route exists for exactly the periods the past bound rejects, \
         and reaching past its own window is an authorization question \
         rather than an admission one",
    );
}

#[test]
fn the_backfill_route_still_rejects_a_period_ending_in_the_future() {
    // The route differs from POST /records in four respects and the future
    // bound is not one of them. Lifting it here would let a defective
    // emitter open a period that does not yet exist, on a route whose whole
    // purpose is history.
    let err = enforce_covered_period_bounds(
        &default_bounds(),
        RecordOrigin::Backfill,
        now(),
        ending(Duration::hours(1)),
    )
    .expect_err("the future bound governs both routes");
    assert_eq!(*reason_of(&err), ValidationReason::FutureWindow);
}

#[test]
fn the_action_is_create_inside_the_backfill_window_and_backfill_beyond_it() {
    let bounds = default_bounds();
    assert_eq!(
        ingestion_action(&bounds, RecordOrigin::Backfill, now(), ending(Duration::days(-30))),
        actions::CREATE,
        "inside the window an import needs no grant a live emission does not",
    );
    assert_eq!(
        ingestion_action(&bounds, RecordOrigin::Backfill, now(), ending(Duration::days(-120))),
        actions::BACKFILL,
        "beyond the window is the elevated case the action exists for",
    );
}

#[test]
fn the_live_path_authorizes_create_whatever_the_backfill_window_says() {
    // The backfill window is not read on the live path at all. An entry the
    // live path admits is inside 48 hours and so can never be beyond a
    // 90-day window — but the function must not lean on that arithmetic,
    // because the two keys are independently configurable and a deployment
    // that widened the past tolerance would otherwise start demanding an
    // elevated grant for live emission.
    assert_eq!(
        ingestion_action(
            &default_bounds(),
            RecordOrigin::Live,
            now(),
            ending(Duration::days(-365)),
        ),
        actions::CREATE,
    );
}
```

Every offset above is unambiguously inside or outside — one hour against five
minutes, 72 hours against 48, 120 days against 90. **Do not add a test at the
exact boundary.** It would assert the comparison's strictness, which no document
fixes, and pinning an unfixed choice makes the test an obstacle to a later
correction rather than a guard.

- [ ] **Step 3: Run to verify they fail**

- [ ] **Step 4: Write the module**

`covered_period.rs`:

```rust
//! The ingestion path's covered-period bounds.
//!
//! Three durations and two pure functions over them. Both functions read
//! **only the end of the covered period** — the instant that makes
//! consumption current or historical
//! (`cpt-cf-usage-collector-adr-backfill-isolation`, "Why the three bounds
//! are asymmetric"). Neither reads `window_start`, the length of the
//! period, the arrival instant, or the entry kind.
//!
//! That last exclusion is the one that surprises. A withdrawal copies its
//! target's period, so an invalidation of a closed month has a `window_end`
//! a month old and is rejected on the live path exactly as a fresh
//! measurement of that month would be. `origin = backfill` on a correction
//! is therefore the ordinary case.
//!
//! `now` is a parameter rather than a call, so every entry of one batch is
//! judged against a single instant and a test needs no clock control.

/// The three configured bounds. See `UsageCollectorConfig` for the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoveredPeriodBounds {
    /// Applies on **both** routes.
    pub future_tolerance: Duration,
    /// Applies on the live route only.
    pub live_past_tolerance: Duration,
    /// Read on the backfill route only, and to select an action rather than
    /// to admit or refuse.
    pub backfill_window: Duration,
}

/// Reject a covered period the path does not admit.
///
/// # Errors
///
/// * `FUTURE_WINDOW` when the period ends further ahead than
///   `future_tolerance`, on either route.
/// * `PAST_WINDOW` when it ends further back than `live_past_tolerance` on
///   the **live** route. The message names the backfill route.
pub fn enforce_covered_period_bounds(
    bounds: &CoveredPeriodBounds,
    origin: RecordOrigin,
    now: OffsetDateTime,
    window_end: OffsetDateTime,
) -> Result<(), UsageCollectorError> {
    if window_end - now > bounds.future_tolerance {
        return Err(UsageCollectorError::covered_period_beyond_future_tolerance(
            window_end,
            now,
            bounds.future_tolerance,
        ));
    }
    if origin == RecordOrigin::Live && now - window_end > bounds.live_past_tolerance {
        return Err(UsageCollectorError::covered_period_before_past_tolerance(
            window_end,
            now,
            bounds.live_past_tolerance,
        ));
    }
    Ok(())
}

/// The PDP action this entry is authorized against.
///
/// Reaching past the backfill window is the elevated case, and the action
/// is what makes it one: an operator grants `backfill` to an import job and
/// not to an ordinary emitter. Inside the window a backfilled entry needs
/// no grant a live entry does not.
#[must_use]
pub fn ingestion_action(
    bounds: &CoveredPeriodBounds,
    origin: RecordOrigin,
    now: OffsetDateTime,
    window_end: OffsetDateTime,
) -> &'static str {
    match origin {
        RecordOrigin::Live => usage_record::actions::CREATE,
        RecordOrigin::Backfill if now - window_end > bounds.backfill_window => {
            usage_record::actions::BACKFILL
        }
        RecordOrigin::Backfill => usage_record::actions::CREATE,
    }
}
```

Subtracting two `OffsetDateTime`s yields a `Duration` and cannot overflow: the
type's range is years -9999..=9999, and the difference of any two values in it
fits. Comparing durations rather than doing `now + tolerance` is why this needs no
`checked_add`. Say that in a comment — the next reader will reach for
`checked_add`.

`actions::BACKFILL` does not exist until Task 10. **Land Task 10 first if you
prefer**, or write `ingestion_action` in Task 10 and only
`enforce_covered_period_bounds` here. Do not stub the constant.

- [ ] **Step 5: Call it from the two ingestion paths**

In `create_usage_record_inner`, capture `now` once at the top, then between the
projection and the authorization:

```rust
        let now = OffsetDateTime::now_utc();
        let record = record.try_into_usage_record(origin)?;
        enforce_covered_period_bounds(&self.bounds, origin, now, record.window_end)?;
```

The order matters and is fixed by the spec's §3.8 pipeline: period validation
(step 3) precedes PDP authorization (step 5). It also has to, because Task 11's
action selection reads the same `window_end` the bound just accepted.

In `create_usage_records_inner`, capture `now` once for the whole batch — before
the projection loop — and apply the bound per input index inside that loop,
recording a rejection at its own slot and clearing `pdp_allowed[index]`, exactly
as the projection's own failure already does. A period rejection is a
per-submission failure, never a batch-level one.

- [ ] **Step 6: Write the service-level tests**

In `service_tests.rs`, using the module's own `input_record(tenant_id,
resource_id, idem)` builder, `service_with_counting_permit(...)`,
`HappyPathPlugin::new()` and `authenticated_ctx()`:

```rust
/// A submission whose covered period closed long enough ago that the live
/// past tolerance refuses it. Deliberately built from the same
/// `input_record` every other ingestion test uses, differing only in the
/// covered period — the bound reads nothing else.
fn stale_input_record(tenant_id: Uuid, resource_id: &str, idem: &str) -> CreateUsageRecord {
    let mut r = input_record(tenant_id, resource_id, idem);
    r.window_end = OffsetDateTime::now_utc() - time::Duration::days(30);
    r.window_start = r.window_end - time::Duration::hours(1);
    r
}

#[tokio::test]
async fn the_live_path_refuses_a_period_older_than_the_past_tolerance_and_names_the_route() {
    let plugin = HappyPathPlugin::new();
    let (service, _) = service_with_counting_permit(
        Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
        "test.bounds.live_past.v1",
    );

    let err = service
        .create_usage_record(
            &authenticated_ctx(),
            stale_input_record(Uuid::from_u128(0xC1), "rsc-stale", "idem-stale"),
        )
        .await
        .expect_err("a period 30 days old is beyond the 48-hour live tolerance");

    let UsageCollectorError::InvalidArgument { reason, detail, .. } = &err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert_eq!(*reason, ValidationReason::PastWindow);
    assert!(
        detail.contains("/usage-collector/v1/records/backfill"),
        "the rejection MUST name the route the entry belongs on: {detail}",
    );
}

#[tokio::test]
async fn the_live_path_refuses_a_withdrawal_over_a_period_older_than_the_past_tolerance() {
    // The failure mode of the whole slice, and the thing no test in the
    // tree pinned before this one. The bounds belong to the path, not to
    // the entry kind: an invalidation copies its target's period, so
    // withdrawing a closed month is refused on the live path exactly as a
    // fresh measurement of that month would be
    // (`cpt-cf-usage-collector-adr-backfill-isolation`).
    //
    // Getting this backwards is easy and silent. If the bound read the
    // arrival instant instead of the copied period, this submission would
    // be accepted and a correction of closed history would persist reading
    // `origin = live` — the exact gap the past bound exists to close.
    //
    // The rejection must arrive on the PERIOD, not on the target: assert
    // the reason is PastWindow rather than an invalidation-rule reason, so
    // the test still fails if the check moves after the target lookup.
    let plugin = FoldingPlugin::new();
    let target = /* store a record whose window closed 30 days ago */;
    let withdrawal = /* a faithful copy of `target` carrying an Invalidation
                        naming it, and its own idempotency key */;

    let (service, _) = service_with_counting_permit(
        Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
        "test.bounds.live_past.withdrawal.v1",
    );

    let err = service
        .create_usage_record(&authenticated_ctx(), withdrawal)
        .await
        .expect_err("a withdrawal of a closed month belongs on the backfill route");

    let UsageCollectorError::InvalidArgument { reason, detail, .. } = &err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert_eq!(*reason, ValidationReason::PastWindow);
    assert!(detail.contains("/usage-collector/v1/records/backfill"), "{detail}");
}

#[tokio::test]
async fn a_batch_rejects_only_the_entries_whose_period_is_out_of_bounds() {
    // Per-submission, at its own input index, with the surviving entries
    // still dispatched — the same posture the projection's own period
    // preconditions already have. A batch-level rejection here would make
    // one stale entry discard a whole import.
    let plugin = HappyPathPlugin::new();
    let tenant = Uuid::from_u128(0xC2);
    let input = vec![
        input_record(tenant, "rsc-ok", "idem-ok-0"),
        stale_input_record(tenant, "rsc-ok", "idem-stale-1"),
        input_record(tenant, "rsc-ok", "idem-ok-2"),
    ];
    // …set the plugin's per-record outcomes for the TWO entries that reach
    // it, not three: a period rejection never reaches the plugin.

    let results = service
        .create_usage_records(&authenticated_ctx(), input)
        .await
        .expect("a rejected period is per-entry, never batch-level");

    assert_eq!(results.len(), 3, "one result per input, in input order");
    assert!(results[0].is_ok());
    assert!(results[2].is_ok());
    let Err(UsageCollectorError::InvalidArgument { reason, .. }) = &results[1] else {
        panic!("index 1 must carry the rejection, at its own index");
    };
    assert_eq!(*reason, ValidationReason::PastWindow);
}
```

`FoldingPlugin` (`test_support.rs:1545`) with its `store` and `withdrawal_of`
helpers is how the existing invalidation tests set up a target; read
`service_tests.rs`'s existing withdrawal tests and build the pair the same way,
adjusting only the covered period. The two `/* … */` comments above are the two
places you must read the existing tests rather than invent a setup — a
hand-rolled withdrawal that is not a faithful copy would fail on
`InvalidationFieldMismatch` and the test would pass for the wrong reason.

- [ ] **Step 7: Run the tests, full bar, commit**

```bash
git add -A
git commit -s -m "feat(usage-collector): bound the live path's covered period on both sides

The Ingestion Gateway rejects a period ending beyond the future tolerance
on either route, and one ending beyond the past tolerance on the live
route, naming the backfill route in the rejection.

Both bounds read the end of the covered period and nothing else, so an
invalidation is bounded over the period it copies: withdrawing a closed
month is refused on the live path and belongs on backfill. No test pinned
that before this one."
```

---

## Task 10: The `backfill` action and its permission

**Files:**
- Modify: `usage-collector/src/domain/authz.rs` (+ `authz_tests.rs`)
- Modify: `usage-collector/src/gts/permissions.rs`
- Modify: `usage-collector/src/domain/ports/metrics.rs`

Slice 4 deleted `DEACTIVATE` from `actions` and deleted both the matching
`gts_instance!` block and its `EXPECTED_PERMISSION_IDS` entry. **You are walking
that path in reverse.** Two tests in `permissions.rs` compare the inventory count
to the list length and the two sets to each other, so editing one side fails both
— which is the good kind.

- [ ] **Step 1: Write the failing test**

Extend the existing `permissions.rs` inline test data — **this is the one place
the sibling-`*_tests.rs` ground rule does not apply**, because the file already
has an inline `mod tests` and restructuring it is out of scope. Add the fourth id
to `EXPECTED_PERMISSION_IDS`. The two existing tests then fail until the
`gts_instance!` block exists.

In `authz_tests.rs`, add a test asserting the four action constants are distinct
and spelled as expected — `create`, `get`, `list`, `backfill`.

- [ ] **Step 2: Run to verify they fail**

Expected: `all_uc_permissions_registered_in_inventory` and
`uc_permission_inventory_covers_every_expected_id` both fail on a count of 3
against 4.

- [ ] **Step 3: Add the action, the permission and the PDP op**

`domain/authz.rs`:

```rust
    pub mod actions {
        pub const CREATE: &str = "create";
        pub const GET: &str = "get";
        pub const LIST: &str = "list";
        /// Import or withdraw a covered period ending further back than the
        /// configured backfill window.
        ///
        /// The elevated grant of
        /// `cpt-cf-usage-collector-adr-backfill-isolation`. It is **not**
        /// the backfill route's action — an entry on that route whose
        /// period ends inside the window authorizes [`CREATE`], because it
        /// needs no privilege a live emission does not. This one is
        /// granted to an import job, so an emitter that finds a gap older
        /// than the window cannot close it on its own.
        pub const BACKFILL: &str = "backfill";
    }
```

`gts/permissions.rs`: a fourth `gts_instance!` with id
`gts.cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_backfill.v1`,
`action: usage_record::actions::BACKFILL.to_owned()`, display name
`"Backfill usage records beyond the backfill window"` — the display name is what
an operator reads in a role editor, so it should say what the grant actually
permits rather than repeat the verb.

`domain/ports/metrics.rs`: add `PdpOp::Backfill => "backfill"`. Check the doc
comment above `PdpOp` for a count of the operation set and fix it if it states
one.

- [ ] **Step 4: Run the tests, full bar, commit**

Both permission tests pass. Run `cargo nextest run -p cf-gears-usage-collector`
and confirm no *other* test asserted a count of three actions or three
permissions:

```bash
grep -rn --include='*.rs' -e 'actions::' -e 'PERMISSION_IDS' usage-collector/src | grep -i 'len()\|three\|3'
```

```bash
git add -A
git commit -s -m "feat(usage-collector): add the backfill action and its permission

The elevated grant a submission reaching past the configured backfill
window is authorized against. A backfill entry inside the window
authorizes create — it needs no privilege a live emission does not — so the
new action gates exactly the beyond-window case. PdpOp gains Backfill for
the operation label."
```

---

## Task 11: `Service::backfill_usage_records`

**Files:**
- Modify: `usage-collector/src/domain/service.rs`
- Test: `usage-collector/src/domain/service_tests.rs`,
  `usage-collector/src/domain/service_metrics_tests.rs`

- [ ] **Step 1: Write the failing tests**

The ADR's Confirmation section is the test list. Five of its six cases are
gear-level; the sixth is a concurrent load test and is out of scope with the
isolation it confirms.

**First, a new PDP double.** The existing fakes count `evaluate` calls
(`CountingTenantPermitResolver::calls()`) but discard the request, so none of
them can say *which action* was authorized — and this task's central property is
about actions. Add to `test_support.rs`, beside the other `AuthZResolverApi`
fakes:

```rust
/// PDP fake that permits everything and records the `action` of every
/// request it sees, in call order.
///
/// The counting fakes beside it answer "how many decisions" — this one
/// answers "which verb". Slice 5 is the first caller that varies the action
/// within one batch, so a test that only counts calls cannot tell a batch
/// authorizing `create` twice from one authorizing `create` and `backfill`.
#[derive(Debug, Default)]
pub struct ActionRecordingPermitResolver {
    actions: std::sync::Mutex<Vec<String>>,
}

impl ActionRecordingPermitResolver {
    #[must_use]
    pub fn new() -> Arc<Self> { Arc::new(Self::default()) }

    /// The actions authorized so far, sorted — the batch fan-out is
    /// `buffer_unordered`, so call order is not deterministic and asserting
    /// on it would produce an intermittently failing test.
    #[must_use]
    pub fn actions_sorted(&self) -> Vec<String> {
        let mut seen = self.actions.lock().expect("not poisoned").clone();
        seen.sort();
        seen
    }
}

#[async_trait]
impl AuthZResolverApi for ActionRecordingPermitResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.actions
            .lock()
            .expect("not poisoned")
            .push(request.action.name.clone());
        Ok(permit_scoped_to_request_tenant(&request))
    }
}
```

Read `EvaluationRequest`'s `action` field before writing that push — it is
`authz_resolver_sdk::Action` (`gears/system/authz-resolver/authz-resolver-sdk/src/models.rs:101`)
and the field holding the verb may not be called `name`. Reuse
`permit_scoped_to_request_tenant`, which `CountingTenantPermitResolver` already
uses, rather than composing a response by hand.

Then the tests:

```rust
#[tokio::test]
async fn the_backfill_route_stamps_every_accepted_entry_with_backfill() {
    // ADR confirmation case 5: an imported entry carries its origin marker
    // on every read path. This is the write half; dto_tests carries the
    // projection half.
    let plugin = HappyPathPlugin::new();
    let input = vec![stale_input_record(Uuid::from_u128(0xD1), "rsc", "idem-import")];
    // …set the plugin to echo back what it is handed, so the assertion is
    // about what the gateway stamped rather than about the fixture.

    let results = service
        .backfill_usage_records(&authenticated_ctx(), input)
        .await
        .expect("batch dispatch succeeded");

    let accepted = results[0].as_ref().expect("accepted");
    assert_eq!(accepted.origin, RecordOrigin::Backfill);
}

#[tokio::test]
async fn the_backfill_route_admits_the_period_the_live_path_rejected() {
    // ADR confirmation case 3, both halves in one test so the two can never
    // drift into agreeing with each other by accident: the live path
    // refuses the period naming the route, and the route admits it.
    let stale = stale_input_record(Uuid::from_u128(0xD2), "rsc", "idem-both");

    let live_err = service
        .create_usage_record(&authenticated_ctx(), stale.clone())
        .await
        .expect_err("beyond the live past tolerance");
    assert!(matches!(
        &live_err,
        UsageCollectorError::InvalidArgument { reason: ValidationReason::PastWindow, .. },
    ));

    let results = service
        .backfill_usage_records(&authenticated_ctx(), vec![stale])
        .await
        .expect("batch dispatch succeeded");
    assert!(results[0].is_ok(), "the route exists for exactly this period");
}

#[tokio::test]
async fn a_withdrawal_of_closed_history_is_refused_live_and_accepted_on_backfill() {
    // ADR confirmation case 4. Build the target with origin = Live and
    // withdraw it on the backfill route: `origin` records the path each
    // entry travelled and is NOT one of the fields the faithful-copy rule
    // compares, so a Live target and a Backfill withdrawal is a correct
    // pair rather than a mismatch. If the comparator ever started reading
    // `origin`, this test is what fails.
    …build the pair with FoldingPlugin as in Task 9's withdrawal test…

    assert!(live_attempt.is_err());
    let withdrawn = backfill_attempt[0].as_ref().expect("accepted");
    assert_eq!(withdrawn.origin, RecordOrigin::Backfill);
    assert_eq!(target.origin, RecordOrigin::Live);
}

#[tokio::test]
async fn one_backfill_batch_mixing_window_sides_authorizes_two_distinct_actions() {
    // AttributionTupleKey includes `action` in its hash/eq precisely so a
    // batch carrying records bound to different actions cannot collapse
    // onto a single PDP decision. Slice 5 is the first caller that mixes
    // them; before this test the property was structural but unexercised.
    //
    // Both entries share one attribution tuple — same tenant, same
    // resource, no subject — so the ONLY thing keeping them apart is the
    // action. If `action` were dropped from the key, the two would collapse
    // onto one decision and an entry reaching past the backfill window
    // would ride in on a `create` permit.
    let tenant = Uuid::from_u128(0xD4);
    let resolver = ActionRecordingPermitResolver::new();
    let plugin = HappyPathPlugin::new();
    let (service, _) = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .with_resolver(Arc::clone(&resolver) as Arc<dyn AuthZResolverApi>)
        .build(Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>, "test.backfill.mixed.v1");

    let inside = /* input_record with window_end 30 days ago  — inside 90d */;
    let beyond = /* input_record with window_end 120 days ago — beyond 90d */;
    // Same tenant and resource_ref, different idempotency keys.

    service
        .backfill_usage_records(&authenticated_ctx(), vec![inside, beyond])
        .await
        .expect("batch dispatch succeeded");

    assert_eq!(
        resolver.actions_sorted(),
        vec!["backfill".to_owned(), "create".to_owned()],
        "one batch, one attribution tuple, two actions — the PDP must see both",
    );
}
```

That last test is the one to write carefully, and the assertion has to be on the
recorded **actions**. An assertion that merely counts PDP calls would pass for
the wrong reason: two calls is also what a broken implementation produces if it
dedupes on something else, and one call is what the *correct* implementation
produces if both entries land on the same side of the window — so the fixture's
two windows must straddle it, and the test should fail loudly if they stop doing
so. Assert on the strings.

Also add to `service_metrics_tests.rs`: a backfill submission increments
`uc_ingestion_records_total` with `origin = backfill` and observes
`uc_ingestion_duration_seconds` with the same label, using the
`build_with_metrics` fixture and the `counter_sum_with_label` reader.

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Implement**

```rust
    /// Bulk historical import, isolated from live ingestion.
    ///
    /// Stamps `origin = backfill` and admits the covered periods the live
    /// past tolerance rejects. Validation is otherwise identical to
    /// [`Self::create_usage_records`] — the future tolerance included,
    /// because this route lifts the past bound and nothing else.
    ///
    /// It takes invalidation entries as well as measurements, mixed in one
    /// batch. A withdrawal of a period older than the live past tolerance
    /// belongs here rather than on the live path, so a correction of closed
    /// history reads as history
    /// (`cpt-cf-usage-collector-adr-backfill-isolation`).
    ///
    /// An entry whose covered period ends further back than the configured
    /// backfill window is authorized against
    /// [`usage_record::actions::BACKFILL`] instead of `CREATE`. One batch
    /// may mix the two.
    ///
    // TODO(`cpt-cf-usage-collector-nfr-workload-isolation`): this route
    // shares the live path's runtime, connection pool and fan-out budget.
    // The ADR makes workload isolation a gear-level obligation and it is
    // unimplemented; a bulk import can still degrade live ingestion p95.
    // Backend pool isolation is separately a plugin deployment obligation.
    // Confirmation is a concurrent load test against
    // `cpt-cf-usage-collector-nfr-throughput-profile`.
    ///
    /// # Errors
    ///
    /// The same variants as [`Self::create_usage_records`].
    pub async fn backfill_usage_records(
        &self,
        ctx: &SecurityContext,
        records: Vec<CreateUsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorError>>, UsageCollectorError> {
        self.create_usage_records_instrumented(ctx, records, RecordOrigin::Backfill)
            .await
    }
```

If Task 5 extracted the instrumented body as suggested, this is the whole
implementation and the two routes cannot drift. If it did not, extract it now
rather than copying — a second copy of the telemetry block is how the backfill
share stops being counted the day the live block changes.

Thread the action through the authorization. In `create_usage_record_inner`,
replace the hard-coded `usage_record::actions::CREATE` with
`ingestion_action(&self.bounds, origin, now, record.window_end)`. In
`create_usage_records_inner`, the same value goes into
`AttributionTupleKey::from_record(record, action)` — computed per record, since
one batch can carry both.

```bash
grep -rn --include='*.rs' -F 'actions::CREATE' usage-collector/src | grep -v tests
```

Check every non-test occurrence: the read paths use `GET` / `LIST` and must not
change, and `AttributionTupleKey`'s doc comment says "Today every batch caller
passes a constant (`usage_record::actions::CREATE`)" — **that sentence is now
false and must be replaced**, not softened. It is the exact shape of residue
slice 4 shipped: a claim that was reworded rather than retired.

- [ ] **Step 4: Run the tests, full bar, commit**

```bash
git add -A
git commit -s -m "feat(usage-collector): add the isolated backfill ingestion path

Service::backfill_usage_records stamps origin = backfill, keeps the future
tolerance, drops the live past tolerance, and authorizes each entry against
create or the backfill action according to the configured window. One batch
may mix both actions; AttributionTupleKey already kept such a batch from
collapsing onto one PDP decision, and there is now a test for it.

Workload isolation is not implemented — the route shares the live path's
runtime and fan-out budget. The obligation is recorded at the entry point
and in DIVERGENCES."
```

---

## Task 12: `backfill_usage_records` on the SDK trait

DESIGN §3.3 already declares it, with the reason it is on the trait at all: "it
is the only route reaching past the live past tolerance, and an in-process
emitter needs it to import history *and to withdraw it*. Confining it to REST
would turn every such correction into an operator escalation."

**Files:**
- Modify: `usage-collector-sdk/src/api.rs`
- Modify: `usage-collector/src/domain/local_client.rs`
- Test: `usage-collector/src/domain/local_client_tests.rs`

- [ ] **Step 1: Write the failing test**

In `local_client_tests.rs`, follow the delegation-test pattern the file already
uses for `create_usage_records`: the client forwards to the service and returns
what it returns, and the returned entries carry `origin = backfill`.

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Add the trait method**

In `api.rs`, after `create_usage_records`, with the doc from DESIGN §3.3
expanded:

```rust
    /// Bulk historical import, isolated from live ingestion.
    ///
    /// Stamps `origin = backfill` and admits the covered periods the live
    /// past tolerance rejects. Validation is otherwise identical to
    /// [`Self::create_usage_records`], the future tolerance included: this
    /// route lifts the past bound and nothing else.
    ///
    /// It is on this trait rather than on REST alone because it is the only
    /// route reaching past the live past tolerance, and a defect is
    /// normally found days later rather than hours later. An in-process
    /// emitter needs it to import history **and to withdraw it** — a
    /// withdrawal of a period older than the live past tolerance is
    /// refused there and belongs here. Confining it to REST would turn
    /// every such correction into an operator escalation
    /// (`cpt-cf-usage-collector-adr-backfill-isolation`).
    ///
    /// An entry whose covered period ends further back than the deployment's
    /// configured backfill window requires elevated authorization.
    async fn backfill_usage_records(
        &self,
        ctx: &SecurityContext,
        records: Vec<CreateUsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorError>>, UsageCollectorError>;
```

Implement it on `UsageCollectorLocalClient` by delegation, like its neighbours.

- [ ] **Step 4: Check for other implementors**

```bash
grep -rn --include='*.rs' -F 'impl UsageCollectorClientV1' . | grep -v target
```

Adding a required trait method breaks every implementor. Expect the local client
and possibly test doubles; the TimescaleDB plugin implements the *plugin* SPI,
not this trait, and must stay untouched — confirm with `git diff --stat`.

- [ ] **Step 5: Run the tests, full bar, commit**

```bash
git add -A
git commit -s -m "feat(usage-collector-sdk)!: put backfill on the client trait

DESIGN §3.3 declares it, because it is the only route reaching past the
live past tolerance and an in-process emitter needs it to import history
and to withdraw it. Confining it to REST would make every correction of
closed history an operator escalation.

BREAKING CHANGE: UsageCollectorClientV1 gains a required
backfill_usage_records method."
```

---

## Task 13: `POST /usage-collector/v1/records/backfill`

**Files:**
- Modify: `usage-collector/src/api/rest/routes/usage_records.rs` (+ `_tests`)
- Modify: `usage-collector/src/api/rest/handlers/usage_records.rs` (+ `_tests`)

- [ ] **Step 1: Write the failing tests**

In `routes/usage_records_tests.rs`, follow the existing registration-test pattern
and assert the route registers at the right path with operationId
`usage_collector.backfill_usage_records` and tag `Backfill`, and that it declares
both a `200` and a `207` response with `CreateUsageRecordsResponse`.

In `handlers/usage_records_tests.rs`, assert the handler returns `200` when every
entry is accepted and `207` when at least one is rejected — the same envelope
`POST /records` uses — and that accepted entries come back with
`"origin": "backfill"`.

Add one test that a request body carrying `origin` is refused, since
`CreateUsageRecordRequest` has `deny_unknown_fields` and the whole point is that
a caller cannot name its own path.

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Register the route and add the handler**

In `routes/usage_records.rs`, add a `BACKFILL_TAG: &str = "Backfill"` beside
`USAGE_RECORDS_TAG` and register:

```rust
    router = OperationBuilder::post(usage_collector_sdk::BACKFILL_ROUTE_PATH)
        .operation_id("usage_collector.backfill_usage_records")
        .summary("Bulk historical import, isolated from live ingestion")
        .description(
            "Identical validation and request shape to POST /records, differing in \
             three respects: every accepted entry is stamped `origin: backfill`, the \
             live path's past bound on the covered period does not apply because this \
             route exists for exactly the periods that bound rejects, and submissions \
             whose covered period ends further back than the configured backfill \
             window require elevated authorization. The route takes measurements and \
             invalidation entries alike, mixed in one batch.",
        )
        .tag(BACKFILL_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<dto::CreateUsageRecordsRequest>(openapi, "Usage-record import payload")
        .handler(handlers::handle_backfill_usage_records)
        .json_response_with_schema::<dto::CreateUsageRecordsResponse>(
            openapi,
            StatusCode::OK,
            "Every entry accepted or deduplicated",
        )
        .json_response_with_schema::<dto::CreateUsageRecordsResponse>(
            openapi,
            StatusCode::MULTI_STATUS,
            "At least one entry rejected; inspect each per-entry outcome",
        )
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);
```

**"three respects", not the contract's four.** The yaml's fourth is workload
isolation, which D4 defers — describing an isolation the gear does not provide
would be a published claim the code falsifies. Task 14 records the difference.

The handler is the existing `handle_create_usage_records` body with
`service.backfill_usage_records(...)` in place of `create_usage_records(...)`.
**Extract the shared body into one private function taking the service call as
the only variation** rather than duplicating ~50 lines of per-index bookkeeping,
`207`/`200` selection and result ordering. Two copies of that logic will diverge,
and the per-index correctness is exactly what a divergence would silently break.

Check whether the six `#[ignore]`d drift tests in
`routes/openapi_contract_tests.rs` reference an operation count or an endpoint
list — they compare the registry against `usage-collector-v1.yaml` and stay
ignored (slice 6 re-enables them), but if one holds a literal set it may need the
new operation added so slice 6 inherits a true list. **Leave them ignored either
way**, and do not touch `docs/api/api.json`.

- [ ] **Step 4: Run the tests, full bar, commit**

```bash
git add -A
git commit -s -m "feat(usage-collector): expose the backfill route on REST

POST /usage-collector/v1/records/backfill, taking the same
CreateUsageRecordsRequest as POST /records and sharing its per-entry
envelope. The handler body is shared with the live route so the two cannot
drift on per-index bookkeeping.

The published description names three differences from POST /records, not
the contract's four: workload isolation is deferred and claiming it would
be false."
```

---

## Task 14: Documentation debt, divergences, and the final sweep

No production code changes. This task exists because slice 4's sweep corrected
thirteen wrong cardinalities and then shipped a wrong count in its own summary
line.

**Files:**
- Modify: `DIVERGENCES.md`
- Modify: `usage-collector/src/config.rs`, `usage-collector/src/lib.rs`,
  `usage-collector/src/config_tests.rs` (the ADR-by-number citations)

- [ ] **Step 1: Strike divergence 9(c) and narrow entry 10**

Entry 9(c) said `origin` is not emitted on `uc_ingestion_records_total` because
the slice that builds it has not run. It has now. Strike the sub-item and adjust
entry 9's own framing — its opening says "Three problems, all on one table row",
which becomes two.

**Then re-read entry 9's header, its body and its "Why the code is right"
paragraph against each other.** A sweep's header contradicting its own
justification section, written at different times and never re-read together, is
a documented failure of this branch.

Entry 10's response-side paragraph lists `accepted_at`, `acceptance_sequence` and
`origin` as required-but-absent. `origin` is now present. Remove it from that
list and from the "what the code does, and why it is where it is" bullet that
attributes it to slice 5 — leaving a bullet saying slice 5 owns it, after slice 5
shipped it, is a correction that lands in one place and not the other. Entry 10
stays open: `value`/`quantity` and the two acceptance fields are untouched.

Also update the entry's "Pinned in both directions" paragraph, which says the two
`dto_tests` key-set assertions "fail the day either gap closes". One gap closed
and the assertions were updated; say which and when.

- [ ] **Step 2: Add the new divergences**

Four candidates. Verify each before writing it — an entry that is wrong costs
more than an entry that is missing.

1. **`uc_ingestion_records_total`'s period-bound classification.** DESIGN §3.11.5
   says "A period-bound rejection is `validation` for either `entry_type`". The
   gear emits no `validation` category at all — `classify_record_error`'s
   catch-all puts `FutureWindow` / `PastWindow` on `semantics_violation`, for the
   same reason entry 9(b) already gives about the other three. This is a concrete
   instance of 9(b) rather than a new entry; **fold it into 9(b)** and say the
   period bounds are the case DESIGN's sentence names.
2. **The published backfill description claims workload isolation the gear does
   not provide.** The yaml enumerates four differences from `POST /records` and
   the registered route names three. `cpt-cf-usage-collector-nfr-workload-isolation`
   is an unmet gear obligation. **Load-bearing**: an operator reading the contract
   would believe a bulk import cannot breach live-path SLOs, and it can.
3. **`accepted_at` and `acceptance_sequence` remain claimed by no slice.**
   DESIGN §3.1 has the gear stamp `accepted_at` and the storage plugin assign
   `acceptance_sequence` strictly monotonic per `(tenant_id, gts_type_id)`, and
   `models.rs` documents the `LATEST` fold's tie-break against a field the record
   does not carry. **This blocks a conformant storage plugin** — there is no field
   to assign and none for the fold to read. `origin` was their sibling in the
   server-assigned group and it shipped; they did not. Re-flag it; do not fill it.
4. **`uc_pdp_duration_seconds` names a "nine-value set"** for its `operation`
   label while `uc_pdp_failures_total` enumerates seven and `PdpOp` now has five.
   Check this before writing it — it is a document-internal cardinality
   disagreement, and if it is real it is the same class as entries 4 and 9.

Update the file's own header, which begins "Eleven places where…" and carries a
"nine of the eleven" breakdown. **Both numbers change.** Recount them by reading
the section headings, not by adding to the old total.

- [ ] **Step 3: Record the handoff's own wrong counts**

Add a short section, or extend the existing "things the branch owes someone
else", noting for the next slice's author:

- The handoff said "four config keys"; there are **three**. The spec's §3.10
  table lists six, three of which already existed.
- The handoff said the DESIGN rework dropped the `flow` / `algo` / `dod` /
  `state` / `component` marker categories and that a resolve-gate would fail
  crate-wide. **The first four resolve** — in `docs/features/*.md` and
  `DECOMPOSITION.md`, which is where this convention's ids live; and `component`
  ids still exist in DESIGN and are not a marker category in this code at all.
  Only the two compensation ids were genuinely stale, and Task 1 removed them.

- [ ] **Step 4: Fix the four ADR-by-number citations**

`config.rs`'s module doc, `lib.rs` (twice) and `config_tests.rs` cite `ADR-0012`
for a claim about the usage-type catalog being plugin-owned. That number now
resolves to `cpt-cf-usage-collector-adr-backfill-isolation` — this slice's ADR,
which says nothing of the kind.

`config.rs` already explains why repointing the id alone would be worse than
leaving it: it would preserve a false claim under a correct reference. So
**rewrite the sentence**, do not repoint it. The catalog is not plugin-owned;
`types-registry` owns every declaration and this gear registers no usage-type
surface. Cite `cpt-cf-usage-collector-adr-registry-owned-typing`, which is the
decision that actually holds, and delete the four-paragraph note explaining why
the stale citation was left standing — it is the record of a debt this step pays.

```bash
grep -rn --include='*.rs' 'ADR-00' usage-collector/src usage-collector-sdk/src
```

Expected after this step: no hits. Any that remain are a fifth site the handoff
did not count.

- [ ] **Step 5: Sweep for cardinalities this slice invalidated**

Wrong counts are this branch's most persistent residue, and the identifier
sweeps cannot see them. Search for every number this slice moved:

```bash
grep -rn --include='*.rs' -iE '\b(two|three|four|five|six|seven|eight|nine)\b' \
  usage-collector/src usage-collector-sdk/src | grep -iE 'action|permission|field|label|key|bound|route|variant|op '
```

Known movers: `actions` 3 → 4; permission instances 3 → 4; `PdpOp` 4 → 5;
`uc_ingestion_records_total` labels 3 → 4; `uc_ingestion_duration_seconds`
labels 0 → 1; `UsageRecordQuery` fields 7 → 8 filterable;
`KEYSET_SAFE_RECORD_FIELDS` 6 → 7; `UsageCollectorConfig` keys 5 → 8;
`UsageCollectorClientV1` methods 5 → 6; REST operations 4 → 5;
`ValidationReason` variants +2; `UsageRecord` fields +1.

For each, grep for prose stating the old number **and for prose enumerating the
old set without a number** — a list that is now one short is the same defect and
is invisible to a numeral search.

- [ ] **Step 6: Final verification**

```bash
cargo check --workspace --all-targets
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk \
  -p cf-gears-noop-usage-collector-plugin --no-fail-fast
cargo clippy --workspace --all-targets --all-features
cargo +nightly fmt
cargo doc --no-deps -p cf-gears-usage-collector-sdk -p cf-gears-usage-collector
git diff --stat main
```

- 6 tests still skipped, and they are the same six `#[ignore]`d drift tests.
- `cargo doc` host warnings still **35**, none naming a symbol this slice created.
- `git diff --stat` shows **zero files** under
  `plugins/timescaledb-usage-collector-plugin/`, and no change to
  `usage-collector-v1.yaml`, `DESIGN.md`, `DECOMPOSITION.md`, `docs/features/*`,
  `docs/api/api.json`, or any `Cargo.toml` version.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -s -m "docs(usage-collector): record what slice 5 closed and what it did not

DIVERGENCES 9(c) is struck and entry 10 loses its origin half; both keep
what is still open. Four new observations, including a published contract
claiming a workload isolation the gear does not implement.

The four ADR-0012-by-number citations are rewritten rather than repointed:
the number now resolves to backfill-isolation while the sentence claimed
the usage-type catalog is plugin-owned, and repointing alone would have
preserved a false claim under a correct reference.

Also records two wrong counts in the slice-5 handoff itself: three config
keys rather than four, and four marker categories that resolve against
docs/features rather than being dropped."
```

---

## Verification bar

Every task ends with this, scoped to the three usage-collector packages. Never
run a workspace-wide **test** build — `target/` reaches ~110 GB.

```bash
cargo check --workspace --all-targets
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk \
  -p cf-gears-noop-usage-collector-plugin --no-fail-fast
cargo clippy --workspace --all-targets --all-features
cargo +nightly fmt
cargo doc --no-deps -p cf-gears-usage-collector-sdk -p cf-gears-usage-collector
```

`cargo doc` is on the bar because it catches intra-doc links to moved or deleted
items that clippy, the tests and rustfmt all pass over. It earned its place in
slice 4. Clippy is deny-warnings in CI.

**Baseline: 648 passed, 6 skipped.** The 6 are the `#[ignore]`d OpenAPI drift
tests; leave them skipped. Task 3 is the one task that legitimately leaves the
host crate not compiling; Task 4 owes the first host run.

---

## Self-review against the slice

**Spec coverage.** `RecordOrigin` (T2, T3); stamped from the path and never
caller-supplied (T3, T4, T13); applying to invalidations (T9, T11); on every read
path (T4, T6); `backfill_usage_records` on the SDK trait (T12) and
`POST /records/backfill` (T13); the live two-sided bound (T9) with the past
rejection naming the route (T7); the backfill window and its elevated
authorization (T8, T10, T11); the config keys (T8); the metric label (T5); the
filter field (T6). Five of the ADR's six confirmation cases have tests (T9, T11);
the sixth is a concurrent load test and is out of scope with the isolation it
confirms.

**Deliberately not built:** gear-level workload isolation (D4), `accepted_at`,
`acceptance_sequence`, the `value` → `quantity` rename, the usage feed,
reconciliation, ingestion quotas, the declaration mirror, and everything in
slice 6's list.

**Known ordering couplings.** T8 references `CoveredPeriodBounds` from T9 and T9
references `actions::BACKFILL` from T10; each is flagged in place with permission
to reorder. Nothing else is coupled.

**Two tasks are much larger than their neighbours**, and a reviewer should expect
that rather than reading it as scope creep:

- **T4** touches ~48 `UsageRecord` construction sites across three packages,
  because adding a field is a compile error at every *literal* one. That is the
  guard working, not a design problem — but note the guard has a hole: a
  **struct-update** site compiles unchanged and silently inherits the base
  record's `origin`. There are **five** of them, not the two this plan first
  listed: `test_support.rs` (`withdrawal_of`), `authz_tests.rs`
  (`record_with_tenant`), `service_tests.rs` ×2 (`target_row`), and
  `service_metrics_tests.rs` (`sample_target_row`). They need reading, not just
  building — a third miscount in this plan, found by the Task 4 implementer.
- **T9** re-bases ~75 ingestion-path assertions off a 1970 covered period before
  it enforces anything, in a separate commit. Its enforcement commit should be
  small; if it is not, the re-base leaked into it.

**Two new test doubles are specified rather than assumed:**
`ActionRecordingPermitResolver` (T11) and `recent_window()` (T9). Both exist
because the current fakes cannot express what this slice needs to assert — the
first records which verb was authorized, the second gives ingestion fixtures a
covered period a live emitter could plausibly have closed.
