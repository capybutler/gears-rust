# Usage Collector — Errors and the Contract Gate (Slice 6) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring the usage-collector's error vocabulary to its final shape, turn the
OpenAPI drift gate back on against an explicit not-yet-implemented set, and build
the DESIGN §3.3 plugin contract suite as a reusable public harness with a
conforming in-memory reference plugin and proof that each check discriminates.

**Architecture:** Six independent seams, ordered so the tree compiles at every
commit. The wire spellings are pinned first (nothing may move an unpinned
constant). Then two small code fixes let the drift gate turn on. Then the cursor
codes move from gear-owned to upstream-projected, the `NotFound` category grows a
typed reason, and finally the contract suite lands in `usage-collector-sdk` as a
feature-gated public module any plugin crate can run against itself.

**Tech Stack:** Rust 2024, `cargo nextest`, `clippy::pedantic` (deny at workspace
level), `toolkit-odata`, `toolkit-canonical-errors`, `thiserror`, `async-trait`.

---

## Decisions already made — do not re-litigate

These five were put to the spec owner before this plan was written. They are
settled. If a task seems to contradict one, the task is wrong, not the decision.

1. **`UsageCollectorPluginError` stays at five variants.**
   `CursorBeyondRetention` lands with the usage feed, not here. Slice 6 records
   the deliberate five-of-six in `DIVERGENCES.md`. **Do not add the variant.**

2. **`INVALID_CURSOR` and `FILTER_MISMATCH` leave this gear entirely** — both the
   `pub const` and the `ValidationReason` variant. Spec §3.13 says `toolkit_odata`
   owns them. The gear will surface them by propagating the upstream error.
   This is a **breaking change** to a public, `#[non_exhaustive]` wire vocabulary
   and takes a `!` and a `BREAKING CHANGE:` trailer. `ORDER_WITH_CURSOR` is
   already absent — do not add it.

3. **`UsageCollectorError::NotFound` grows a typed reason** so
   `classify_record_error` can route an unresolvable `invalidates` to
   `invalidation_rule` without substring-matching a caller-facing string. This
   closes `DIVERGENCES.md` entry 9(a) in code.

4. **`accepted_at` and `acceptance_sequence` are not slice 6's.** Do not add
   them. `latest-tie-break` stays unwritable and is recorded as blocked, naming
   what it needs.

5. **The contract suite is a public reusable harness with a conforming in-memory
   reference plugin**, plus deliberately non-conforming mutants proving each
   check discriminates. It is **not** an integration test in `tests/`, and it is
   **not** run against the noop plugin as its subject — see the next section.

### Why the noop plugin cannot be the suite's subject

The spec (§6) and the slice-6 handoff both say the suite is "scaffolded in
`usage-collector-sdk` against the noop plugin." **That is not implementable.**
`plugins/noop-usage-collector-plugin/src/plugin.rs` persists nothing:
`create_usage_record` echoes its input, `get_usage_record` always returns
`UsageRecordNotFound`, `list_usage_records` always returns an empty page, and
the fold always returns zero buckets. All five writable contract tests are
behavioural over stored rows, so noop fails every one of them.

The suite therefore needs a conforming subject, and this plan builds one. Noop
stays exactly as it is — **do not "fix" the noop plugin to pass the suite.** Its
whole point is to be a null backend for host-binding development.

---

## Ground rules — every one of these cost a review cycle in slices 1-5

### 1. Name the mutation, or it is not a test

**Before accepting any test — one this plan prescribes included — state the
one-token edit to production code that makes it red.** If you cannot name one,
the test is decoration. Seven prescribed checks in slice 5's plan could not fail.
The recurring shapes:

- An assertion that holds whether or not the label under test exists.
- **A negative assertion satisfied by an empty message.** `!detail.contains("x")`
  passes against `""`. **A negative assertion needs a positive anchor** — assert
  what the message *does* say in the same test.
- A test asserting `Err` where the value also trips a *different* rule whose
  message names the same key. Assert **which** rejection fired, by typed variant.
- A test claiming to catch a rule about a period's *length* against a function
  that only ever receives one instant.

### 2. Two layers each tested against themselves is not a test of the seam

The sharpest defect in slice 5 was a REST route's binding to its own handler:
route tests checked path/operationId/tag/schema, handler tests called the handler
directly, and a one-token edit between them was invisible with 700 tests green.

**This is exactly the risk of the contract suite in Tasks 7-11.** A suite that
runs a reference plugin against assertions derived from that same plugin proves
nothing. Task 11 exists solely to close this, and it is not optional.

### 3. Counting things

- **A count is verified by a grep whose pattern you have argued cannot miss a
  spelling** — not by having run a grep.
- **Never post-filter `grep -rn` output on digit patterns.** It matches grep's own
  line numbers and silently drops every hit on a line >= 10.
- **A count can be spelled with no number at all.** `(create / get / list /
  aggregate)` one operation short is the same defect and no numeral search finds
  it. **Grep for the members, not the cardinality.**
- **`cargo doc`'s warning count: read the "generated N warnings" line.**
  `grep -c '^warning:'` over-counts by one, because rustdoc's summary line begins
  with `warning:` and counts itself. Two reviewers, an implementer and the
  controller all got this wrong in slice 5, in both directions.

### 4. A claim that outlives the code

**When your change falsifies a sentence, replace the sentence — do not reword
it.** Then grep for it in any wording, because it is usually in three places.
Slice 5 committed a note saying a snippet did not compile while leaving the
uncompilable snippet standing in the step a reader would copy.

**Rustdoc drops `//` inside a `///` block.** A retraction written as a `//`
comment in a doc block is invisible in generated docs and in the item summary
list. If a doc must not claim something, the retraction is a `///` paragraph.

### 5. Five ways a falsification lies to you

All five produce "mutation survived", which argues for deleting a working test.

1. **`mv` preserves mtime**, so cargo skips the rebuild. Use `cp`, then `touch`,
   and confirm a `Compiling cf-gears-…` line.
2. **A relative path in a mutation script edits nothing** after a working-directory
   reset — and the `Compiling` line still appears. Use absolute paths and **grep
   the mutated line** before believing any pass.
3. **A count under a nextest `-E` filter is a lower bound.** Use `--no-fail-fast`,
   unfiltered, for any number you report.
4. **`git checkout` restores from HEAD**, discarding uncommitted work. **Restore
   from a `cp` snapshot, never from git.** There is uncommitted work in this tree.
5. **Someone else's restore script.** Keep scratch in the session scratchpad.

### 6. Process

- **One worker mutation-tests the tree at a time.**
- **Do not commit while an implementer subagent is working.** An amend lands on
  the wrong commit. This happened twice in slice 5, both times because the
  *controller* committed documentation mid-task. Batch doc commits between tasks.

  **It happened a third time in slice 6, during Task 2, and the same way.** The
  controller committed a plan note after the implementer had reported DONE, then
  resumed that same implementer with review fixes and told it to
  `git commit --amend`. By then the implementer's commit was no longer `HEAD` —
  the controller's docs commit was — so the amend absorbed the docs commit and
  replaced its message. Nothing was lost: the implementer noticed a six-file
  `--stat` where five were expected, reset the stray path out of the index and
  re-amended. But the branch ended up carrying two commits under one subject
  line, and the controller's note back in the working tree, uncommitted.

  So the rule has a second half, and it is the half that was missing:
  **before telling an implementer to amend, confirm its commit is still `HEAD`.**
  If you have committed on top of it, say "make a new commit" instead — an amend
  instruction is only safe against a tip the implementer still owns.
  `git log --oneline -1` before writing the message costs nothing.

  The corollary, learned in the same minute: **a commit is not made because the
  `git commit` ran.** The `python` heredoc that was supposed to write this very
  paragraph aborted on a stale anchor, and the `git add && git commit` after it
  in the same shell ran anyway — producing a commit whose message described an
  edit the commit did not contain. Chain the edit to the commit (`&&`), or print
  `git show --stat` and read it.
- **Demand per-test verdicts on deletions and repointings.** "Deleted the failing
  test" and "deleted the test whose question no longer exists" look identical in
  a diff.
- **Do not run a workspace-wide test build.** `target/` reaches ~110 GB and fills
  the disk. Scope every run with `-p`.
- **Cite ADRs by `cpt-cf-usage-collector-adr-*` id, never by number.** Slice 5
  paid off the last five by-number citations. Keep
  `grep -rn 'ADR-00' --include='*.rs' gears/system/usage-collector/ | grep -v timescaledb`
  returning nothing.
- **The host crate is one compilation unit.** Its test binary builds nothing while
  any compile error remains anywhere in the crate. Tasks 4 and 5 both touch it;
  **Task 4 owes the first in-place run.**
- **Markers that resolve stay.** The `@cpt-flow:` / `@cpt-algo:` / `@cpt-dod:` /
  `@cpt-state:` ids live in `docs/features/*.md` and `DECOMPOSITION.md`, not in
  `DESIGN.md`, and they resolve. 472 marker lines today. Zero name a
  compensation id. Do not strip any.

---

## Repository facts, measured on 2026-09-09

Re-run anything you are about to depend on. These were true when this plan was
written; each command is given because the number is load-bearing.

| Fact | Value | Command |
| --- | --- | --- |
| Baseline | **701 passed, 6 skipped** | the nextest line in "Verification bar" |
| The 6 skipped | the `#[ignore]`d drift tests | `grep -c '#\[ignore = ' usage-collector/src/api/rest/routes/openapi_contract_tests.rs` |
| yaml operations | **7** | `grep -c 'operationId:' docs/usage-collector-v1.yaml` |
| registered operations | **5** | `grep 'operation_id("' usage-collector/src/api/rest/routes/usage_records.rs` |
| `UsageCollectorPluginError` | **5** variants | `sed -n '/pub enum UsageCollectorPluginError/,/^}/p' usage-collector-sdk/src/error.rs` |
| `ValidationReason` | 16 variants + `Unknown` | `sed -n '/pub enum ValidationReason/,/^}/p' usage-collector-sdk/src/reason.rs` |
| `reason.rs` wire constants | **18** | `grep -c 'pub const' usage-collector-sdk/src/reason.rs` |
| `RecordErrorCategory` | **8** emitted values | `sed -n '/enum RecordErrorCategory/,/^}/p' usage-collector/src/domain/ports/metrics.rs` |
| `acceptance_sequence` / `accepted_at` | **0 fields**, 7 prose mentions | `grep -rn 'acceptance_sequence\|accepted_at' --include='*.rs' usage-collector-sdk/src usage-collector/src` |
| `cargo doc` warnings | host **35**, SDK **0** | read the "generated N warnings" line |

> The handoff said `acceptance_sequence` had "two hits, both inside doc
> comments". It has seven now, still all comments and still zero fields. The
> conclusion held; the count did not. Re-measure.

**All six drift tests fail on exactly one cause today**, verified by running them
with `--run-ignored all`:

```
GET /usage-collector/v1/feed: documented in usage-collector-v1.yaml but no
route registers it (see `register_usage_record_routes`)
```

**With the two unimplemented operations excluded, three real drifts remain** —
measured, not predicted, by patching the exclusion in a scratch copy and running:

1. `POST /records/aggregate` request body: registered `QueryAggregatedUsageRecordsRequest`
   vs documented `AggregationRequest`. **Task 2 fixes this in code.**
2. `every_registered_component_is_documented`: the same name, as a component.
   **Task 2 fixes this too.**
3. `GET /records` parameters: the gear registers `$top` and `metadata.<key>`,
   which the yaml documents **neither** of. The yaml is out of scope, so
   **Task 3 records this as a self-cleaning named gap** and files it as the
   seventeenth `DIVERGENCES.md` entry.

Everything else passes once feed and reconciliation are excluded.

---

## Authoritative documents

- `gears/system/usage-collector/docs/DESIGN.md`
  - §3.3 plugin-error table, lines 1214-1233 (six variants — we ship five, by decision 1)
  - §3.3 plugin contract-test table, lines 1105-1116 (seven tests — five writable)
  - §3.3 SPI trait, lines ~1015-1080 (seven methods — the gear implements five)
  - §3.11.5 `uc_ingestion_records_total`, line ~1779
- `gears/system/usage-collector/docs/usage-collector-v1.yaml` — the document the
  drift tests compare against
- Spec: `docs/superpowers/specs/2026-09-06-usage-collector-gateway-registry-owned-typing-design.md`
  — **§3.13 is the reason-vocabulary target**
- `DIVERGENCES.md` at the repo root
- Slice 5's plan: `docs/superpowers/plans/2026-09-08-usage-collector-origin-and-backfill.md`

**Do NOT implement from these — they are stale:** `docs/DECOMPOSITION.md` and
`docs/features/*.md`. They describe the pre-slice-4 compensation model. They are
**not inert**: they are where the traceability marker ids resolve, which is why
they cannot simply be deleted.

---

## Out of scope

- **The usage feed and reconciliation.** Named as not-yet-implemented; not built.
- **`accepted_at` / `acceptance_sequence`** (decision 4).
- **`CursorBeyondRetention`** (decision 1).
- **`DIVERGENCES.md` entry 9(b)'s label rename.** `FUTURE_WINDOW` / `PAST_WINDOW`
  stay on `semantics_violation`. Giving them a `validation` category would move a
  metric label an operator's dashboard already groups by, and entry 9 argues the
  code is right. Task 12 records it; **do not rename a `RecordErrorCategory`
  value in this slice.**
- **Editing `usage-collector-v1.yaml`, `DESIGN.md`, `DECOMPOSITION.md`,
  `docs/features/*`.**
- **Gear-level workload isolation** (entry 12) and **`AggregationDimension`
  growing `Origin`** (entry 15).
- **The TimescaleDB plugin.** Not a workspace member, has not compiled since
  slice 4. **`git diff --stat` must show zero files from it.**
- **`docs/api/api.json`.** Stale; regenerating it needs `make openapi` plus a
  `breaking-api-acknowledged` label decision. It belongs to whoever opens the PR.
- **Any crate version bump.**

---

## The working tree

Four items were already uncommitted when slice 5 began and still are.
**Leave them exactly as they are, and never `git add -A`:**

```
 D docs/superpowers/plans/2026-09-07-usage-collector-record-model-handoff.md
?? NEXT-SLICE.md
?? SLICE4.md
?? SLICE5.md
?? SLICE6.md
```

`SLICE6.md` joins them. The deletion predates slice 5 and is not yours to restore
or to commit.

---

## Verification bar

Run from the repository root. **Never workspace-wide for tests.**

```bash
cargo check --workspace --all-targets
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk \
  -p cf-gears-noop-usage-collector-plugin --no-fail-fast
cargo clippy --workspace --all-targets --all-features
cargo +nightly fmt
cargo doc --no-deps -p cf-gears-usage-collector-sdk -p cf-gears-usage-collector
```

The SDK gains a `contract` feature in Task 7, so from then on also run:

```bash
cargo nextest run -p cf-gears-usage-collector-sdk --features contract --no-fail-fast
cargo clippy -p cf-gears-usage-collector-sdk --all-targets --features contract
```

**Final state must show 0 skipped.** Baseline is 701 passed / 6 skipped; the 6 are
the drift tests and Task 3 un-skips all six.

`cargo doc` is on the bar because it catches intra-doc links to moved or deleted
items that clippy, the tests and rustfmt all pass over. The host emits **35**
warnings and the SDK **0**. Confirm your work adds none — an intra-doc link from
a public doc to a `pub(crate)` item adds one, and that happened three times in
slice 5.

Clippy is deny-warnings in CI and `clippy::pedantic` is deny at workspace level.
Tests live in a sibling `*_tests.rs` file with a
`#[cfg(test)] #[cfg_attr(coverage_nightly, coverage(off))] #[path = "..."]` hook,
never an inline `mod tests` — **except `src/gts/permissions.rs`**, which has a
pre-existing inline one. Extend it if you must; do not restructure it and do not
copy the pattern. Commits are Conventional Commits with a `Signed-off-by`
trailer; a breaking change takes a `!` and a `BREAKING CHANGE:` trailer. No
attribution lines.

---

## File structure

Paths are relative to `gears/system/usage-collector/` unless they start with a
repository-root name.

**Created:**

| File | Responsibility |
| --- | --- |
| `usage-collector-sdk/src/contract.rs` | Public, feature-gated contract-suite harness: one `async fn` per DESIGN §3.3 check plus `run_all`, generic over `UsageCollectorPluginV1`. The thing a plugin crate calls. |
| `usage-collector-sdk/src/contract/reference.rs` | `InMemoryReferencePlugin` — a conforming backend, and the suite's subject. Also the scope evaluator. |
| `usage-collector-sdk/src/contract_tests.rs` | Runs `run_all` against the reference plugin, and runs each check against a deliberately non-conforming mutant to prove it discriminates. |

**Modified:**

| File | Change |
| --- | --- |
| `usage-collector-sdk/src/reason.rs` | Delete two constants + two variants; add `NotFoundReason`. |
| `usage-collector-sdk/src/reason_tests.rs` | `stringify!` spelling table; the two removed codes fall through to `Unknown`. |
| `usage-collector-sdk/src/error.rs` | New `CursorRejected` variant + constructors; `NotFound` grows `reason`. |
| `usage-collector-sdk/src/lib.rs` | Export `NotFoundReason` and the `contract` module. |
| `usage-collector-sdk/Cargo.toml` | `contract` feature. |
| `usage-collector/src/domain/query.rs` | Four cursor-rejection raise sites re-pointed. |
| `usage-collector/src/domain/service.rs` | `classify_record_error` / `classify_query_result`. |
| `usage-collector/src/domain/error.rs` | `DeclarationNotFound` lift sets `reason`. |
| `usage-collector/src/infra/sdk_error_mapping.rs` | Lift `CursorRejected` through `toolkit_odata`. |
| `usage-collector/src/api/rest/dto.rs` | Rename the aggregate request DTO. |
| `usage-collector/src/api/rest/routes/openapi_contract_tests.rs` | The gate. |
| `DIVERGENCES.md` (repo root) | Entry 9 rewrite, §D struck, three new entries. |


---

## Task 1: Pin every wire spelling before the vocabulary moves

`DIVERGENCES.md` §D: `reason_tests.rs` walks a `(constant, variant)` table
through `from_wire` / `as_wire`, but **both sides of every assertion read the same
constant**. Change `FUTURE_WINDOW`'s *value* to `"FUTURE_WINDOWX"` and every test
still passes, while every client matching the published spelling breaks. All 18
constants have `value == identifier`, so one `stringify!` table pins every
spelling at once.

This lands **first**, before Tasks 4-6 move the vocabulary. Pinning after the move
would pin whatever the move produced.

**Files:**
- Modify: `gears/system/usage-collector/usage-collector-sdk/src/reason_tests.rs`

- [ ] **Step 1: Write the failing test**

Append to `usage-collector-sdk/src/reason_tests.rs`:

```rust
/// Pins the *value* of every wire constant against its own identifier.
///
/// `DIVERGENCES.md` §D: the round-trip tables above prove `from_wire` and
/// `as_wire` are inverses and nothing else, because both sides of every
/// assertion read the same constant. Changing a constant's value leaves
/// them green while every client matching the published spelling breaks.
///
/// Every code in this module is `SCREAMING_SNAKE` and identical to its
/// identifier, which is what makes a single `stringify!` table able to pin
/// all of them. A future code that deliberately departs from that rule
/// belongs in an explicit second table with the departure argued in a
/// comment — not silently omitted from this one.
#[test]
fn every_wire_constant_spells_its_own_identifier() {
    macro_rules! pin {
        ($($name:ident),+ $(,)?) => {
            [$((stringify!($name), $name)),+]
        };
    }

    let pinned = pin![
        SEMANTICS_VIOLATION,
        VALIDATION,
        METADATA_VALIDATION,
        UNKNOWN_METADATA_KEY,
        INVALID_BASE_GTS_ID,
        INVALID_METADATA_FIELDS_EMPTY_STRING,
        INVALID_METADATA_FIELDS_INVALID_KEY,
        INVALID_METADATA_FIELDS_DUPLICATE,
        AGGREGATION_RESULT_TOO_LARGE,
        INVALID_CURSOR,
        FILTER_MISMATCH,
        INVALIDATION_REFERENCE_INCOMPLETE,
        INVALIDATION_TARGET_NOT_RECORD,
        INVALIDATION_FIELD_MISMATCH,
        FUTURE_WINDOW,
        PAST_WINDOW,
        IDEMPOTENCY_CONFLICT,
        ALREADY_INVALIDATED,
    ];

    for (identifier, value) in pinned {
        assert_eq!(
            value, identifier,
            "the wire code `{identifier}` must spell its own identifier: it is \
             published in usage-collector-v1.yaml and matched by clients, so a \
             changed value is a silent wire break",
        );
    }

    // The table must cover the module, not a subset of it. A constant added
    // without a row here would otherwise be unpinned and invisible.
    assert_eq!(
        pinned.len(),
        18,
        "every `pub const` in reason.rs needs a row: run \
         `grep -c 'pub const' usage-collector-sdk/src/reason.rs`",
    );
}
```

- [ ] **Step 2: Run it and confirm it passes, then confirm it can fail**

```bash
cargo nextest run -p cf-gears-usage-collector-sdk \
  -E 'test(every_wire_constant_spells_its_own_identifier)'
```

Expected: PASS.

**Now falsify it.** This is the mutation that makes it red — a value change that
the pre-existing round-trip table cannot see:

```bash
SCRATCH="$(mktemp -d)"
SDK=/Users/binarycode/code/virtuozzo/gears-rust/gears/system/usage-collector/usage-collector-sdk/src
cp "$SDK/reason.rs" "$SCRATCH/reason.rs"
sed -i '' 's/pub const FUTURE_WINDOW: &str = "FUTURE_WINDOW";/pub const FUTURE_WINDOW: \&str = "FUTURE_WINDOWX";/' "$SDK/reason.rs"
grep -n 'FUTURE_WINDOWX' "$SDK/reason.rs"   # MUST print a line — if not, the sed missed
cargo nextest run -p cf-gears-usage-collector-sdk -E 'test(reason_tests)' --no-fail-fast
```

Expected: `every_wire_constant_spells_its_own_identifier` FAILS, and
`validation_reason_round_trips_each_constant` still **PASSES** — that contrast is
the whole point of the new test and is worth reading in the output.

Restore from the snapshot, never from git:

```bash
cp "$SCRATCH/reason.rs" "$SDK/reason.rs"
touch "$SDK/reason.rs"
cargo nextest run -p cf-gears-usage-collector-sdk -E 'test(reason_tests)'
```

Expected: all PASS, and a `Compiling cf-gears-usage-collector-sdk` line appears.

- [ ] **Step 3: Commit**

```bash
cd /Users/binarycode/code/virtuozzo/gears-rust
git add gears/system/usage-collector/usage-collector-sdk/src/reason_tests.rs
git commit -s -m "test(usage-collector): pin every wire reason spelling to its identifier

The round-trip table proves \`from_wire\` and \`as_wire\` are inverses and
nothing else: both sides of every assertion read the same constant, so
changing a constant's value leaves it green while every client matching the
published spelling breaks. One \`stringify!\` table pins all 18 at once, and
a length assertion keeps the table covering the module rather than a subset.

Recorded as DIVERGENCES.md item D, deferred from slice 5 on the grounds that
slice 6 owns this vocabulary — and this is the front of that pass, before
the vocabulary moves."
```

---

## Task 2: Rename the aggregate request DTO to the name the contract publishes

Two of the three real drifts are one naming mismatch: the registry publishes the
component `QueryAggregatedUsageRecordsRequest`, the yaml declares
`AggregationRequest`. The DTO's own doc comment already says it "matches
`AggregationRequest` in `docs/usage-collector-v1.yaml`" — the code knows the
contract name and does not use it.

`contract_name` in the drift suite strips a `Dto` suffix, and the repo convention
is `UsageRecordDto` / `UsageRecord`. So the target name is **`AggregationRequestDto`**.

This changes a schema name in `/cf/openapi.json`. It does **not** change any JSON
payload shape — no field is renamed, added or removed.

**Files:**
- Modify: `usage-collector/src/api/rest/dto.rs:381` (the struct and its `impl`)
- Modify: `usage-collector/src/api/rest/routes/usage_records.rs:220`
- Modify: `usage-collector/src/api/rest/dto_tests.rs` (5 references)
- Modify: `usage-collector/src/api/rest/handlers/usage_records_tests.rs` (references)

- [ ] **Step 1: Find every reference before touching anything**

**Grep for the member, not the count.** Report the list, not a number:

```bash
cd /Users/binarycode/code/virtuozzo/gears-rust/gears/system/usage-collector
grep -rn 'QueryAggregatedUsageRecordsRequest' --include='*.rs' usage-collector/src
```

- [ ] **Step 2: Capture the red — from real state, not a new unit test**

**Do not write a `std::any::type_name::<AggregationRequestDto>()` assertion.**
It would pass the moment the type compiles, which makes it a tautology: there is
no mutation to production code that reddens it without also failing the build.
The property under test is not "a Rust type has a name" — it is "the OpenAPI
component the registry publishes equals the one the contract declares", and that
comparison already exists in the drift suite. Task 3 turns it on permanently;
this task uses it as the red.

Record the current failures verbatim — you will assert against these exact
strings in Step 5:

```bash
cargo nextest run -p cf-gears-usage-collector --run-ignored all \
  -E 'test(openapi_contract_tests)' --no-fail-fast 2>&1 | grep -A6 'panicked at'
```

**Exactly one** of the six failures names the DTO:

```
every_registered_component_is_documented ... the runtime document publishes
components the contract does not declare: ["QueryAggregatedUsageRecordsRequest"]
```

**Not two.** The other half of this drift lives in `body_schemas_match`, and you
will not see it yet: that check iterates `yaml_ops()` — a `BTreeMap` keyed
`"METHOD /path"` — and `spec_for` panics on the first documented-but-unregistered
route it reaches. `"GET /usage-collector/v1/feed"` sorts before
`"POST /usage-collector/v1/records/aggregate"`, so the check short-circuits on
`/feed` every time and never reaches the aggregate body comparison. The
`AggregationRequest` / `QueryAggregatedUsageRecordsRequest` mismatch in that
check is real, and it surfaces only once Task 3 excludes `/feed`.

That ordering artifact is why this task's green is
`every_registered_component_is_documented` flipping to **pass outright** — it
compares component-schema sets and calls no `spec_for`, so nothing shadows it.

Paste the failure above into your report before changing anything. If it does not
appear, stop and report — the premise of this task has moved.

- [ ] **Step 3: Rename**

In `usage-collector/src/api/rest/dto.rs`, rename the struct and its inherent
`impl`, and update the doc comment so it states the relationship rather than
noting a mismatch:

```rust
/// Request body for `POST /usage-collector/v1/records/aggregate`.
///
/// Published as the `AggregationRequest` component — the drift suite strips
/// the `Dto` suffix, so this type's name *is* the contract's component name
/// (`docs/usage-collector-v1.yaml`). Carries no aggregation parameter: the
/// fold is a property of the meter's resolved declaration, never a caller
/// input.
pub struct AggregationRequestDto {
```

Then rename every reference found in Step 1. Do it explicitly, file by file — do
**not** run a repository-wide `sed`, because `docs/`, `DIVERGENCES.md` and this
plan all mention the old name in prose where it is still correct as history.

- [ ] **Step 4: Confirm the red went green**

```bash
cargo nextest run -p cf-gears-usage-collector --run-ignored all \
  -E 'test(openapi_contract_tests)' --no-fail-fast 2>&1 | grep -A6 'panicked at'
```

Expected: **5 failures, down from 6** — `every_registered_component_is_documented`
now passes outright, because the only component drift was this name. The other
five still fail on the `/feed` cause, which this task does not touch.

The count alone is still not the signal, because a count cannot tell you *which*
check moved. The signal is that **neither `AggregationRequest` nor
`QueryAggregatedUsageRecordsRequest` appears anywhere in the output any more.**

```bash
cargo nextest run -p cf-gears-usage-collector --run-ignored all \
  -E 'test(openapi_contract_tests)' --no-fail-fast 2>&1 \
  | grep -c 'AggregationRequest'
```

Expected: `0`. Report that number and the two before/after failure messages.

Then confirm nothing else broke:

```bash
cargo nextest run -p cf-gears-usage-collector --no-fail-fast
```

- [ ] **Step 5: Commit**

```bash
git add gears/system/usage-collector/usage-collector/src/api/rest/
git commit -s -m "refactor(usage-collector)!: name the aggregate request DTO for its contract component

The registry published \`QueryAggregatedUsageRecordsRequest\` while
usage-collector-v1.yaml declares \`AggregationRequest\`, so a client generated
from the contract named one type and /cf/openapi.json advertised another.
The DTO's own doc comment already said which component it matched.

No wire payload changes: no field is renamed, added or removed. Only the
OpenAPI component name moves, and it moves onto the published one.

BREAKING CHANGE: the OpenAPI component for the aggregate request body is
now \`AggregationRequest\` (was \`QueryAggregatedUsageRecordsRequest\`). Any
client generated from /cf/openapi.json regenerates to the contract's name."
```

---

## Task 3: Turn the OpenAPI drift gate back on

Six `#[ignore]`s come off. Their reason string was stale on three of its four
claims: `/records/backfill` was registered by slice 5, the yaml has no `{gts…}`
path parameter at all, and no usage-type route is registered. Only `/feed` and
`/reconciliation` were still true.

**Rewriting those six strings is not the job. Deleting them against an explicit
not-yet-implemented list is.**

Two structures are needed, and they are different in kind:

- `NOT_YET_IMPLEMENTED` — documented operations no route registers yet. This
  **shrinks to empty** as the feed and reconciliation land.
- `UNDOCUMENTED_PARAMETERS` — parameters the gear registers that the yaml omits.
  The yaml is out of scope, so this records a real gap. It must be **self-cleaning**:
  a listed gap that has closed fails the suite, so the list can never rot into a
  general-purpose exemption list.

**The exclusion must not weaken the live checks.** `harness_sees_the_whole_rest_surface`
asserts the yaml documents 7 operations, and it is the guard that keeps
`NOT_YET_IMPLEMENTED` honest. **Do not filter inside `yaml_ops`** — that was tried
and it turned a live green test red by hiding two operations from it. Filter at
the comparison sites instead.

**Files:**
- Modify: `usage-collector/src/api/rest/routes/openapi_contract_tests.rs`

- [ ] **Step 1: Add the two lists and their guard tests**

After `fn op_key` (around line 91), add:

```rust
/// Operations `usage-collector-v1.yaml` documents that no route registers
/// yet.
///
/// Both are out of scope for the slice that re-enabled this gate: the SPI
/// declares `read_feed_page` and `get_reconciliation_metadata` in DESIGN
/// §3.3 and the gear implements neither, so there is nothing to compare.
/// Naming them here rather than `#[ignore]`ing the whole comparison is the
/// difference between two known gaps and a blanket exemption: every
/// operation that *does* ship is checked on every run.
///
/// This list shrinks to empty. [`not_yet_implemented_operations_are_really_documented`]
/// and [`not_yet_implemented_operations_are_really_unregistered`] fail if an
/// entry stops being true in either direction, so landing `/feed` forces its
/// row out rather than leaving it silently excusing a live route.
const NOT_YET_IMPLEMENTED: &[&str] = &[
    "GET /usage-collector/v1/feed",
    "GET /usage-collector/v1/reconciliation",
];

/// `(operation key, parameter name)` pairs the gear registers and the
/// contract does not document.
///
/// The gear serves both, deliberately and with its reasoning recorded at the
/// registration site: `$top` because the toolkit `OData` extractor binds page
/// size from `$top` or `limit` onto one slot and publishing one spelling
/// under-reports the accepted surface, and `metadata.<key>` because the raw
/// read path accepts repeated metadata filters. `usage-collector-v1.yaml`
/// documents neither, and the yaml is not this slice's to edit.
///
/// **This list only ever excuses registered-but-undocumented.** The reverse —
/// documented but unregistered — is the dangerous direction (it means the
/// gear does not serve something the contract promises) and is never
/// excused here; that is what `NOT_YET_IMPLEMENTED` is for, at whole-operation
/// granularity where it is visible.
///
/// Self-cleaning: [`undocumented_parameters_are_really_undocumented`] fails
/// on any row whose gap has closed.
const UNDOCUMENTED_PARAMETERS: &[(&str, &str)] = &[
    ("GET /usage-collector/v1/records", "$top"),
    ("GET /usage-collector/v1/records", "metadata.<key>"),
];
```

- [ ] **Step 2: Write the three guard tests**

Add near the other live tests (after `registry_keys_match_the_generated_document`):

```rust
/// Every `NOT_YET_IMPLEMENTED` key names an operation the document really
/// carries. A typo, or a row left behind after the document dropped an
/// operation, would silently excuse nothing while looking like it excused
/// something.
#[test]
fn not_yet_implemented_operations_are_really_documented() {
    let doc = spec_doc();
    let documented = yaml_ops(&doc);
    for key in NOT_YET_IMPLEMENTED {
        assert!(
            documented.contains_key(*key),
            "`{key}` is listed as not-yet-implemented but \
             usage-collector-v1.yaml documents no such operation; remove the row",
        );
    }
}

/// No `NOT_YET_IMPLEMENTED` key names an operation that now registers.
///
/// This is the row's expiry. The day `/feed` ships, its route registers and
/// this fails, forcing the row out — instead of leaving an excused live
/// operation permanently outside the comparison.
#[test]
fn not_yet_implemented_operations_are_really_unregistered() {
    let reg = registry();
    let registered = registry_ops(&reg);
    for key in NOT_YET_IMPLEMENTED {
        assert!(
            !registered.contains_key(*key),
            "`{key}` is listed as not-yet-implemented but a route registers it \
             now; delete the row so the drift gate compares it",
        );
    }
}

/// Every `UNDOCUMENTED_PARAMETERS` row names a parameter that is really
/// registered and really absent from the document.
///
/// Both halves matter and each expires the row on its own. If the gear stops
/// registering the parameter the row excuses nothing; if the yaml starts
/// documenting it the row hides a comparison that would now pass. Either way
/// the row must go, and this says so rather than letting the list ossify.
#[test]
fn undocumented_parameters_are_really_undocumented() {
    let reg = registry();
    let doc = spec_doc();
    let registered = registry_ops(&reg);
    let documented = yaml_ops(&doc);

    for (key, param) in UNDOCUMENTED_PARAMETERS {
        let spec = spec_for(&registered, key);
        let registered_names: BTreeSet<String> = registry_params(spec)
            .into_iter()
            .map(|(name, _, _)| name)
            .collect();
        assert!(
            registered_names.contains(*param),
            "`{key}` no longer registers `{param}`; delete the row",
        );

        let op = documented
            .get(*key)
            .unwrap_or_else(|| panic!("`{key}` must be documented"));
        let documented_names: BTreeSet<String> = yaml_params(&doc, op)
            .into_iter()
            .map(|(name, _, _)| name)
            .collect();
        assert!(
            !documented_names.contains(*param),
            "usage-collector-v1.yaml now documents `{param}` on `{key}`; delete \
             the row so `parameters_match` compares it",
        );
    }
}
```

> `ParamTriple` is `(String, String, bool)` — `(name, location, required)`.
> Confirm that by reading `fn registry_params` before writing the destructuring;
> if the tuple order differs, adjust the `.map` and say so in your report.

- [ ] **Step 3: Apply the exclusions at the comparison sites**

In each of the **six** comparison tests, filter the documented set. Add this
helper next to `yaml_ops` and call it from the six, leaving `yaml_ops` itself
whole so the live checks keep seeing all 7 operations:

```rust
/// The documented operations this suite compares against the registry: every
/// documented operation except the ones no route registers yet.
///
/// Separate from [`yaml_ops`] on purpose. `yaml_ops` is the whole document
/// and the YAML-internal checks read it — in particular
/// [`harness_sees_the_whole_rest_surface`], which asserts the document
/// carries 7 operations and is what keeps [`NOT_YET_IMPLEMENTED`] from
/// quietly growing. Filtering inside `yaml_ops` would hide those two
/// operations from that check too and turn the guard into a tautology.
fn comparable_yaml_ops(doc: &Value) -> BTreeMap<String, Value> {
    yaml_ops(doc)
        .into_iter()
        .filter(|(key, _)| !NOT_YET_IMPLEMENTED.contains(&key.as_str()))
        .collect()
}
```

Then in `operation_identity_matches`, `parameters_match`, `body_schemas_match`,
`every_operation_declares_the_standard_error_set`,
`every_registered_component_is_documented` and `security_matches_authenticated_routes`,
replace `yaml_ops(&doc)` with `comparable_yaml_ops(&doc)`.

In `parameters_match` only, subtract the documented gap before comparing:

```rust
        let mut registered = registry_params(spec);
        // Parameters the gear serves and the contract does not document.
        // Removed from the registered side rather than added to the
        // documented one: adding would assert the document says something it
        // does not, and this suite exists to compare the two documents as
        // they are.
        registered.retain(|(name, _, _)| {
            !UNDOCUMENTED_PARAMETERS
                .iter()
                .any(|(gap_key, gap_param)| gap_key == key && gap_param == name)
        });
```

- [ ] **Step 4: Delete the six `#[ignore]` attributes and rewrite the module header**

Delete all six. Then replace the module header's "Deferred" paragraph
(lines 16-20) — **replace the sentence, do not reword it**:

```rust
//! Scope: the comparison checks run against every operation the gear
//! registers. Two documented operations are named in
//! [`NOT_YET_IMPLEMENTED`] because no route registers them yet, and two
//! registered parameters are named in [`UNDOCUMENTED_PARAMETERS`] because
//! the contract does not document them. Both lists are guarded by tests
//! that fail when a listed gap closes, so neither can become a standing
//! exemption. Everything else is compared on every run.
```

- [ ] **Step 5: Run the whole drift suite**

```bash
cargo nextest run -p cf-gears-usage-collector -E 'test(openapi_contract_tests)' --no-fail-fast
```

Expected: **13 passed, 0 skipped, 0 failed** (the 10 that existed, minus none,
plus the 3 new guards). If a comparison test still fails, **read the message
before adding a row to either list** — a new row is only correct for a gap this
plan already named. Any other failure is real drift that Task 12 must record and
the controller must be told about.

- [ ] **Step 6: Falsify the gate**

A gate that cannot fail is worse than an `#[ignore]`, because it looks like
coverage. Confirm it catches drift in the direction that matters:

```bash
SCRATCH="$(mktemp -d)"
R=/Users/binarycode/code/virtuozzo/gears-rust/gears/system/usage-collector/usage-collector/src/api/rest/routes/usage_records.rs
cp "$R" "$SCRATCH/usage_records.rs"
sed -i '' 's/\.operation_id("usage_collector.list_usage_records")/.operation_id("usage_collector.list_usage_recordz")/' "$R"
grep -n 'list_usage_recordz' "$R"   # MUST print — otherwise the sed missed
cargo nextest run -p cf-gears-usage-collector -E 'test(openapi_contract_tests)' --no-fail-fast
```

Expected: `operation_identity_matches` FAILS with an operationId mismatch.

```bash
cp "$SCRATCH/usage_records.rs" "$R"
touch "$R"
cargo nextest run -p cf-gears-usage-collector -E 'test(openapi_contract_tests)' --no-fail-fast
```

Expected: all PASS, with a `Compiling cf-gears-usage-collector` line.

- [ ] **Step 7: Commit**

```bash
git add gears/system/usage-collector/usage-collector/src/api/rest/routes/openapi_contract_tests.rs
git commit -s -m "test(usage-collector): re-enable the OpenAPI drift gate

The six comparison checks were \`#[ignore]\`d behind a reason string that was
stale on three of its four claims: /records/backfill registered in the
origin-and-backfill slice, the yaml carries no {gts...} path parameter at
all, and no usage-type route is registered. Only /feed and /reconciliation
were still true, and they are two whole operations rather than a reason to
stop comparing the five that ship.

They are now named in NOT_YET_IMPLEMENTED, applied at the comparison sites
so the YAML-internal checks still see the whole document — filtering inside
yaml_ops instead turns harness_sees_the_whole_rest_surface, the check that
keeps the list honest, into a tautology.

\`\$top\` and \`metadata.<key>\` are registered and undocumented, and the yaml
is not this slice's to edit, so they are named in UNDOCUMENTED_PARAMETERS
and filed as a divergence. Both lists are guarded by tests that fail when a
listed gap closes in either direction, so neither can ossify into a general
exemption list."
```


---

## Task 4: Route cursor rejections through `toolkit_odata`'s error

Decision 2. Spec §3.13: "The gear does **not** define `INVALID_CURSOR`,
`FILTER_MISMATCH`, or `ORDER_WITH_CURSOR`. Those are owned upstream by
`toolkit_odata` … and duplicating them here would create a second place the same
code can be read and disagree."

The gear does not merely offer a typed *view* of these codes — it **originates**
them. `usage-collector-sdk/src/error.rs` constructs
`ValidationReason::InvalidCursor` and `ValidationReason::FilterMismatch`, and
`domain/query.rs` raises them from the gear's own cursor checks at four sites.

`toolkit_odata` declares `Error::InvalidCursor` and `Error::FilterMismatch`
(`libs/toolkit-odata/src/lib.rs:296-305`) and maps both to a `cursor` field
violation carrying the right code (`libs/toolkit-odata/src/problem_mapping.rs`
via `impl From<Error> for CanonicalError`). It exports no bare constant, so the
projection goes through that `From`.

**The gear keeps its detail and gives up the code.** The four raise sites carry
caller guidance that upstream's generic "invalid cursor" does not ("restart
pagination without a cursor"; which inputs the cursor binds). Discarding that to
satisfy §3.13 would trade one real regression for a naming rule. So the new
variant carries both: the upstream error supplies `field` and `reason`, and the
gear supplies the description.

This task **adds** the variant and re-points the raise sites. Task 5 deletes the
now-unreachable `ValidationReason` variants. Split because each half compiles on
its own, and the host crate is one compilation unit.

**This task owes the first in-place `cargo nextest` run for the host crate.**

**Files:**
- Modify: `usage-collector-sdk/src/error.rs`
- Modify: `usage-collector/src/domain/query.rs` (4 raise sites + doc comments)
- Modify: `usage-collector/src/domain/service.rs` (`classify_query_result`)
- Modify: `usage-collector/src/infra/sdk_error_mapping.rs` (the lift)
- Modify: `usage-collector/src/domain/service_metrics_tests.rs` (2 constructor calls)

- [ ] **Step 1: Write the failing cross-seam test**

This is the seam Ground rule 2 is about: the gear's rendered code and
`toolkit_odata`'s must be *the same value*, not two values that happen to match.
Assert against the upstream mapping's output, never against a literal.

Add to `usage-collector/src/infra/sdk_error_mapping_tests.rs` (it exists, and it
already holds this crate's lift tests):

```rust
/// The wire code on a cursor rejection is `toolkit_odata`'s, read from
/// `toolkit_odata`.
///
/// Spec §3.13 gives the cursor codes to `toolkit_odata` because a second
/// declaration is a second place the same code can be read and disagree.
/// Asserting against a `"INVALID_CURSOR"` literal here would *be* that
/// second place — the test would keep passing if upstream renamed the code,
/// which is the exact failure the rule exists to prevent. So both sides of
/// this assertion come from upstream, and the gear's side has to travel
/// through the gear's own lift to get there.
#[test]
fn a_cursor_rejection_carries_the_upstream_wire_code() {
    for (upstream, gear) in [
        (
            toolkit_odata::Error::InvalidCursor,
            UsageCollectorError::inadmissible_cursor_keyset("mixed directions"),
        ),
        (
            toolkit_odata::Error::FilterMismatch,
            UsageCollectorError::cursor_query_mismatch(),
        ),
    ] {
        let expected = first_field_violation(&CanonicalError::from(upstream));
        let actual = first_field_violation(&usage_collector_error_to_canonical_for_usage_record(gear));

        assert_eq!(
            actual.reason, expected.reason,
            "the gear must surface `toolkit_odata`'s reason code verbatim",
        );
        assert_eq!(
            actual.field, expected.field,
            "the gear must attribute to the same request field as upstream",
        );
    }
}

/// The gear's own caller guidance survives the projection.
///
/// The point of carrying `detail` alongside the upstream error is that a
/// caller is told how to recover; upstream's description is "invalid
/// cursor". A positive anchor, not just a `!=`: an assertion that the
/// description merely *differs* from upstream's would pass against an
/// empty string.
#[test]
fn a_cursor_rejection_keeps_the_gear_s_recovery_guidance() {
    let lifted = usage_collector_error_to_canonical_for_usage_record(
        UsageCollectorError::inadmissible_cursor_keyset("it carries no cursor"),
    );
    let violation = first_field_violation(&lifted);

    assert!(
        violation.description.contains("it carries no cursor"),
        "the defect must reach the caller: got {:?}",
        violation.description,
    );
    assert!(
        violation.description.contains("restart pagination without a cursor"),
        "the recovery must reach the caller: got {:?}",
        violation.description,
    );
}
```

You will need a small helper in the same test file:

```rust
/// The single `field_violations` entry on an `InvalidArgument` canonical
/// error. Panics loudly on any other shape — a cursor rejection that stopped
/// being a field violation is the thing under test, not a reason to skip.
fn first_field_violation(err: &CanonicalError) -> &FieldViolation {
    match err {
        CanonicalError::InvalidArgument {
            ctx: InvalidArgument::FieldViolations { field_violations },
            ..
        } => field_violations
            .first()
            .expect("a cursor rejection carries one field violation"),
        other => panic!("expected an InvalidArgument field violation, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo nextest run -p cf-gears-usage-collector -E 'test(a_cursor_rejection)' --no-fail-fast
```

Expected: FAIL. Today `inadmissible_cursor_keyset` produces an
`InvalidArgument` whose reason string is the gear's own `INVALID_CURSOR`
constant — the values are equal, so **the first test may pass by coincidence**.
That is precisely the coincidence Task 5 removes. If it passes here, say so in
your report and confirm it still passes after Task 5, which is when it becomes
load-bearing. `a_cursor_rejection_keeps_the_gear_s_recovery_guidance` must pass
throughout — it pins the detail that Step 3 must not lose.

- [ ] **Step 3: Add the variant and its constructors**

In `usage-collector-sdk/src/error.rs`, add to `UsageCollectorError`:

```rust
    /// A continuation token refused by the gear, carrying the wire code
    /// `toolkit_odata` owns.
    ///
    /// Spec §3.13 gives `INVALID_CURSOR`, `FILTER_MISMATCH` and
    /// `ORDER_WITH_CURSOR` to `toolkit_odata`: the gear declares none of
    /// them, because a second declaration is a second place the same code
    /// can be read and disagree. `source` is the upstream error and is the
    /// only thing that decides the wire `field` and `reason`; the host lift
    /// obtains both by converting it.
    ///
    /// `detail` is the gear's own, and is why this variant carries two
    /// things rather than one. The gear knows which of its checks refused
    /// and how the caller recovers; upstream's description for every cursor
    /// failure is "invalid cursor". Propagating the bare upstream error
    /// would satisfy the naming rule by discarding the guidance, so the
    /// code comes from upstream and the prose stays here.
    #[error("cursor rejected [{source}]: {detail}")]
    CursorRejected {
        /// The upstream cursor error. Sole source of the wire `field` and
        /// `reason`.
        source: toolkit_odata::errors::Error,
        /// Gear-authored caller guidance, rendered as the violation
        /// description.
        detail: String,
    },
```

> Confirm the import path for `toolkit_odata`'s error before writing this —
> `libs/toolkit-odata/src/lib.rs` declares `pub mod errors;` and the `Error` enum
> is at `lib.rs:296`. Read `lib.rs`'s `pub use` lines and use whichever path
> actually resolves; if it is `toolkit_odata::Error`, use that and note the
> correction in your report.

Replace the two constructors' bodies (keep their doc comments, **rewriting the
paragraphs that name the code** — `inadmissible_cursor_keyset`'s says "with
`INVALID_CURSOR` — the same field and code `toolkit_odata`'s own decode failures
use", and that sentence is now describing a mechanism rather than a coincidence,
so say so):

```rust
    #[must_use]
    pub fn inadmissible_cursor_keyset(defect: impl Into<String>) -> Self {
        let defect = defect.into();
        Self::CursorRejected {
            source: toolkit_odata::errors::Error::InvalidCursor,
            detail: format!(
                "the cursor's bound order is not a usable keyset ({defect}); \
                 restart pagination without a cursor"
            ),
        }
    }
```

```rust
    #[must_use]
    pub fn cursor_query_mismatch() -> Self {
        Self::CursorRejected {
            source: toolkit_odata::errors::Error::FilterMismatch,
            detail: "the cursor was minted over a different query: continue a page by \
                     resending the same request, cursor apart, or restart pagination \
                     without a cursor. The cursor binds `gts_type_id`, the `from` / \
                     `to` range, `$filter` and every `metadata.<key>` filter, so \
                     changing any of them invalidates it"
                .to_owned(),
        }
    }
```

The four raise sites in `domain/query.rs` and the two in
`service_metrics_tests.rs` call these constructors, so they **do not change**.
Their surrounding doc comments do — see Step 6.

- [ ] **Step 4: Lift it at the REST boundary**

In `usage-collector/src/infra/sdk_error_mapping.rs`, add an arm to `lift_common`:

```rust
        // ---- 400 InvalidArgument, upstream-coded ----
        // The wire `field` and `reason` come from `toolkit_odata`'s own
        // mapping and are never spelled here (Spec §3.13). Only the
        // description is replaced, because that half is the gear's: it names
        // which check refused and how the caller recovers, and upstream has
        // one string for every cursor failure.
        E::CursorRejected { source, detail } => {
            let mut lifted = CanonicalError::from(source);
            if let CanonicalError::InvalidArgument {
                ctx: InvalidArgument::FieldViolations { field_violations },
                ..
            } = &mut lifted
            {
                if let Some(first) = field_violations.first_mut() {
                    first.description = detail;
                }
            }
            lifted
        }
```

> If `toolkit_odata`'s conversion ever stopped producing a field violation, the
> `if let` would silently ship upstream's description. That is a real hole, so
> **add a `debug_assert!` on the match failing** — the same posture
> `unrecognized_resource` already takes in this file for a host-side breach.
> Write it as an `else` branch that asserts, not as a silent fallthrough.

- [ ] **Step 5: Keep the query metric discriminating**

`classify_query_result` in `domain/service.rs` currently reads
`ValidationReason::FilterMismatch` to emit `QueryErrorCategory::FilterMismatch`.
Re-point it at the upstream variant:

```rust
        Err(UsageCollectorError::CursorRejected { source, .. }) => match source {
            toolkit_odata::errors::Error::FilterMismatch => {
                (RequestOutcome::Error, QueryErrorCategory::FilterMismatch)
            }
            // Everything else this gear raises as a cursor rejection is an
            // `INVALID_CURSOR`, which folds into `query_budget`. That is the
            // one known imprecision on this seam and it is inherited, not
            // introduced here: a continuation whose bound order is not a
            // keyset is not a budget rejection either. Left as it was so the
            // vocabulary pass does not silently move an operator's series.
            _ => (RequestOutcome::Error, QueryErrorCategory::QueryBudget),
        },
```

Keep the existing `InvalidArgument` arm for everything else. **Read the
surrounding doc comment on `classify_query_result` before editing** — it explains
the `filter_mismatch` seam at length and names `ValidationReason`. That sentence
is falsified by this change: replace it, do not reword it.

- [ ] **Step 6: Rewrite every doc comment that says the gear owns these codes**

**Grep for the members, not a count**, and in every file:

```bash
cd /Users/binarycode/code/virtuozzo/gears-rust/gears/system/usage-collector
grep -rn 'INVALID_CURSOR\|FILTER_MISMATCH\|InvalidCursor\|FilterMismatch' \
  --include='*.rs' usage-collector/src usage-collector-sdk/src plugins/
```

Known sites: `domain/query.rs` (the module-level explanation of the three cursor
rules, plus the `# Errors` sections on `bind_continuation_order`,
`require_continuation_keyset` and `require_cursor_fingerprint`),
`api/rest/handlers/usage_records.rs:283,298`, `plugin_api.rs:215,244`,
`error.rs` (both constructors), and the tests.

Each of those says the gear raises the code. It now *projects* it. Every one of
these is a `///` doc block: **a `//` retraction inside one is not rendered.**

- [ ] **Step 7: Run the full three-package suite — the first in-place run**

```bash
cargo check --workspace --all-targets
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk \
  -p cf-gears-noop-usage-collector-plugin --no-fail-fast
```

Expected: all pass. Report the exact `N tests run: N passed, 0 skipped` line —
**unfiltered, `--no-fail-fast`**, because a count under `-E` is a lower bound.

- [ ] **Step 8: Commit**

```bash
git add gears/system/usage-collector/
git commit -s -m "refactor(usage-collector): project cursor codes from toolkit_odata

Spec 3.13 gives INVALID_CURSOR, FILTER_MISMATCH and ORDER_WITH_CURSOR to
toolkit_odata, because a second declaration is a second place the same code
can be read and disagree. The gear was not merely offering a typed view of
two of them: its own cursor checks originated them, from constants it
declared itself.

UsageCollectorError::CursorRejected now carries the upstream error, and the
host lift converts it to get the wire field and reason. The gear spells
neither. It keeps the description, because that half is genuinely the
gear's: it names which check refused and how the caller recovers, where
upstream has one string for every cursor failure, and propagating the bare
upstream error would have satisfied the naming rule by discarding the
guidance.

The wire output is unchanged: same field, same code, same description. The
codes now have one definition instead of two."
```

---

## Task 5: Delete `INVALID_CURSOR` and `FILTER_MISMATCH` from the gear's vocabulary

With Task 4 landed, nothing constructs `ValidationReason::InvalidCursor` or
`ValidationReason::FilterMismatch`. Remove them and their constants.

**This is a breaking change**, and a quiet one: `ValidationReason` is
`#[non_exhaustive]`, so a downstream matcher on either variant stops compiling
while a downstream `from_wire` caller silently starts getting `Unknown`. The
existing test `the_retired_compensation_reasons_no_longer_model_themselves` is
the precedent for pinning exactly that.

**Files:**
- Modify: `usage-collector-sdk/src/reason.rs`
- Modify: `usage-collector-sdk/src/reason_tests.rs`

- [ ] **Step 1: Write the failing test**

In `reason_tests.rs`, extend the existing fall-through test's sibling. Add:

```rust
/// The two cursor codes now fall through to `Unknown`, preserving the wire
/// string.
///
/// Spec §3.13 gives them to `toolkit_odata`. Because `ValidationReason` is
/// `#[non_exhaustive]`, removing a variant is silent for a downstream
/// matcher: it falls through rather than failing to build. So pin that the
/// fall-through happens *and* that it preserves the raw string, so a
/// consumer holding a stored envelope that carries either code still reads
/// what it said.
///
/// `ORDER_WITH_CURSOR` is in the list although this gear never modeled it —
/// the rule §3.13 states covers all three, and a future variant added for it
/// would be the same mistake.
#[test]
fn the_upstream_cursor_reasons_no_longer_model_themselves() {
    for wire in ["INVALID_CURSOR", "FILTER_MISMATCH", "ORDER_WITH_CURSOR"] {
        assert_eq!(
            ValidationReason::from_wire(wire),
            ValidationReason::Unknown(wire.to_owned()),
        );
        assert_eq!(ValidationReason::from_wire(wire).as_wire(), wire);
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo nextest run -p cf-gears-usage-collector-sdk \
  -E 'test(the_upstream_cursor_reasons_no_longer_model_themselves)'
```

Expected: FAIL — `from_wire("INVALID_CURSOR")` still returns
`ValidationReason::InvalidCursor`.

> **One stale doc rides along with the deletion.** `reason.rs:49-52`'s
> `INVALID_CURSOR` doc says the code is emitted "by `toolkit_odata`'s own cursor
> decode path **and by the read path's keyset floor**". The second half stopped
> being true when Task 4 re-pointed the constructors, and Task 4 deliberately
> left it because this task deletes the const outright. Confirm the whole doc
> block goes with the const rather than being orphaned onto something else — and
> if you find you are *keeping* any of that prose, fix the claim first.

- [ ] **Step 3: Delete**

From `reason.rs`, remove:
- `pub const INVALID_CURSOR` (line ~52) and `pub const FILTER_MISMATCH` (~67),
  **with their doc comments**
- `ValidationReason::InvalidCursor` and `ValidationReason::FilterMismatch`
- their `from_wire` arms
- their `as_wire` arms

Add to the `ValidationReason` doc comment a `///` paragraph — not a `//` one —
recording what left and why:

```rust
/// Typed view of the `field_violations[].reason` codes carried by
/// [`crate::UsageCollectorError::InvalidArgument`].
///
/// It does **not** model `INVALID_CURSOR`, `FILTER_MISMATCH` or
/// `ORDER_WITH_CURSOR`. Those belong to `toolkit_odata`, which declares them
/// on its own cursor error enum and maps them to `Problem` field violations;
/// this gear surfaces them by propagating that error through
/// [`crate::UsageCollectorError::CursorRejected`]. Modeling them here as
/// well would create a second place the same code can be read and disagree
/// (Spec §3.13). A consumer matching on one reads it out of the wire string
/// via [`Self::Unknown`].
```

- [ ] **Step 4: Update the pinning table from Task 1**

**This step is smaller than it was when the plan was written, and the reason
matters.** The original text said the table "names all 18 constants and asserts
the length … change `18` to `16`". That hardcoded literal is gone: Task 1's
review found `assert_eq!(pinned.len(), 18)` was a tautology — a fixed-size
array's length always equals the rows written above it — and replaced it with a
compile-time count of `reason.rs` itself:

```rust
let declared = include_str!("reason.rs")
    .lines()
    .filter(|line| line.starts_with("pub const "))
    .count();
```

So there is **no number to update.** Delete the two `pin![…]` rows and the
`declared` count follows the file down from 18 to 16 on its own. Deleting the
constants without deleting the rows is a compile error (unresolved identifier),
and deleting the rows without the constants fails the count — the pair is
inseparable in both directions with nothing to remember.

Confirm afterwards that `grep -c '^pub const ' usage-collector-sdk/src/reason.rs`
prints `16` and the test passes; if the two disagree, the line-anchored scan has
met a case it cannot see (a `pub const`-shaped line in a block comment or a raw
string) and that is worth reporting, not working around.

Also remove the two rows from `validation_reason_round_trips_each_constant`.

- [ ] **Step 5: Run**

```bash
cargo nextest run -p cf-gears-usage-collector-sdk -p cf-gears-usage-collector \
  -p cf-gears-noop-usage-collector-plugin --no-fail-fast
```

Expected: all pass. In particular
`a_cursor_rejection_carries_the_upstream_wire_code` from Task 4 is now
load-bearing rather than coincidentally green — **say so explicitly in your
report**, because it is the test that proves the deletion did not change the wire.

- [ ] **Step 6: Commit**

```bash
git add gears/system/usage-collector/usage-collector-sdk/src/reason.rs \
        gears/system/usage-collector/usage-collector-sdk/src/reason_tests.rs
git commit -s -m "refactor(usage-collector)!: drop the two upstream-owned cursor reasons

Nothing constructs ValidationReason::InvalidCursor or ::FilterMismatch since
cursor rejections began projecting toolkit_odata's error, so the variants
and their constants go. Spec 3.13: those codes are toolkit_odata's, and a
second declaration is a second place the same code can be read and disagree.

The wire is unchanged — the same codes go out, from one definition instead
of two — but the typed view is not, and ValidationReason is #[non_exhaustive],
so this is silent for a downstream from_wire caller. Pinned in both
directions: both codes now fall through to Unknown preserving the raw
string, and the spelling table drops from 18 rows to 16 with its length
assertion moved in the same edit.

BREAKING CHANGE: ValidationReason no longer models INVALID_CURSOR or
FILTER_MISMATCH, and usage_collector_sdk::reason no longer exports those
constants. A consumer matching on either variant must match
ValidationReason::Unknown, or read the code from
toolkit_odata. Envelopes on the wire are unaffected."
```

---

## Task 6: Give `NotFound` a typed reason

Decision 3, and it closes `DIVERGENCES.md` entry 9(a).

Today `classify_record_error` tells an unresolvable meter from an unresolvable
`invalidates` by **parsing the `name` field as a UUID** — a `gts_type_id` never
parses, an entry id always does. That works, and it cannot go further: it still
cannot separate `usage_record_not_found` from `invalidation_target_not_found`,
because both carry a UUID. So the ADR's valid-reference rule lands on
`semantics_violation` beside unrelated failures, and §3.11.5's `invalidation_rule`
under-counts a correction backlog by exactly that condition.

The existing code was right to refuse a substring match on `detail` — a label
classified off a message string stops matching the day the message is reworded.
A typed discriminator is the fix the entry names.

**The reason is not projected onto the wire.** A 404 canonical envelope has no
`context.reason` slot, and inventing one is a contract change this slice does not
own. So a *client* still cannot tell the two apart; only the gear can. Task 12
records that remainder in entry 9 rather than letting the entry read as closed.

**Files:**
- Modify: `usage-collector-sdk/src/reason.rs` (new `NotFoundReason`)
- Modify: `usage-collector-sdk/src/error.rs` (`NotFound` gains `reason`; 2 constructors)
- Modify: `usage-collector-sdk/src/lib.rs` (export)
- Modify: `usage-collector/src/domain/error.rs:374` (the `DeclarationNotFound` lift)
- Modify: `usage-collector/src/domain/service.rs` (`classify_record_error`)
- Modify: `usage-collector/src/infra/sdk_error_mapping.rs` (destructuring)

- [ ] **Step 1: Write the failing test**

In `usage-collector/src/domain/service_metrics_tests.rs`, where the other
`classify_record_error` cases live (`grep -rln 'classify_record_error'
--include='*_tests.rs' usage-collector/src` also names
`infra/sdk_error_mapping_tests.rs`; the classification cases belong with the
metric tests):

```rust
/// An unresolvable `invalidates` is an invalidation-rule rejection.
///
/// DESIGN §3.11.5 gives `invalidation_rule` "the copy, reference and
/// at-most-one rules". The reference rule is the one that used to be
/// unreachable: it surfaces as `NotFound`, which carried no typed reason, so
/// nothing but `detail` prose separated it from an ordinary
/// `usage_record_not_found` — and a plugin's own `UsageRecordNotFound`
/// reaches the same variant. A bounded metric label classified by substring
/// match on a caller-facing string stops matching the day the string is
/// reworded, so the code declined to guess and the label under-counted.
///
/// The three cases are asserted together because the discriminator is what
/// is under test: any one alone passes against a function that returns a
/// constant.
#[test]
fn not_found_classifies_by_typed_reason_not_by_parsing_the_name() {
    let target = Uuid::new_v4();

    assert_eq!(
        classify_record_error(&UsageCollectorError::invalidation_target_not_found(target)),
        RecordErrorCategory::InvalidationRule,
        "an `invalidates` resolving to nothing is the ADR's valid-reference \
         rule and belongs with the other invalidation rules",
    );
    assert_eq!(
        classify_record_error(&UsageCollectorError::usage_record_not_found(target)),
        RecordErrorCategory::SemanticsViolation,
        "an ordinary missing entry is not an invalidation rule, and it carries \
         a uuid `name` exactly like the case above — which is why parsing \
         `name` could never separate them",
    );
    assert_eq!(
        classify_record_error(&UsageCollectorError::NotFound {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            name: "cf.core.uc.meter.v1~cpu".to_owned(),
            reason: NotFoundReason::DeclarationNotFound,
            detail: "GTS type not declared".to_owned(),
        }),
        RecordErrorCategory::UnknownUsageType,
    );
}
```

> The third case constructs the variant literally rather than through a
> constructor because the `DeclarationNotFound` lift lives in the host crate's
> `domain/error.rs`, not in the SDK. The identifier field is spelled **`name`**
> (`usage-collector-sdk/src/error.rs:94`), not `resource_name` — `resource_name`
> is `InvalidArgument`'s. The two variants differ here and the mix-up compiles
> nowhere, but it costs a round trip.

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo nextest run -p cf-gears-usage-collector \
  -E 'test(not_found_classifies_by_typed_reason)'
```

Expected: FAIL to compile — `NotFoundReason` does not exist and `NotFound` has no
`reason` field.

- [ ] **Step 3: Add `NotFoundReason`**

In `usage-collector-sdk/src/reason.rs`, after `ConflictReason`:

```rust
// ─────────────────────────────────────────────────────────────────────
// NotFoundReason — 404, gear-internal. NOT a wire vocabulary.
// ─────────────────────────────────────────────────────────────────────

/// Which lookup failed, on [`crate::UsageCollectorError::NotFound`].
///
/// **Not a wire vocabulary, and deliberately not one.** The AIP-193 404
/// envelope has no `context.reason` slot, so unlike [`ValidationReason`] and
/// [`ConflictReason`] these have no `SCREAMING_SNAKE` constants, no
/// `from_wire` / `as_wire`, and never appear on a `Problem` body. A caller
/// still tells the three apart only by `detail` prose. Giving 404 a wire
/// reason is a contract change and belongs to whoever owns
/// `usage-collector-v1.yaml`.
///
/// What it exists for is the gear's own classification. §3.11.5's
/// `invalidation_rule` covers "the copy, reference and at-most-one rules",
/// and the reference rule — an `invalidates` resolving to nothing — is a
/// `NotFound`. Without a discriminator the only thing separating it from an
/// ordinary missing entry is the message string, and classifying a bounded
/// metric label by substring match on caller-facing prose is how a label
/// stops matching silently when the prose is reworded.
///
/// `#[non_exhaustive]` for the same reason the wire enums are: a consumer
/// matching on it must keep a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NotFoundReason {
    /// A `gts_type_id` that does not resolve to a usable declaration —
    /// the Type Resolver's `DeclarationNotFound`, whether the registry
    /// never declared it or the declaration was rejected as incomplete.
    DeclarationNotFound,
    /// A lookup named a `UsageRecord.id` the store does not hold: a point
    /// read, or a plugin's own `UsageRecordNotFound`.
    UsageRecordNotFound,
    /// An invalidation's `invalidates` resolved to nothing. The
    /// valid-reference rule of
    /// `cpt-cf-usage-collector-adr-append-only-invalidation`.
    InvalidationTargetNotFound,
}
```

Export it from `lib.rs` alongside `ConflictReason` and `ValidationReason`.

- [ ] **Step 4: Grow the variant**

In `error.rs`, add `reason: NotFoundReason` to `NotFound` with a doc comment
saying it is gear-internal and not on the wire. Set it in both constructors
(`usage_record_not_found` → `UsageRecordNotFound`,
`invalidation_target_not_found` → `InvalidationTargetNotFound`).

`invalidation_target_not_found`'s doc comment currently explains at length that
`name` is "what separates this from the other `NotFound` a submission can raise"
because "the category carries no wire `context.reason`". **That sentence is now
half wrong** — the wire half still holds, the gear-internal half does not.
Replace it; do not reword it.

In `domain/error.rs:374`, set `reason: NotFoundReason::DeclarationNotFound`.

In `infra/sdk_error_mapping.rs:104`, the `E::NotFound { .. }` destructuring gains
the field. Bind it as `reason: _` **with a comment saying why it is dropped** —
that it has no wire slot, and that a reader looking for it on the `Problem` body
will not find it by design.

- [ ] **Step 5: Rewrite `classify_record_error`**

Replace the two `NotFound` arms and the long comment above them. The comment
explains the UUID-parsing discriminator and why the reference rule stays on
`semantics_violation` — **both facts are now false**, so the paragraph is
replaced, not edited:

```rust
        // §3.11.5's `invalidation_rule` covers the copy, reference and
        // at-most-one rules. The reference rule — an `invalidates` resolving
        // to nothing — is a `NotFound`, and used to be unreachable from
        // here: the variant carried no discriminator, so separating it from
        // an ordinary missing entry meant matching a substring of
        // caller-facing prose, and a bounded metric label classified that way
        // stops matching the day the prose is reworded. `NotFoundReason` is
        // the typed discriminator that closes it.
        UsageCollectorError::NotFound { reason, .. } => match reason {
            NotFoundReason::DeclarationNotFound => RecordErrorCategory::UnknownUsageType,
            NotFoundReason::InvalidationTargetNotFound => RecordErrorCategory::InvalidationRule,
            NotFoundReason::UsageRecordNotFound => RecordErrorCategory::SemanticsViolation,
            // `NotFoundReason` is `#[non_exhaustive]`. A new lookup failure
            // is not silently an invalidation rule.
            _ => RecordErrorCategory::SemanticsViolation,
        },
```

`classify_query_result`'s `NotFound` arm maps everything to
`QueryErrorCategory::UnknownUsageType`. On the query paths the only reachable
reason is `DeclarationNotFound` — the point read raises `usage_record_not_found`
through a different route. **Read that arm and decide deliberately**: either
leave it collapsed with a comment saying which reason reaches it, or discriminate.
Say which you chose and why in your report; do not leave it unexamined.

- [ ] **Step 6: Run**

```bash
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk \
  -p cf-gears-noop-usage-collector-plugin --no-fail-fast
```

Expected: all pass.

Now falsify. The mutation that must make the new test red is a one-token swap in
the match — the shape of defect this whole task exists to make visible:

```bash
SCRATCH="$(mktemp -d)"
S=/Users/binarycode/code/virtuozzo/gears-rust/gears/system/usage-collector/usage-collector/src/domain/service.rs
cp "$S" "$SCRATCH/service.rs"
sed -i '' 's/NotFoundReason::InvalidationTargetNotFound => RecordErrorCategory::InvalidationRule,/NotFoundReason::InvalidationTargetNotFound => RecordErrorCategory::SemanticsViolation,/' "$S"
grep -n 'InvalidationTargetNotFound => RecordErrorCategory::SemanticsViolation' "$S"
cargo nextest run -p cf-gears-usage-collector -E 'test(not_found_classifies_by_typed_reason)'
cp "$SCRATCH/service.rs" "$S" && touch "$S"
```

Expected: FAILS mutated, PASSES restored, with a `Compiling` line on the restore.

- [ ] **Step 7: Commit**

```bash
git add gears/system/usage-collector/
git commit -s -m "feat(usage-collector)!: give NotFound a typed reason

DESIGN 3.11.5 gives invalidation_rule the copy, reference and at-most-one
rules. The reference rule was unreachable: an unresolvable invalidates
surfaces as NotFound, which carried no discriminator, so the only thing
separating it from an ordinary missing entry was detail prose — and a
plugin's own UsageRecordNotFound reaches the same variant. The code declined
to classify a bounded metric label by substring match on a caller-facing
string, correctly, and the label under-counted a correction backlog by
exactly that condition.

NotFoundReason is the typed discriminator. classify_record_error reads it
instead of parsing the name field as a uuid to tell a gts_type_id from an
entry id — a test that could never have separated the two uuid-named cases
from each other.

NotFoundReason is deliberately not a wire vocabulary: the 404 envelope has
no context.reason slot, so a client still tells the three apart only by
detail. That remainder stays recorded in DIVERGENCES.md entry 9.

BREAKING CHANGE: UsageCollectorError::NotFound carries a new \`reason\` field
of type NotFoundReason. Code constructing the variant literally must supply
it; code matching it with \`..\` is unaffected. No wire body changes."
```


---

## The contract suite (Tasks 7-11) — read this before Task 7

DESIGN §3.3: "Every conforming plugin MUST pass the suite in
`usage-collector-sdk`. The tests are behavioural and MUST pass on any backend."

Three consequences shape every one of the next five tasks.

**It must be reachable from a plugin crate.** "Every conforming plugin MUST pass
it" is unenforceable if it lives in `usage-collector-sdk/tests/`, because an
integration test directory is private to its crate. The TimescaleDB port's
acceptance criteria is being able to run this. So the harness is a **public,
feature-gated module** — `usage_collector_sdk::contract` — and the SDK's own
tests are one caller of it, not its home.

**Two of the seven are unwritable, and stay unwritten.**
- `feed-snapshot-and-replay` — the SPI declares no feed method in this gear.
- `latest-tie-break` — asserts "greatest `window_end`, then greatest
  `acceptance_sequence`", and **`acceptance_sequence` is not a field**
  (`grep -rn 'acceptance_sequence' --include='*.rs'` returns seven hits, all
  comments). Decision 4 says it is not this slice's to add.

Both get a `///`-documented `pub const` in the harness naming what they need, so
a plugin author reading the module sees five of seven and *why*, rather than
five and no mention of the other two.

**A suite that only ever runs against one plugin proves the suite runs, not that
it discriminates.** This is Ground rule 2's shape exactly: a reference plugin
checked by assertions written alongside it is two layers tested against
themselves. **Task 11 is the seam**, and it is not optional. Every check must be
shown to fail against a plugin that gets that specific thing wrong — and the
mutants must be wrong in *one* way each, or a mutant that fails three checks
proves nothing about which check caught it.

---

## Task 7: The harness, the reference plugin, and `quantity-round-trip`

**Files:**
- Modify: `usage-collector-sdk/Cargo.toml`
- Create: `usage-collector-sdk/src/contract.rs`
- Create: `usage-collector-sdk/src/contract/reference.rs`
- Create: `usage-collector-sdk/src/contract_tests.rs`
- Modify: `usage-collector-sdk/src/lib.rs`

- [ ] **Step 1: Add the feature**

In `usage-collector-sdk/Cargo.toml`:

```toml
[features]
# The DESIGN §3.3 plugin contract suite, plus the in-memory reference
# backend it is validated against. Off by default: a production build of a
# plugin has no use for either, and the reference backend must never be
# reachable from one by accident.
contract = []

[dev-dependencies]
tokio = { workspace = true }
```

In `lib.rs`:

```rust
#[cfg(feature = "contract")]
pub mod contract;
```

- [ ] **Step 2: Write the harness skeleton with one check**

Create `usage-collector-sdk/src/contract.rs`:

```rust
//! The DESIGN §3.3 plugin contract suite.
//!
//! Every conforming storage plugin MUST pass this. The checks are
//! behavioural and hold on any backend: each one drives the
//! [`UsageCollectorPluginV1`](crate::UsageCollectorPluginV1) surface and
//! asserts an observable outcome, never an implementation detail.
//!
//! # Running it against your plugin
//!
//! ```rust,ignore
//! #[tokio::test]
//! async fn my_backend_is_conforming() {
//!     let plugin = MyBackend::connect(&test_dsn()).await;
//!     let violations = usage_collector_sdk::contract::run_all(&plugin).await;
//!     assert!(violations.is_empty(), "{violations:#?}");
//! }
//! ```
//!
//! # Five of seven
//!
//! DESIGN §3.3's table lists seven checks. Two cannot be written against the
//! SPI as it stands, and are named here rather than silently omitted — a
//! plugin author counting five needs to know which two are missing and what
//! unblocks them. See [`BLOCKED_CHECKS`].

use crate::{UsageCollectorPluginV1, ...};

pub mod reference;

/// A check that failed, naming the check and what was observed.
///
/// Carries the check's own name so a caller can report a whole run without
/// re-deriving which assertion produced which failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractViolation {
    /// The DESIGN §3.3 check name, spelled as the table spells it.
    pub check: &'static str,
    /// What was observed, and what the check required instead.
    pub detail: String,
}

/// The two DESIGN §3.3 checks this suite does not implement, and why.
///
/// Named rather than omitted: a plugin author who counts five checks against
/// a table of seven needs to know whether the other two are missing or
/// merely renamed, and what would unblock them.
pub const BLOCKED_CHECKS: &[(&str, &str)] = &[
    (
        "feed-snapshot-and-replay",
        "the gear's SPI declares no feed method: DESIGN §3.3 gives \
         `UsageCollectorPluginV1` a `read_feed_page`, and this gear \
         implements five methods, none of which reads a feed. Unblocked by \
         the usage feed.",
    ),
    (
        "latest-tie-break",
        "asserts `greatest window_end, then greatest acceptance_sequence`, \
         and `UsageRecord` carries no `acceptance_sequence` field. DESIGN \
         §3.1 has the plugin assign it monotonically per \
         `(tenant_id, gts_type_id)`; until the field exists there is nothing \
         for a plugin to assign or a fold to read.",
    ),
];

/// Run every implemented check, returning one entry per violation.
///
/// An empty result means the plugin conforms as far as this suite reaches.
/// The checks are run in sequence rather than concurrently: several store
/// entries and then read them back, and interleaving them would let one
/// check observe another's rows.
pub async fn run_all(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    let mut violations = Vec::new();
    for outcome in [
        quantity_round_trip(plugin).await,
        // Tasks 8-10 extend this list.
    ] {
        if let Err(violation) = outcome {
            violations.push(violation);
        }
    }
    violations
}

/// `quantity-round-trip`: "The full published range round-trips digit for
/// digit, negative half included."
///
/// A backend that stores the quantity as a float, or narrows the scale, or
/// normalizes away trailing zeros, fails here. The negative half is
/// explicit because a negative quantity is an ordinary measurement
/// recording a real decrease — `usage-collector-v1.yaml` says the sign is
/// never constrained — and a backend using an unsigned column passes every
/// other check in this suite.
pub async fn quantity_round_trip(
    plugin: &dyn UsageCollectorPluginV1,
) -> Result<(), ContractViolation> {
    // ...
}
```

**Write `quantity_round_trip` in full.** It must:
1. Build records whose `value` covers the published range — the 28-digit
   maximum, its negation, a value with significant trailing zeros, and a
   sub-unit fraction. Read the exact bound from `usage-collector-v1.yaml`'s
   `UsageQuantity` and cite the line in a comment; **do not invent a bound.**
2. Persist each through `create_usage_record`.
3. Read each back through `list_usage_records` over a range containing its
   `window_end`.
4. Compare with `Decimal`'s **string** representation, not `==` —
   `Decimal::eq` may ignore scale, which is one of the things this check exists
   to catch. State that in a comment.

- [ ] **Step 3: Write the in-memory reference plugin**

Create `usage-collector-sdk/src/contract/reference.rs`:

```rust
//! A conforming in-memory backend: the subject the contract suite is
//! validated against.
//!
//! It exists so the checks in [`super`] are known to be satisfiable and
//! known to discriminate — a suite with no passing subject cannot tell a
//! broken check from a broken plugin. It is **not** a production backend and
//! not an example of how to write one: everything is a `Vec` behind a
//! `Mutex`, and it holds the whole ledger in memory.
//!
//! It is not the noop plugin. `noop-usage-collector-plugin` persists
//! nothing — it echoes creates, returns `UsageRecordNotFound` from every
//! point read and an empty page from every list — so it fails all five
//! behavioural checks by construction. That is correct behaviour for a null
//! backend used to resolve the host binding in development, and it is why
//! the suite needed a second subject.

use std::sync::Mutex;
// ...

/// An in-memory conforming backend.
#[derive(Debug, Default)]
pub struct InMemoryReferencePlugin {
    entries: Mutex<Vec<UsageRecord>>,
}
```

Implement `UsageCollectorPluginV1` for it, with these obligations — each is a
DESIGN §3.1 invariant the plugin owns:

| Method | Obligation |
| --- | --- |
| `create_usage_record` | Reject a duplicate `id` whose canonical fields differ, as `IdempotencyConflict`. Return the stored row unchanged on an identical resubmission. Reject a second invalidation of one target as `AlreadyInvalidated`, checked against the entries vector while the lock is held — that atomicity is the point of the rule. |
| `create_usage_records` | Per-entry outcomes aligned to input order. Reject an empty batch as `Internal` (host-contract breach), matching noop. |
| `get_usage_record` | Return the row **only if** it satisfies `scope`. A row that exists but fails the scope is `UsageRecordNotFound` — never a distinguishable "denied". |
| `list_usage_records` | Select on **period end**: `from <= window_end < to`. Apply `query.filter` (which is where the gear puts the compiled scope) and `metadata_filter`. Order by `(window_end, id)`. |
| `query_aggregated_usage_records` | Fold over the selected set, **excluding both halves of every withdrawn pair** — the record and the invalidation that withdrew it. |

The scope evaluator is the part worth writing carefully:

```rust
/// Evaluate a compiled PDP scope against one entry.
///
/// The gear compiles a scope into a disjunction of conjunctions over five
/// identifiers — `tenant_id`, `resource_id`, `resource_type`, `subject_id`,
/// `subject_type` (`usage-collector/src/domain/authz.rs`) — built from
/// `Compare(Identifier, Eq|Ne, Value)` and `In(Identifier, [Value])` nodes.
/// This covers that grammar and refuses the rest.
///
/// **An unrecognised node evaluates to `false`, never `true`.** A scope this
/// backend cannot interpret must exclude the row: the alternative is a
/// filter that silently widens, which is a cross-tenant read. A real backend
/// projecting to SQL owes the same posture — refuse the query rather than
/// drop the predicate.
fn scope_admits(entry: &UsageRecord, scope: &ast::Expr) -> bool {
    match scope {
        ast::Expr::And(l, r) => scope_admits(entry, l) && scope_admits(entry, r),
        ast::Expr::Or(l, r) => scope_admits(entry, l) || scope_admits(entry, r),
        ast::Expr::Not(inner) => !scope_admits(entry, inner),
        ast::Expr::Compare(lhs, op, rhs) => { /* Eq / Ne over field vs Value */ }
        ast::Expr::In(lhs, values) => { /* membership */ }
        _ => false,
    }
}

/// The scope-visible value of one identifier, or `None` if the entry does
/// not carry it (an absent `subject_ref`) or the name is not one this
/// backend knows. `None` never matches — see [`scope_admits`].
fn field_value(entry: &UsageRecord, identifier: &str) -> Option<String> {
    match identifier {
        "tenant_id" => Some(entry.tenant_id.to_string()),
        "resource_id" => Some(entry.resource_ref.resource_id().to_owned()),
        "resource_type" => Some(entry.resource_ref.resource_type().to_owned()),
        "subject_id" => entry.subject_ref.as_ref().map(|s| s.subject_id().to_owned()),
        "subject_type" => entry.subject_ref.as_ref().and_then(|s| s.subject_type()).map(ToOwned::to_owned),
        _ => None,
    }
}
```

> `ast::Value::Uuid` and `ast::Value::String` are different variants, and
> `tenant_id` compiles to a `Uuid`. Compare on the rendered string in both cases
> and say so in a comment, or match the variants explicitly — either is fine,
> but **not** a `Display` on `ast::Value`: its `Display` prints the *type name*
> (`"uuid"`, `"string"`), not the value. Read
> `libs/toolkit-odata/src/lib.rs`'s `impl Display for Value` before writing this
> comparison. This is a trap that produces a filter matching everything.

- [ ] **Step 4: Wire the SDK's own test**

Create `usage-collector-sdk/src/contract_tests.rs` and hook it from `contract.rs`
with the standard pattern:

```rust
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "contract_tests.rs"]
mod contract_tests;
```

```rust
//! The contract suite, run against the reference backend.

use super::*;
use reference::InMemoryReferencePlugin;

#[tokio::test]
async fn the_reference_backend_conforms() {
    let plugin = InMemoryReferencePlugin::default();
    let violations = run_all(&plugin).await;
    assert!(violations.is_empty(), "{violations:#?}");
}

/// The blocked list names checks DESIGN §3.3 actually declares, and does not
/// name one this suite implements.
///
/// Without this, `BLOCKED_CHECKS` is prose: a typo, or a row left behind
/// after a check became writable, reads exactly like an honest gap.
#[test]
fn the_blocked_checks_are_the_ones_not_implemented() {
    let blocked: BTreeSet<&str> = BLOCKED_CHECKS.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        blocked,
        BTreeSet::from(["feed-snapshot-and-replay", "latest-tie-break"]),
    );
    assert!(
        BLOCKED_CHECKS.iter().all(|(_, why)| !why.is_empty()),
        "a blocked check without a reason is a TODO wearing a const",
    );
}
```

- [ ] **Step 5: Run**

```bash
cargo nextest run -p cf-gears-usage-collector-sdk --features contract --no-fail-fast
cargo clippy -p cf-gears-usage-collector-sdk --all-targets --features contract
cargo nextest run -p cf-gears-usage-collector-sdk --no-fail-fast
```

Expected: the first two clean; the third confirms the default feature set still
builds and the contract module is genuinely gated.

- [ ] **Step 6: Commit**

```bash
git add gears/system/usage-collector/usage-collector-sdk/
git commit -s -m "feat(usage-collector-sdk): add the plugin contract suite harness

DESIGN 3.3 says every conforming plugin MUST pass the suite in
usage-collector-sdk. That is unenforceable from an integration tests/
directory, which no other crate can reach, so the harness is a public
feature-gated module a plugin crate calls against itself — which is also the
acceptance criteria for porting the TimescaleDB backend.

It needed a conforming subject. The noop plugin cannot be one: it persists
nothing, so it fails all five behavioural checks by construction, which is
correct for a null backend used to resolve the host binding. So this adds an
in-memory reference backend alongside, and quantity-round-trip as the first
check.

Two of DESIGN's seven checks are unimplementable against the SPI as it
stands and are named in BLOCKED_CHECKS with what unblocks each, rather than
silently omitted: feed-snapshot-and-replay has no feed method, and
latest-tie-break breaks ties on an acceptance_sequence field UsageRecord
does not carry."
```

---

## Task 8: `window-end-selection` and `dedup-identity-over-window`

**Files:**
- Modify: `usage-collector-sdk/src/contract.rs`
- Modify: `usage-collector-sdk/src/contract/reference.rs` (only if a check exposes a real gap)

- [ ] **Step 1: Write `window_end_selection`**

DESIGN: "A range selects by period end, exclusive at the upper bound. A point
event needs no special case. An entry wider than the range is selected by neither
side."

Three entries and three ranges, and **each of the three assertions must be able
to fail alone**:

```rust
/// `window-end-selection`: a range selects on the end of an entry's covered
/// period, `[from, to)`.
///
/// The three cases are one check because they are one rule seen from three
/// sides, and each catches a different plausible backend: selecting on
/// `window_start` (a backend that ported the old point-in-time column),
/// selecting inclusively at `to` (an off-by-one in a `BETWEEN`), and
/// selecting an entry that merely *overlaps* the range (an interval-overlap
/// predicate, which is the natural thing to write and the wrong thing).
pub async fn window_end_selection(
    plugin: &dyn UsageCollectorPluginV1,
) -> Result<(), ContractViolation> {
    // ...
}
```

Cases, all over one meter and one tenant so nothing else can select them:

1. **Selected by end, not by start.** An entry with `window_start` **before**
   `from` and `window_end` **inside** `[from, to)` is returned. A backend
   selecting on `window_start` misses it.
2. **Exclusive upper bound.** An entry whose `window_end` is exactly `to` is
   **not** returned; one whose `window_end` is exactly `from` **is**.
3. **Wider than the range, selected by neither side.** An entry with
   `window_start < from` and `window_end >= to` is not returned by a range
   strictly inside it.
4. **A point event.** `window_start == window_end`, inside the range, is
   returned — with no special case, i.e. by the same predicate as the rest.

> Case 2's two halves must be asserted separately. A single "the boundary
> behaves" assertion that ORs them passes against a backend that gets both wrong
> in compensating directions.

- [ ] **Step 2: Write `dedup_identity_over_window`**

DESIGN: "Both period bounds are part of the identity, so a same-key submission
over a different period is a distinct entry."

```rust
/// `dedup-identity-over-window`: the covered period is part of an entry's
/// identity, so the same idempotency key over a different period is a
/// distinct entry rather than a duplicate.
///
/// The check drives the SPI, so it derives both identifiers through
/// [`crate::derive_usage_record_id`] the way the gear does and asserts the
/// backend stores two rows. A backend keying dedup on
/// `(tenant, type, idempotency_key)` alone — the obvious schema, and the one
/// the pre-period model had — absorbs the second submission and returns the
/// first row. It would pass every other check here.
pub async fn dedup_identity_over_window(
    plugin: &dyn UsageCollectorPluginV1,
) -> Result<(), ContractViolation> {
    // ...
}
```

Assert **both** halves:
- Same key, **different** period → two entries, both readable, with the two
  distinct ids `derive_usage_record_id` produces.
- Same key, **same** period, identical canonical fields → the stored row comes
  back, and `list_usage_records` shows **one** entry, not two.

> The second half is what stops the check passing against a backend that dedups
> nothing at all. Without it, "two submissions produced two rows" is satisfied by
> a plugin with no dedup whatsoever.

- [ ] **Step 3: Add both to `run_all` and run**

```bash
cargo nextest run -p cf-gears-usage-collector-sdk --features contract --no-fail-fast
```

Expected: PASS. If the reference plugin fails a case, **fix the reference
plugin** — that is the check doing its job. Report which case and what was wrong.

- [ ] **Step 4: Commit**

```bash
git add gears/system/usage-collector/usage-collector-sdk/
git commit -s -m "feat(usage-collector-sdk): contract checks for period-end selection and dedup identity

window-end-selection covers the rule from the three sides that catch
different plausible backends: selecting on window_start, an inclusive upper
bound, and an interval-overlap predicate — which is the natural thing to
write and the wrong thing. The point-event case asserts it needs no special
arm.

dedup-identity-over-window asserts both halves. A same-key submission over a
different period is a distinct entry, and a same-key submission over the
same period is not: the first alone passes against a backend that dedups
nothing, and the second alone passes against the pre-period schema that keys
on (tenant, type, idempotency_key)."
```

---

## Task 9: `invalidation-excluded-from-fold` and `at-most-one-invalidation`

**Files:**
- Modify: `usage-collector-sdk/src/contract.rs`

- [ ] **Step 1: Write `invalidation_excluded_from_fold`**

DESIGN: "A withdrawn pair folds to nothing while both entries stay readable.
Excluding only the record double-counts the withdrawn measurement."

That second sentence is the check's real content and is easy to lose:

```rust
/// `invalidation-excluded-from-fold`: a withdrawn pair contributes nothing
/// to a fold, while both entries stay readable on the raw path.
///
/// Both halves are the rule. Excluding the record but folding the
/// invalidation double-counts the withdrawn measurement with its sign
/// flipped; excluding both from the *raw* path instead would make the ledger
/// unauditable, and the append-only correction model
/// (`cpt-cf-usage-collector-adr-append-only-invalidation`) exists precisely
/// so a withdrawal is visible rather than a deletion.
///
/// Asserted against a surviving entry, not against zero. A `SUM` over a
/// withdrawn pair alone is zero for a backend that folds correctly *and* for
/// one that returns no buckets at all, which is what the noop backend does —
/// so the fixture carries one live entry whose value the fold must equal.
pub async fn invalidation_excluded_from_fold(
    plugin: &dyn UsageCollectorPluginV1,
) -> Result<(), ContractViolation> {
    // ...
}
```

Fixture: one live record with a distinctive value, plus a second record and the
invalidation that withdraws it. Then:
- `SUM` over the range equals the live entry's value **exactly** — not zero, not
  the live value plus or minus the withdrawn one.
- `list_usage_records` over the same range returns **three** entries, and the
  invalidation among them names its target.

- [ ] **Step 2: Write `at_most_one_invalidation`**

DESIGN: "A second withdrawal of one record is rejected. Under two concurrent
submissions exactly one succeeds."

```rust
/// `at-most-one-invalidation`: a record carries at most one withdrawal, and
/// the check is the store's because only the store can make it atomic with
/// the entry it admits
/// (`cpt-cf-usage-collector-adr-append-only-invalidation`).
///
/// The concurrent half is the part a gateway pre-read cannot supply and is
/// therefore the part worth driving: a backend that checks and then inserts
/// non-atomically passes the sequential case and admits both under a race.
pub async fn at_most_one_invalidation(
    plugin: &dyn UsageCollectorPluginV1,
) -> Result<(), ContractViolation> {
    // ...
}
```

- **Sequential:** the second withdrawal is rejected as
  `UsageCollectorPluginError::AlreadyInvalidated`, carrying the id of the
  invalidation that already withdrew the target. Assert the **variant**, not just
  `is_err()` — a backend rejecting it as `Internal` is not conforming.
- **Concurrent:** submit two withdrawals of one target through
  `create_usage_records` in a single batch — the SPI's per-entry outcomes make
  the race expressible without a runtime-dependent `join!`. Assert **exactly
  one** `Ok` and exactly one `AlreadyInvalidated`. State in a comment that this
  is a same-batch race, and that a backend serialising a batch entry-by-entry
  satisfies it while one checking the whole batch against pre-existing state does
  not — which is the defect.

> Do not write the concurrent case with `tokio::join!` over two
> `create_usage_record` calls. It is timing-dependent, it would be the flake
> trade this gear has declined elsewhere, and the batch form tests the same
> invariant deterministically. If you believe the batch form does not reach the
> race, say so and stop — do not substitute a timing test.

- [ ] **Step 3: Add both to `run_all` and run**

```bash
cargo nextest run -p cf-gears-usage-collector-sdk --features contract --no-fail-fast
```

- [ ] **Step 4: Commit**

```bash
git add gears/system/usage-collector/usage-collector-sdk/
git commit -s -m "feat(usage-collector-sdk): contract checks for the fold exclusion and the at-most-one rule

invalidation-excluded-from-fold asserts against a surviving entry rather
than against zero: a SUM over a withdrawn pair is zero both for a backend
that folds correctly and for one that returns no buckets at all. It also
asserts both entries stay readable on the raw path, which is what makes the
append-only correction model auditable rather than a deletion.

at-most-one-invalidation drives the concurrent half through a single batch,
where the SPI's per-entry outcomes make the race expressible without a
timing-dependent join. A backend that checks and then inserts non-atomically
passes the sequential case and admits both under a race."
```

---

## Task 10: The scope-enforcement check

Slice 3's Task 13 retired the point lookup's in-process per-record attribution
check, per DESIGN §3.2 and §3.3, putting `get_usage_record` on the same
scope-as-filter posture as the raw and aggregate paths. The "exists but not yours
reads as `NotFound`" guarantee now rests **entirely** on each storage plugin
intersecting the filter it is handed.

**No test anywhere exercises a plugin filtering out a real stored row that fails a
non-trivial compiled scope.** Every plugin double in
`usage-collector/src/domain/test_support.rs` takes the scope as
`_scope: &ast::Expr` — underscore-prefixed, ignored. `TargetLookupDouble` records
it (`last_get_scope()`, used to assert the gateway *passes* a compiled scope) but
never filters by it. A plugin returning a row that fails its own scope filter
passes every test in the tree.

One check closes it for all three read paths, because the scope reaches
`get_usage_record` as an argument and reaches `list_usage_records` /
`query_aggregated_usage_records` inside `query.filter`.

**Files:**
- Modify: `usage-collector-sdk/src/contract.rs`

- [ ] **Step 1: Write it**

```rust
/// Scope is a filter, on every read path.
///
/// Not one of DESIGN §3.3's seven names. It is here because §3.2 and §3.3
/// moved the point lookup off an in-process per-record attribution check and
/// onto the same scope-as-filter posture the raw and aggregate paths already
/// had, which put the whole "exists but not yours reads as `NotFound`"
/// guarantee inside the plugin — and nothing was checking it.
///
/// **The scope must be non-trivial.** A scope that excludes every row is
/// satisfied by a backend that returns nothing, and a scope that excludes
/// none is satisfied by a backend that ignores it. So the fixture stores two
/// entries differing only in `tenant_id` and hands over a scope admitting
/// exactly one, and every assertion is a pair: the admitted entry is
/// returned **and** the excluded one is not.
///
/// The point read must answer `UsageRecordNotFound` for the excluded row —
/// not a distinguishable denial. A backend that separates "not yours" from
/// "not here" leaks the existence of another tenant's entry to anyone who
/// can guess a uuid.
pub async fn scope_is_a_filter_on_every_read_path(
    plugin: &dyn UsageCollectorPluginV1,
) -> Result<(), ContractViolation> {
    // ...
}
```

Three paired assertions, over a scope of the shape `domain/authz.rs` compiles —
a disjunction of conjunctions, each conjunction tenant-pinned. Build one with a
second conjunct (say `resource_type`) so it is not a bare `tenant_id eq`:

1. `get_usage_record(admitted.id, &scope)` → `Ok`, and
   `get_usage_record(excluded.id, &scope)` → `Err(UsageRecordNotFound)`.
2. `list_usage_records` with the scope in `query.filter` returns the admitted
   entry and **not** the excluded one.
3. `query_aggregated_usage_records` with the same filter folds the admitted
   entry's value and not the excluded one's — assert the **value**, so a backend
   returning an empty bucket set fails.

- [ ] **Step 2: Add to `run_all`, and record the check in `BLOCKED_CHECKS`'s neighbourhood**

This check is not in DESIGN's table. Add a `///` paragraph to the module header
saying so and why, so a plugin author does not go looking for it in DESIGN §3.3
and conclude the suite has drifted.

- [ ] **Step 3: Run and falsify**

```bash
cargo nextest run -p cf-gears-usage-collector-sdk --features contract --no-fail-fast
```

Then confirm the check discriminates, since this is the one closing a gap that
existed *because* nothing discriminated:

```bash
SCRATCH="$(mktemp -d)"
R=/Users/binarycode/code/virtuozzo/gears-rust/gears/system/usage-collector/usage-collector-sdk/src/contract/reference.rs
cp "$R" "$SCRATCH/reference.rs"
# Make the reference backend ignore the scope on the point read.
```

Edit `get_usage_record` to return the row without consulting `scope_admits`,
**grep the mutated line to confirm the edit landed**, run, and expect
`scope_is_a_filter_on_every_read_path` to fail on assertion 1. Restore with `cp`
and `touch`.

- [ ] **Step 4: Commit**

```bash
git add gears/system/usage-collector/usage-collector-sdk/
git commit -s -m "feat(usage-collector-sdk): contract check that scope is a filter on every read path

DESIGN 3.2 and 3.3 moved the point lookup off an in-process per-record
attribution check onto the same scope-as-filter posture the raw and
aggregate paths had, which put the whole exists-but-not-yours-reads-as-
NotFound guarantee inside the plugin. Nothing was checking it: every plugin
double in the gear takes the scope as _scope and ignores it, and the one
that records it asserts only that the gateway passes a compiled scope, never
that a plugin honours one.

The scope is non-trivial in both directions — two entries differing only in
tenant_id, a tenant-pinned scope with a second conjunct, and every assertion
paired so that returning nothing and ignoring the scope both fail. The point
read must answer UsageRecordNotFound for the excluded row rather than a
distinguishable denial, which would leak another tenant's entry to anyone
who can guess a uuid.

Not one of DESIGN 3.3's seven names, and the module header says so."
```


---

## Task 11: Prove every check discriminates

**This task is the seam, and it is not optional.** Tasks 7-10 produced a
reference plugin and a set of checks written alongside it — two layers tested
against themselves, which is the exact shape of slice 5's sharpest defect (a REST
route's binding to its own handler, invisible with 700 tests green because route
tests and handler tests each checked their own side).

A green `the_reference_backend_conforms` proves the suite runs. It does not prove
any check would notice a non-conforming plugin. The TimescaleDB port is going to
be accepted on this suite, so a check that cannot fail is worse than a missing
one.

**Files:**
- Modify: `usage-collector-sdk/src/contract_tests.rs`

- [ ] **Step 1: Write one mutant per check**

Each mutant is the reference plugin wrong in **exactly one** way. A mutant that
fails three checks proves nothing about which check caught it, so each is a thin
wrapper delegating everything else:

```rust
/// A backend that is the reference backend with one rule broken.
///
/// One rule each, deliberately. A mutant wrong in two ways fails two checks
/// and proves neither of them was the one that noticed — the suite would
/// look discriminating while one of its checks did nothing.
///
/// Each `Defect` is written as the *plausible* wrong implementation, not an
/// absurd one. A backend that returns garbage is caught by anything; the
/// question this test answers is whether the suite catches the mistake
/// someone would actually make.
enum Defect {
    /// Stores the quantity through an `f64`. The mistake a backend makes by
    /// choosing a `double precision` column.
    QuantityThroughFloat,
    /// Selects on `window_start` instead of `window_end`. The mistake a
    /// backend makes by porting the pre-period point-in-time column.
    SelectsOnWindowStart,
    /// Keys dedup on `(tenant, type, idempotency_key)`, omitting the period
    /// bounds. The pre-period identity.
    DedupIgnoresThePeriod,
    /// Excludes the withdrawn record from the fold but folds the
    /// invalidation. DESIGN names this one explicitly: it double-counts the
    /// withdrawn measurement.
    FoldsTheInvalidation,
    /// Checks for an existing invalidation, then inserts, without holding
    /// the lock across both.
    ChecksThenInsertsTheInvalidation,
    /// Honours the scope on the list and aggregate paths and ignores it on
    /// the point read. The asymmetry slice 3 created and nothing checked.
    IgnoresScopeOnThePointRead,
}
```

- [ ] **Step 2: Write the discrimination test**

```rust
/// Every check fails against a backend that gets its rule wrong, and passes
/// against every other backend.
///
/// The second half is what makes this a test of *discrimination* rather than
/// of sensitivity. A check that fails against all six mutants is not
/// detecting its own rule; it is detecting that something is different. So
/// each row asserts a full column: the named check fails, and the other five
/// still pass against the same mutant.
#[tokio::test]
async fn each_check_fails_against_its_own_defect_and_no_other() {
    for (defect, expected_check) in [
        (Defect::QuantityThroughFloat, "quantity-round-trip"),
        (Defect::SelectsOnWindowStart, "window-end-selection"),
        (Defect::DedupIgnoresThePeriod, "dedup-identity-over-window"),
        (Defect::FoldsTheInvalidation, "invalidation-excluded-from-fold"),
        (Defect::ChecksThenInsertsTheInvalidation, "at-most-one-invalidation"),
        (Defect::IgnoresScopeOnThePointRead, "scope-is-a-filter"),
    ] {
        let plugin = MutantPlugin::new(defect);
        let violations = run_all(&plugin).await;

        let failed: BTreeSet<&str> = violations.iter().map(|v| v.check).collect();
        assert_eq!(
            failed,
            BTreeSet::from([expected_check]),
            "defect {defect:?} must be caught by `{expected_check}` and by \
             nothing else; a check that fires on every defect is detecting \
             difference, not its own rule",
        );
    }
}
```

> **If a mutant trips more than its own check, do not relax the assertion to
> `contains`.** Either the mutant is wrong in more than one way — narrow it — or
> two checks genuinely overlap, in which case say which two and why in a comment
> and make the expected set name both. What you must not do is weaken the
> assertion until it passes; that reproduces the defect this task exists to
> prevent.

> **If a check cannot be made to fail against any plausible defect, say so
> rather than shipping it.** A check that passes vacuously is worse than a
> documented gap, because the TimescaleDB port will be accepted on it. Report it
> to the controller as a `DONE_WITH_CONCERNS`.

- [ ] **Step 3: Run**

```bash
cargo nextest run -p cf-gears-usage-collector-sdk --features contract --no-fail-fast
```

Expected: PASS, including `each_check_fails_against_its_own_defect_and_no_other`.

- [ ] **Step 4: Commit**

```bash
git add gears/system/usage-collector/usage-collector-sdk/src/contract_tests.rs
git commit -s -m "test(usage-collector-sdk): prove each contract check discriminates

The suite and the reference backend were written alongside each other, which
is two layers tested against themselves: a green run proves the suite runs,
not that any check would notice a non-conforming plugin. The TimescaleDB
port is going to be accepted on this suite, so a check that cannot fail is
worse than a missing one.

Six mutants, each the reference backend wrong in exactly one plausible way —
a float quantity column, selection on window_start, the pre-period dedup
identity, folding the invalidation, a check-then-insert on the at-most-one
rule, and honouring the scope everywhere but the point read.

Each row asserts a full column: the named check fails and the other five
still pass. A check that fires on every mutant is detecting difference
rather than its own rule, and would look discriminating while doing
nothing."
```

---

## Task 12: Record what moved, and what did not

**Files:**
- Modify: `DIVERGENCES.md` (repository root)
- Modify: `gears/system/usage-collector/usage-collector-sdk/src/error.rs` (one doc paragraph)

- [ ] **Step 1: Rewrite entry 9**

9(a) is **closed in code**. Rewrite it to say what closed and what remains —
`NotFoundReason` is gear-internal, so a *client* still cannot separate the three
404s. Strike 9(a) the way slice 5 struck the `origin` sub-item: struck rather
than rewritten, because nothing about the old text survives.

9(b) is **not closed and is not being closed.** Two documented `error_category`
values still cannot fire and three emitted ones are still undocumented. Update
the sentence about `invalidation_rule` under-counting the reference rule — that
one is now false — and leave the rest, including the period-bound paragraph.
Say explicitly that slice 6 declined the label rename, and why: moving
`semantics_violation` to `validation` breaks every dashboard grouping on it, and
the entry's own "Why the code is right" argues against it.

- [ ] **Step 2: Strike item D**

The round-trip table now has a `stringify!` companion pinning all spellings.
Record that it landed and that the table shrank from 18 rows to 16 when the two
cursor codes left.

- [ ] **Step 3: Add the new entries**

Number them from 17 (confirm with `grep -c '^## [0-9]' DIVERGENCES.md`, which read
**16** when this plan was written).

**17. `$top` and `metadata.<key>` are served and undocumented.** The gear
registers both on `GET /records`, with its reasoning at the registration site;
`usage-collector-v1.yaml` documents neither. Load-bearing: a client generated
from the contract cannot set a page size by the canonical OData spelling and does
not know the metadata filter exists. Names `UNDOCUMENTED_PARAMETERS` in
`openapi_contract_tests.rs` as where it is pinned, and that the pin expires the
row automatically when the yaml catches up. Proposed wording: document both.

**18. `UsageCollectorPluginError` ships five of DESIGN's six variants.**
`CursorBeyondRetention { oldest_available }` is absent. It is the feed's
replay-refusal signal, the feed is not on the SPI, and its `error_category`
mapping (`cursor_beyond_retention` on `uc_feed_requests_total`) has no emitter
either. Decision: land it with the feed rather than ship a variant nothing
constructs and no test can exercise. Not load-bearing today — nothing can raise
it, so nothing mis-reports — but it becomes so the moment the feed lands without
it.

**Extend entry 10 with a third instance, found during Task 2's review.** Entry 10
records that a generated client cannot submit a record (`quantity` vs `value`)
and that the emitted `UsageRecord` is not an instance of its declared schema.
The **aggregate request** is a third case of the same defect, on a third
operation: `usage-collector-v1.yaml:1045` declares `AggregationRequest` with
`required: [gts_type_id, time_range]` and `additionalProperties: false`, while
`AggregationRequest` in `api/rest/dto.rs` carries `#[serde(deny_unknown_fields)]`
and **no `gts_type_id` field** — the gear takes it as a query parameter. So a
client generated from the contract sends `gts_type_id` in the body and is
refused as an unknown field, *and* omits the query parameter the gear requires.
Two independent 400s, exactly entry 10's shape.

**Nothing in the drift gate catches this, by design.** `body_schemas_match`
compares content type, `required` and schema *name*; the suite's module header
lists field-level schema contents under what it deliberately does not enforce.
Task 3 turning the gate on does not close it and must not be described as
though it does. Record it under entry 10 rather than as a new entry — same
defect, same resolution, and entry 10 already says the fix is a spec decision
plus a scheduled slice rather than an editorial pass.

**Entry 8's plugin half is closed by Task 7 — say so, and say what is still
open.** Entry 8 records that the 28-digit quantity guarantee has "no enforcement
and no test", and names this slice's `quantity-round-trip` as where the plugin
half lands. It now exists. What entry 8 says remains unowned is the **gateway**
half — rejecting an out-of-range submission at ingestion, which DESIGN §3.3
explicitly promises a plugin it need not do — and this slice did not build it,
deliberately, because entry 8 says not to. Update the entry to distinguish the
two halves rather than marking it resolved: a contract check proves a *plugin*
round-trips the range, and proves nothing about what the gear admits.

**Two gaps in the gate itself, to state inside the entry-10 extension rather
than leave in code comments only:**

- The cross-check that keeps a placement disagreement out of
  `UNDOCUMENTED_PARAMETERS` **matches on the property name alone.** The
  `$filter` row is the one case it provably cannot police, because the query
  spelling (`$filter`) and the body spelling (`filter`) differ — re-file that
  row into the wrong list and the suite stays green. It is correctly filed
  today and the assertion message says so, which is the most a name-matching
  check can do. Say it in the entry so the limit is on record outside a
  comment.
- **`DIVERGENCES.md:990-995` carries an unpinned evidence line** — "Re-verified
  at the branch head … 648 passed, 6 skipped" — that, unlike the three
  commit-pinned lines around it (`:935`, `:959`, `:975`), names no commit. It
  was true when written and is false now; this slice alone moved the count
  twice. Pin it to a commit or delete it. Do not simply update the number: an
  unpinned count re-breaks on the next commit, which is why the other three
  are pinned.

**Fold into the existing entries 5 and 14, do not open a new one:** Task 2's
rename left `docs/DECOMPOSITION.md:573` and `docs/features/usage-query.md:127,139`
asserting `QueryAggregatedUsageRecordsRequest` is the implemented request body.
That is now false. Both files are edit-forbidden for this slice and both are
already recorded as stale wholesale — entry 5 for `DECOMPOSITION.md`, entry 14
for `docs/features/*`. Add the concrete instance to whichever entry owns the
file, with the line numbers, rather than opening a twentieth entry for a
known-stale document. Verify the line numbers still hold when you write it.

**A second candidate check, also from Task 7: `group-by-absent-dimension`.**
The reference backend drops a row with no `subject_ref` from a
`GROUP BY subject_id` entirely, where naive SQL would put it in a NULL group —
so an exemplar and a SQL projection give different sums, and the buckets do not
add up to the ungrouped total. DESIGN says nothing about an absent dimension,
and `usage-collector-v1.yaml:1092-1101` types key items as non-nullable
`string`, so dropping may be the only *representable* answer. It is now in the
reference backend's stated limits, but no check pins it and nothing records
which answer is right. Writable against today's SPI; a spec question first.

**A candidate check the suite does not have, surfaced by Task 7.** The
reference backend serves the canonical `(window_end, id)` order and mints no
`next_cursor`, so nothing in the suite exercises the SPI's keyset obligations.
That matters more than it looks: `require_cursor_fingerprint` in
`usage-collector/src/domain/query.rs` states that carrying `query.filter_hash`
into `next_cursor.f` is **"the one requirement in this gear's Plugin SPI that
gives an implementor no compiler error — a plugin written before it recompiles
clean and paginates exactly once"**. An obligation with no compiler backstop and
no contract check is the strongest candidate for the next check, stronger than
some of DESIGN's seven. Record it; do not build it in this slice.

Consequence for wording: `the_reference_backend_conforms` passing means the
backend satisfies *this suite*, not that it is a conforming plugin. Neither the
harness docs nor the entry may say otherwise.

**Task 7's `LATEST` fold substitutes greatest `id` for the declared tie-break**,
because `acceptance_sequence` does not exist. It is documented at the fold, and
it belongs in entry 19 beside `latest-tie-break` — same root cause, and the
substitution is *not* the declared rule.

**19. Two DESIGN §3.3 contract checks are unwritable.**
`feed-snapshot-and-replay` and `latest-tie-break`, with what unblocks each.
Cross-reference `BLOCKED_CHECKS` in `usage-collector-sdk/src/contract.rs` as the
in-code record, and entry 10, which already carries `accepted_at` /
`acceptance_sequence` as blocking a conformant storage plugin. **This is the
second slice to stand next to that hole and flag it** — say so, so the third
does not read two independent flags as two independent problems.

- [ ] **Step 4: Sweep for claims the slice falsified**

Not a numeral search — **grep for the members**. Every one of these is a sentence
some file states that this slice made untrue:

```bash
cd /Users/binarycode/code/virtuozzo/gears-rust
# The cursor codes: any file still saying the gear defines or raises them.
grep -rn 'INVALID_CURSOR\|FILTER_MISMATCH' --include='*.rs' --include='*.md' \
  gears/system/usage-collector DIVERGENCES.md | grep -v timescaledb
# The NotFound discriminator: any file still saying the category carries no reason.
grep -rn 'carries no typed reason\|no wire `context.reason`\|parse_str' \
  --include='*.rs' gears/system/usage-collector/usage-collector/src
# The drift gate: any file still saying the six checks are ignored.
grep -rn 'ignore\]\|Phase 2 documentation' --include='*.rs' --include='*.md' \
  gears/system/usage-collector
# The contract suite: any file still saying it runs against the noop plugin.
grep -rn 'noop' --include='*.md' docs/superpowers/specs docs/superpowers/plans
# Enumerations one member short — the count with no number in it.
grep -rn 'five variants\|5 variants\|six variants\|seven checks\|five checks' \
  --include='*.rs' --include='*.md' gears/system/usage-collector
```

For each hit: **replace the sentence, do not reword it**, and remember it is
usually in three places. `docs/DESIGN.md`, `usage-collector-v1.yaml`,
`DECOMPOSITION.md` and `docs/features/*` are out of scope — a stale sentence
there is a `DIVERGENCES.md` row, not an edit.

- [ ] **Step 5: Full verification bar**

```bash
cd /Users/binarycode/code/virtuozzo/gears-rust
cargo check --workspace --all-targets
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk \
  -p cf-gears-noop-usage-collector-plugin --no-fail-fast
cargo nextest run -p cf-gears-usage-collector-sdk --features contract --no-fail-fast
cargo clippy --workspace --all-targets --all-features
cargo clippy -p cf-gears-usage-collector-sdk --all-targets --features contract
cargo +nightly fmt
cargo doc --no-deps -p cf-gears-usage-collector-sdk -p cf-gears-usage-collector 2>&1 | grep 'generated'
```

Report:
- the exact `N tests run: N passed, N skipped` line, **unfiltered**. **Skipped
  must be 0.**
- the `generated N warnings` line for each crate — **read that line, do not
  `grep -c '^warning:'`**, which over-counts by one because rustdoc's summary
  line begins with `warning:`. Host was 35, SDK was 0. Neither may grow.
- `git diff --stat` — **must show zero files under `plugins/timescaledb*`**.
- `git status --short` — must show exactly the five pre-existing items
  (the deleted plan, `NEXT-SLICE.md`, `SLICE4.md`, `SLICE5.md`, `SLICE6.md`) and
  nothing else uncommitted.

- [ ] **Step 6: Commit**

```bash
git add DIVERGENCES.md gears/system/usage-collector/usage-collector-sdk/src/error.rs
git commit -s -m "docs(usage-collector): record what slice 6 closed and what it opened

Entry 9(a) closed in code: NotFound carries a typed reason, so
invalidation_rule covers the ADR's valid-reference rule and the metric no
longer under-counts a correction backlog. Struck rather than rewritten,
since nothing of the old text survives. The wire half remains — 404 has no
context.reason slot — and the entry says so rather than reading as closed.

9(b) is not closed and was deliberately not closed: giving the period bounds
a validation category would move a metric label an operator's dashboard
already groups by, and the entry's own reasoning argues against the rename.

Item D struck: every wire spelling is pinned to its identifier, and the
table moved from 18 rows to 16 when the cursor codes left.

Three new entries. 17: \$top and metadata.<key> are served and undocumented,
pinned by the drift gate's self-expiring list. 18: the plugin error taxonomy
ships five of six, CursorBeyondRetention deliberately deferred to the feed
rather than shipped as a variant nothing constructs. 19: two DESIGN 3.3
contract checks are unwritable, and this is the second slice to flag the
same missing fields — recorded as one problem, not two."
```

---

## Self-review — run this before dispatching Task 1

Checked against the spec, `DESIGN.md` §3.3 / §3.11.5, `DIVERGENCES.md` and the
slice-6 handoff.

**Spec coverage.** Spec §3.13's four paragraphs: `ValidationReason` losses are
already done (slices 2-4); its gains — `FUTURE_WINDOW`, `PAST_WINDOW`, the
faithful-copy reasons — are present today and pinned by Task 1;
`CURSOR_BEYOND_RETENTION` is decision 1 and recorded by Task 12; the "gear does
not define" rule is Tasks 4-5; `ConflictReason`'s shape is already correct;
`UsageCollectorPluginError`'s six is decision 1. §6's contract-suite sentence is
Tasks 7-11, with its "against the noop plugin" clause overridden by decision 5
and the reason stated. §6's OpenAPI-drift-gate paragraph is Task 3.

**Not covered, deliberately, each with a decision or an entry behind it:**
`CursorBeyondRetention` (decision 1, entry 18); `accepted_at` /
`acceptance_sequence` (decision 4, entries 10 and 19); the 9(b) label rename (out
of scope, entry 9); the yaml's parameter gap (out of scope, entry 17); the two
blocked contract checks (entry 19).

**Type consistency.** `NotFoundReason` is spelled identically in Tasks 6 and 12.
`CursorRejected { source, detail }` is spelled identically in Tasks 4, 5 and 12.
`AggregationRequestDto` in Task 2 is the name Task 3's drift run expects.
`ContractViolation { check, detail }` in Task 7 is what Task 11's
`violations.iter().map(|v| v.check)` reads. `NOT_YET_IMPLEMENTED` and
`UNDOCUMENTED_PARAMETERS` are spelled identically in Task 3 and Task 12.

**One inconsistency was found by this review and fixed inline:** Task 6's Step 1
test had spelled `NotFound`'s identifier field `resource_name`, which is
`InvalidArgument`'s spelling; the variant carries `name`
(`usage-collector-sdk/src/error.rs:94`). Corrected in the code block and in the
note beneath it — both places, rather than one place plus a caveat.

**Two file locations were verified rather than assumed:**
`usage-collector/src/infra/sdk_error_mapping_tests.rs` exists (Task 4), and
`classify_record_error`'s cases live in
`usage-collector/src/domain/service_metrics_tests.rs` (Task 6).

**Ordering.** Task 1 pins spellings before Tasks 4-6 move them. Task 2's rename
lands before Task 3 measures the drift set. Task 4 adds before Task 5 deletes, so
the host crate compiles at both commits, and **Task 4 owes the first in-place
run**. Tasks 7-11 touch only the SDK and are independent of 1-6 — they may be
reordered against them, but 11 must follow 7-10.

---

## Execution handoff

Plan complete and saved to
`docs/superpowers/plans/2026-09-09-usage-collector-errors-and-contract-gate.md`.

**REQUIRED SUB-SKILL:** Use superpowers:subagent-driven-development — fresh
implementer subagent per task, two-stage review after each (spec compliance
first, then code quality), continuous execution without checking in between
tasks.

Suggested models: Tasks 1, 2 and 5 are mechanical against a complete spec. Tasks
3, 4, 6, 8, 9 and 10 are integration work. Tasks 7, 11 and 12 need design
judgment and the broadest context — give them the most capable model available.
