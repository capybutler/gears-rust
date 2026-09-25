# Usage Collector Correction Model Implementation Plan (slice 4)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the usage-collector's mutate-in-place correction model — a `status` latch, a `corrects_id` compensation reference and a `deactivate_usage_record` operation — with an append-only one: a correction is an ordinary ingested entry that carries `invalidates` and `reason_code`, is a faithful copy of the entry it withdraws, and is never rewritten.

**Architecture:** `invalidates` alone decides an entry's kind, so `entry_type` is *derived* — a method on the record and a projection on the wire, never a stored field and never submitted, which is what stops a marker disagreeing with the payload it marks. The ingestion gateway enforces the ADR's five rules before dispatch — explicit reference, valid reference, no-invalidation-of-an-invalidation, faithful copy naming the field that differs, and reason code (an earlier draft substituted "quantity echoed" for "reason code", which kept the count while changing the membership: the echo is a *consequence* of faithful copy, not a rule of its own); the sixth — at most one invalidation per record — belongs to the store, because only the store can make it atomic. Ledger reads return both entries of a withdrawn pair as persisted; excluding them is the fold's job, which is a plugin obligation stated on the SPI.

**Tech Stack:** Rust 2024, `time` 0.3.55, `uuid` v5, `rust_decimal`, `toolkit-odata` (`ODataQuery` / `ODataFilterable`), `toolkit-macros` (`api_dto`, `domain_model`), `cargo nextest`.

---

## Read this before you touch anything

### Authoritative documents

Read the sections named, not the whole corpus.

| Document | What to read |
| --- | --- |
| `gears/system/usage-collector/docs/ADR/0010-cpt-cf-usage-collector-adr-append-only-invalidation.md` | **All of it.** "Five rules at the gateway, one at the store", "Fold exclusion over both entries of the pair", "Consequences", and "Confirmation" are normative for this slice. |
| `gears/system/usage-collector/docs/ADR/0007-cpt-cf-usage-collector-adr-record-identity-derivation.md` | The section stating that **entry type is excluded** from the derivation. An invalidation derives its `id` from the same five inputs as its target and differs only through its own idempotency key. |
| `gears/system/usage-collector/docs/DESIGN.md` | §3.1 "Value Objects and Invariants" — the eight rows this slice implements (faithful copy, echo-not-compensation, both-or-neither, at-most-one-invalidation, no-invalidation-of-an-invalidation, permanence, withdrawal exclusion, append-only). §3.1 "Field ownership" table. §3.3 (SDK trait, Plugin SPI, error taxonomy). §3.6 "Invalidate Usage Record". §3.11.5 (metric label vocabularies). |
| `gears/system/usage-collector/docs/usage-collector-v1.yaml` | `EntryType` (~line 502), `ReasonCode` (~665), `UsageRecord` (~719), `CreateUsageRecordRequest` (~810) and its `dependentRequired` block, and the `Filter` parameter (~440). |
| `gears/system/usage-collector/docs/PRD.md` | `fr-record-invalidation`, `fr-invalidation-reason-code`. |

**Do NOT implement from these — they are stale and describe a deleted model:**
`docs/DECOMPOSITION.md`, `docs/features/*.md`. Both still describe the usage-type catalog, event deactivation and compensation.

### Non-negotiable ground rules

These are slice 3's rules, still current, plus what slice 3's review added for this slice. Every one of them cost a review cycle already.

1. **Distrust every code sketch in this plan.** Sketches were written against the tree at `74e3d4429` and are a starting point, not a specification. Verify each API against the real source before using it. If a sketch does not match reality, **push back and say so in your report** — do not bend code to make a sketch compile. Slice 3's plan shipped two vacuous test assertions, an implied-infallible signature, a truncation bug and one prescribed mutation that was an equivalent mutant whose stated hazard was exactly inverted.
2. **Falsify every task before reporting done.** After the tests pass: deliberately weaken the branch the task exists to protect, confirm the corresponding test fails, restore, force a genuine rebuild, re-verify. Report the falsification honestly, **including anything that did not fail when you expected it to**. Three distinct false-pass mechanisms have bitten this branch:
   - Restoring a file with `mv` preserves its mtime, so cargo skips the rebuild. Use `cp`, then `touch`, and confirm a `Compiling cf-gears-…` line.
   - A `Compiling` line is necessary but **not sufficient**. A mutation script using a **relative** path silently edits nothing after a working-directory reset, and the `Compiling` line still appears because the preceding `touch` already invalidated the artifact. Use **absolute paths**, and **`grep` the mutated line to confirm the mutation is in the file** before believing any pass.
   - A per-mutation count taken under a nextest `-E` filter is a **lower bound**. One mutation measured 13 failures filtered and 21 unfiltered; the 8 it hid included two full-stack handler tests flipping 200 to 400. Use `--no-fail-fast`, unfiltered, for any number you report.
   - **Restore from a snapshot, never from git.** A fourth false-pass mechanism, found live in task 4 and the nastiest of the four. Its falsification loop restored the mutated file with `git checkout`, which restores from **HEAD** — silently discarding an uncommitted rewrite of the very function under test. The next mutation then applies to the *old* code, the rebuild happens, the `Compiling` line appears, and the numbers are real for the wrong file. Task 4 caught it only because the following mutation's exact-string match failed; had it matched, it would have measured the old code and reported it as the new. `cp` the file to a snapshot before mutating and restore from that snapshot, so the loop is correct whether or not the deliverable is committed. (A5 rule 1 — commit the deliverable first — also prevents it, and this is why that rule exists.)
   - **Enumerate input shapes, not just mutations.** A mutation can only be caught by an input shape the suite already feeds it, so mutation coverage silently inherits the blind spots of input coverage. The shapes this slice's code distinguishes are listed per task; check you have one of each **before** falsifying.
3. **Tests live in a sibling `*_tests.rs` file**, hooked with
   ```rust
   #[cfg(test)]
   #[cfg_attr(coverage_nightly, coverage(off))]
   #[path = "foo_tests.rs"]
   mod foo_tests;
   ```
   Never an inline `mod tests`. (`gts/permissions.rs` has a pre-existing inline `mod tests`; do not copy it, and do not restructure it either.)
4. **Deletions need a per-test verdict.** This slice deletes more tests than any before it. For every test you delete, state in your report: the test name, what it pinned, and why that question no longer exists. "Deleted the failing test" and "deleted the test whose question no longer exists" look identical in a diff.
5. **Comments must survive a grep.** When you retire a thing, grep for its old name in prose too, and grep the **mechanism-describing verbs** ("hands", "enforces", "needs no", "every read path", "cascade", "latch", "flip"), not just the retired identifiers. Five of slice 3's final findings were a rule stated in a second file at a different strength, all in the published SDK crate — the crate every task touches and no task owns.
   - **Grep on the shortest distinctive fragment, never a phrase.** rustfmt does not reflow doc comments, so a multi-word grep undercounts by however many times an editor happened to wrap. This bit twice in slice 3.
   - **Cite an ADR by its id, never by its number**, in anything that ships: `cpt-cf-usage-collector-adr-append-only-invalidation`, not `ADR-0010`. A number is exactly the reference that silently retargets when a document is renumbered, which already happened in this gear. Numbers are fine in this plan and in commit messages; they are not fine in a doc comment or a test comment.
   - **A hedge about what a later task changes is a claim about the code, and it expires.** Name the state ("the SPI still carries a deactivation method"), never a plan task number no outside reader can resolve. Task 8 removes every hedge; none may survive the slice.
6. **Commits** are Conventional Commits with a `Signed-off-by: capybutler <capybutler@gmail.com>` trailer and a body explaining *why*, not what. A breaking change takes `!` in the subject and a `BREAKING CHANGE:` trailer. Do **not** bump crate versions.
7. **Do not commit to the branch while another implementer is working.** An amend lands on the wrong commit. This happened once in slice 3 and cost a history reconstruction.
8. **Do not ask a task to hold the host-crate error count flat, and do not treat a rise as a defect.** Between task 1 and task 3 the host does not compile, and a task that deletes a vocabulary whose callers a *later* task removes necessarily raises the count — task 2 took it from 72 to 91 and was right to. The check that means something is a **classification**: every error names a retired symbol, and none is in a file the task itself owns. Ask for that, not for a number.
9. **Namespace your scratchpad, and never let a restore script reach the deliverable.** A fifth false-pass shape, hit in task 5. The reviewer wrote its own `restore_probe.sh` over the implementer's at the same top-level path; that version restored `domain/service.rs` from a snapshot taken at the *committed* state, so running it silently reverted two uncommitted doc amendments. The implementer noticed only because the script printed no marker lines and `git diff HEAD` disagreed with a `grep` it had run seconds before. Rule 10 forbids two workers mutating *concurrently*; it says nothing about the artifacts they leave behind, and those persist and collide across turns. So: put your scripts and snapshots under `scratchpad/<task>/`, scope every restore to the files your patch actually touched (plus `Cargo.lock`), and state in the script why it deliberately does not reach the deliverable. A restore script that can revert the thing under test is a mutation you did not intend.
10. **Only one worker mutation-tests the tree at a time.** Rule 7 covers commits and does not cover this. In task 1 two reviewers ran against one working tree concurrently and one of them found the other's live mutation — a `(None, Some(_))` arm replaced with `Ok(None)` — sitting in `models.rs` mid-review. It contaminated two runs and very nearly produced a reported finding that was really the other reviewer's edit. **A foreign mutation produces false passes and false failures in both directions, and neither is distinguishable from a real result.** Before mutating: confirm `git status --porcelain` is clean apart from known untracked files, and confirm your baseline run is green at the expected count. If it is not, stop and say so rather than reasoning about the delta. The coordinator is responsible for not overlapping two mutating workers; a worker that finds an unexplained diff should report it rather than restore it silently.

### Verification bar (every task ends here)

```bash
cargo check --workspace --all-targets
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk \
  -p cf-gears-noop-usage-collector-plugin --no-fail-fast
cargo clippy --workspace --all-targets --all-features
cargo +nightly fmt
cargo doc --no-deps -p cf-gears-usage-collector-sdk -p cf-gears-usage-collector
```

Add `-p cf-gears-toolkit-odata` only if you touch that shared lib (no task here should).

**`cargo doc --no-deps` is on the bar because task 1 found it earning its place.** This slice moves and renames types that doc comments link to with intra-doc `[`…`]` syntax, and a link to a moved item is a warning nothing else in the bar reports. Task 1 ran it unprompted after the `Option<Invalidation>` fold and it caught a stale `UsageRecord::reason_code` link that clippy, the tests and rustfmt all passed over. Every task here retires or moves a linked item, so run it every time.

**Do not run a workspace-wide test build.** `target/` reaches ~110 GB and fills the disk; the user had to stop mid-session to clean it once already. Scope test runs to the three packages above.

Clippy is deny-warnings in CI. **Baseline entering this slice: 606 passed, 6 skipped** across the three usage-collector packages (724 with `cf-gears-toolkit-odata`). The 6 skipped are the `#[ignore]`d OpenAPI drift tests that slice 6 re-enables — leave them skipped.

### API facts verified against this tree at `74e3d4429` — use these, do not re-derive them

**One caveat, added after task 3.** Ground rule 1 tells you to distrust every code sketch and exempts this table. That exemption has already failed once: the `unrestricted_read_filter` row inherited a miscount from the doc comment it described, and task 3's implementer — correctly trusting the table — wrote a wrong call-site count into shipped code. **Any row that states a count, a cardinality or an enumeration is worth one `grep -c` before you rely on it.** Rows stating an API's shape or behaviour are what the exemption is really for.

| Fact | Consequence |
| --- | --- |
| `UsageCollectorError::Conflict` has fields `{ resource_type, name, reason: ConflictReason, detail }` | A new conflict constructor fills all four. `resource_type` is `USAGE_RECORD_RESOURCE.to_owned()`. |
| `UsageCollectorError::NotFound` has fields `{ resource_type, name, detail }` — **no `reason`** | The invalidation-target-missing error carries its discrimination in `name` (a UUID) and `detail` only. |
| `UsageCollectorError::InvalidArgument` has `{ resource_type, resource_name: Option<String>, field, reason: ValidationReason, detail }` | `field` is the attributed request field — this is where the faithful-copy rejection names the field that differs. |
| `classify_record_error` in `service.rs` discriminates `NotFound` by whether `name` parses as a `Uuid` | An unresolved meter's `gts_type_id` never parses; an entry id always does. Keep that discriminator working when you repoint the constructors. |
| `IdempotencyKey::new` caps at 256 **bytes** while the OAS `maxLength: 256` counts code points | A new `ReasonCode` newtype mirrors the byte cap for intra-crate consistency and joins the same pre-existing divergence family. Do not "fix" one side alone. |
| `IdempotencyKey::new` rejects ASCII control characters via `char::is_ascii_control()` (covers DEL) | `ReasonCode` does the same. It is not an identity input, so the reason is wire hygiene rather than pre-image safety — say so, do not copy the `0x1F` justification. |
| `#[derive(ODataFilterable)]` on `UsageRecordQuery` generates `UsageRecordQueryFilterField`, re-exported as `UsageRecordFilterField`, with `from_name(&str) -> Option<Self>` | Adding or removing a field on that struct is a **compile break** for any plugin `FieldToColumn` impl. That is the good kind — loud. |
| `KEYSET_SAFE_RECORD_FIELDS` is `pub`, and `error.rs::inadmissible_order_key` interpolates it verbatim (`{:?}`) into a 400 detail | Removing `status` changes an exported constant's value **with no compile break for a consumer**, and changes a user-visible error message. Two tests pin the seven names in two different shapes. |
| `every_admissible_order_key_resolves_to_a_column` (`models_tests.rs:1104`) asserts every allowlist name resolves through `UsageRecordFilterField::from_name` | It will **not** catch a derived field added to the allowlist once that field is also on the filterable schema. Task 1 must widen it — see the task. |
| `ValidationReason` and `ConflictReason` are `#[non_exhaustive]` | Adding a variant is not a compile break for a consumer, and **removing one is not either**. The removals here are therefore **silent** for a downstream matcher. Say so in the `BREAKING CHANGE:` trailer. |
| `harness_sees_the_whole_rest_surface` asserts `registry_ops(&reg).len() == 5` and `yaml_ops(&doc).len() == 7` | The yaml documents no deactivate operation (its 7 are create, list, get, aggregate, backfill, feed, reconciliation). Removing the route takes the registry count to **4**; the yaml count is unchanged. |
| `registration_tests.rs` pins the exact `(method, path)` set, including `POST /usage-collector/v1/records/{id}/deactivate` | Repoint the list, do not delete the test. |
| `HappyPathPlugin::calls()` / `last_fold()` count **only** `query_aggregated_usage_records` dispatches | For create-path assertions use `last_create_record_input()` / `last_create_records_input()`. Asserting `calls() == 1` in an ingestion test fails permanently. |
| `ServiceFixture` is the only fixture shape | `.with_source(..)`, `.with_cap(..)`, `.with_resolver(..)`, then `.build(..)` / `.build_with_default_resolver_handle(..)` / `.build_with_metrics(..)`. Do not reintroduce `service_with_*_and_*` functions. |
| `resolve_l1_lookups` (`service.rs:475`) already solves dedup-by-distinct-id + bounded fan-out + per-index projection with a preserved error-priority ordering | Task 5 **rewrites** it rather than replacing it wholesale. Read it before touching it. |
| `unrestricted_read_filter()` (`service.rs`) is `ast::Expr::Value(ast::Value::Bool(true))` | The target lookup keeps using it: it is a same-request shape check after the submitting caller's own PDP authorization succeeded, not a caller-scoped read. **It has three physical call sites before task 3 and two after** — `resolve_l1_lookups` (batch) and `create_usage_record_inner` (single-record). Its doc comment says "two", counting *logical* sites because one bullet covers two functions; that imprecision was inherited into an earlier draft of this table as fact. `grep -c` before you write the count. |
| `#[domain_model]` (from `toolkit_macros`) is the attribute every domain enum in `ports/metrics.rs` and `validation.rs` carries | New domain enums carry it too. |
| `#[toolkit_macros::api_dto(request)]` / `(response)` are the REST DTO attributes | `api_dto` applies `rename_all`; do not add a second one. |

### Amendments made after task 1 landed

Task 1's two reviews changed three things this plan asserted. They are folded into the tasks below; recorded here so a reader of a later task knows the tasks were edited rather than written this way.

**A1. The correction fields are one `Option<Invalidation>`, not a flat pair.** `Invalidation { target: Uuid, reason: ReasonCode }` on both `UsageRecord` and `CreateUsageRecord`. The half-shape — a reason with no target, or the reverse — is unrepresentable, so `entry_type()` is total by construction and cannot lie. **The wire encoding is unchanged**: a shadow struct carrying `deny_unknown_fields` keeps `invalidates` and `reason_code` as two flat sibling properties exactly as the OAS declares them, both omitted on an ordinary record. No client can observe the change, so it is not a `DIVERGENCES.md` entry.

**Scope note, added after task 7.** The boundaries listed below are the ones on a **submission** path. There is a fourth enforcement of the same rule this list deliberately does not cover: `invalidation_from_wire` is called from *both* serde shadows, so a `UsageRecord` rehydrated from JSON is refused too, and `models_tests.rs` exercises both call sites independently. Two implementers transcribed this list into shipped comments without the scope and produced a count short by one. **If you restate it, say "on a submission path" or say four.**

**Consequence for the error taxonomy: `INVALIDATION_REFERENCE_INCOMPLETE` moves surface.** An in-process caller cannot construct a half-shape at all, so there is no projection-time check and task 1 removed the variant it had added. **Task 2 adds it back, and task 7 is its only caller.** The reasoning, worked through with task 1's implementer:

- The SDK's own shadow struct refuses a half-shape body, but that rejection happens inside a `Deserialize`, which erases everything but a message string. A typed reason there is information no caller receives, so that path stays untyped — and no REST request takes it. It is the direct-JSON-into-`CreateUsageRecord` path only.
- The REST DTO **keeps the flat pair**, because `api_dto` emits it into the served OpenAPI document and the OAS declares two flat properties; folding the DTO would change the published schema, which is a real contract break rather than a representation choice.
- So the flat DTO deserializes cleanly and `record_request_into_domain` is the fold point — an ordinary fallible conversion in handler code, which *can* raise a typed `InvalidArgument` naming the missing half. That is where the variant lives.

The net effect is better than the flat pair gave us: one typed rejection at the one boundary a caller can reach, and no downstream re-check to mask a handler that drops a field.

**A2. `entry_type` cannot be lowered through `FieldToColumn::map_value`.** This plan's task-1 sketch said it could, and the sketch shipped into two doc comments before the review caught it. Verified against `libs/toolkit-db/src/odata/sea_orm_filter.rs`: `map_field` (L47-52) is total and must return a column; `map_value` (L86-88) rewrites the value alone and can change neither the column (fixed at L233) nor the operator (`*op` passed by value at L235). `build_binary_condition` calls `odata_value_to_sea_value` at **L284**, *before* the `is_null()` arm at L287-293, so a `Null` errors at L343 and the NULL arm is never reached from `filter_node_to_condition` at all — neither `eq 'record'` nor `eq 'invalidation'` is expressible, and `in (...)` fails the same way through L245-247. **The only implementable lowering is a stored generated column** (`CASE WHEN invalidates IS NULL THEN 'record' ELSE 'invalidation' END`) returned from `map_field`. Any task writing about how a plugin filters on `entry_type` says that and nothing else.

**A3. The keyset exclusion is grounded in an SDK-owned fact, not a storage claim.** Because a conforming plugin *does* materialize a column for `entry_type` (A2), "it has no column" is false where it matters. The SDK-owned statement is that `entry_type` is a function of the optional `invalidates` and partitions entries on that field's *absence*, so the SDK carries no such attribute, obliges no plugin to materialize one, and will not promise a keyset key over it. A plugin that materializes it for filtering still gets no order key, because the guarantee a caller's `$orderby` rests on is the SDK's to give.

**A4. The host crate does not compile until tasks 4, 5 AND 7 have all landed — and the task order changes because of it.** This plan asserted twice that task 3 "makes the host crate compile again". That is false, and task 3 proved it. `cf-gears-usage-collector`'s unit-test binary is **one compilation unit**: while any `corrects_id` / `status` reference remains anywhere in the crate, `cargo nextest run -p cf-gears-usage-collector` runs *no* host test at all. Task 3 left 82 such errors, every one in code tasks 4 (`validation.rs`), 5 (`service.rs`) and 7 (`dto.rs`, `handlers/`) own.

Two consequences:

- **The execution order is now 4 → 5 → 7 → 6 → 8**, not 4 → 5 → 6 → 7. Task 6 needs a compiling host to run the fold tests it writes, and task 7 is what finally delivers one. Running 6 before 7 would make it the third task in a row writing tests it cannot execute.
- **Task 7 inherits task 3's verification obligation**, which task 3 could not discharge: task 2's eight never-run tests, and the three mutations against task 2's constructors. That list now lives in task 7.

Tasks 4 and 5 will write host tests they cannot run. That is unavoidable without merging three tasks into one, and merging them would produce a diff no reviewer could hold. **Say so in your report rather than implying a green run.**

**A5. The measurement patch is an approved technique, with rules.** Task 3 needed numbers from a crate that does not build, and got them honestly: it committed the deliverable first, applied a throwaway compile-through patch *on top of* the commit, took every measurement, restored with `git checkout -- gears/`, and disclosed exactly what the patch contained. That is the right shape and you may use it. The rules:

1. **Commit the deliverable first.** The patch never touches the commit.
2. **Never commit the patch**, and verify it is gone with `git status --porcelain` plus `git diff HEAD -- gears/` **and `git diff HEAD -- Cargo.lock`**. A probe that adds a dev-dependency dirties the root `Cargo.lock`, which `git checkout -- gears/` does not reach — task 4's reviewer caught exactly that and had to restore it explicitly.
3. **Disclose exactly what it did** — which files, which call sites, which test modules you disabled at their `#[path]` hooks.
   - **Scope the restore to the patched files, never to `gears/`.** `git checkout -- gears/` also discards uncommitted work on your own deliverable. Task 4 lost two doc corrections that way and had to reapply them. Write a `restore_probe.sh` that names the patched paths plus `Cargo.lock`, and have it print the verification itself.
4. **Mark every number obtained under it.** A measurement taken under a patch is evidence about the code *plus the patch*, and a reader cannot tell which unless you say.
5. **Leave the script where a reviewer can reproduce it**, in the scratchpad.

**Two shapes have been used; the second is better and you should prefer it.** Task 3's was a *compile-through* patch — it rewrote fixtures and disabled four test modules at their `#[path]` hooks to force the host crate to build. It works, but it stubs other tasks' work and two reviewers declined it as too contaminating to trust. Task 4's was a *probe*: it appended `#[cfg(test)] #[path = "../../../usage-collector/src/domain/invalidation.rs"] pub mod invalidation_probe;` plus one dev-dep to the **noop plugin** crate, which does build and carries the same `[lints] workspace = true`. That compiles the real files **unmodified, by path**, stubs nothing and disables nothing. Task 4's reviewer reused it and endorsed it.

Its one honest limit, which you must state: the probe exercises the files under a *different crate's* `#[cfg]` set, feature unification and test doubles. It proves the code compiles and its own tests pass; it does not prove they pass in place. **Task 7 owes the first in-place run of everything measured this way.**

### Two decisions taken before this plan, with their reasons

Both are departures from a one-line summary in `SLICE4.md`. They are deliberate, and the reasoning belongs in your report if a reviewer asks.

**1. At-most-one-invalidation is the plugin's obligation alone. The gateway does not pre-read for it.**

`SLICE4.md` says "the plugin enforces this atomically; the gateway pre-checks." `cpt-cf-usage-collector-adr-append-only-invalidation` — the governing decision for this slice — says the opposite about the gateway half:

> Only the store can make that check atomic with the entry it admits, in a single backend transaction. A gateway-side pre-read cannot exclude a concurrent second submission, so at-most-one is the plugin's one invalidation obligation.

A record carries no reverse link to its invalidation (DESIGN §3.1: "No read path carries a reverse one"), so a gateway pre-check is not a field on the target lookup — it would need a **second** SPI call per distinct target, `list_usage_records(target.gts_type_id, [target.window_end, +1µs), $filter=invalidates eq <target>)`, which is still unsound under concurrency. The user chose the ADR's posture. **Build the plugin-only enforcement.** The target itself answers **two** — is-a-record and faithful-copy; those are the comparator's. Valid-reference is answered by the lookup rather than by the target, and the reference/reason pair is answered by the type. (An earlier draft said "the four the target itself can answer", miscounting the same way task 4's sketch did.)

**2. The new wire reason codes are not in the yaml, and you are not editing the yaml.**

`usage-collector-v1.yaml` names `IDEMPOTENCY_CONFLICT` and `VALIDATION` in prose and enumerates no invalidation-specific reason. DESIGN §3.11.5 names an `invalidation_rule` *metric* category but no *wire* reason. This slice introduces four wire codes (below). Editing the contract is the spec owner's call, so **record the gap as a sixth entry in `DIVERGENCES.md`** (task 8) rather than editing `usage-collector-v1.yaml` or `DESIGN.md`.

### The error taxonomy this slice introduces

Fixed here so eight tasks spell it one way. Slice 6 owns the final reason-vocabulary pass and may rename; nothing below is load-bearing beyond this slice's internal consistency.

| Condition | Variant | `reason` | `field` / `name` |
| --- | --- | --- | --- |
| A **REST body** carrying `invalidates` without `reason_code`, or the reverse | `InvalidArgument` | `ValidationReason::InvalidationReferenceIncomplete` (`INVALIDATION_REFERENCE_INCOMPLETE`) | `field` = the **missing** one |
| `invalidates` resolves to nothing | `NotFound` | — (`NotFound` carries no reason) | `name` = the target uuid |
| Target is itself an invalidation | `InvalidArgument` | `ValidationReason::InvalidationTargetNotRecord` (`INVALIDATION_TARGET_NOT_RECORD`) | `field` = `"invalidates"` |
| A caller-supplied field differs from the target's | `InvalidArgument` | `ValidationReason::InvalidationFieldMismatch` (`INVALIDATION_FIELD_MISMATCH`) | `field` = **the field that differs** |
| The target already carries an invalidation (plugin-detected) | `Conflict` | `ConflictReason::AlreadyInvalidated` (`ALREADY_INVALIDATED`) | `name` = the target uuid |

The first row is raised at the **REST fold point** (`record_request_into_domain`, task 7), not in the domain — see amendment A1. No in-process path can produce a half-shape, so nothing downstream re-checks it, which makes task 7's test the only guard on that rule.

Retired in the same pass: `ALREADY_INACTIVE`, `CORRECTS_ID_TARGETS_COMPENSATION`, `CORRECTS_ID_WRONG_SCOPE`, `CORRECTS_ID_INACTIVE` and their `ConflictReason` variants; the constructors `corrects_id_not_found`, `already_inactive`, `corrects_id_targets_compensation`, `corrects_id_wrong_scope`, `corrects_id_inactive`.

### Deliberately not in this slice

Do not build these, and do not "while I'm here" them:

- **`RecordOrigin`, `backfill_usage_records`, the live path's future/past tolerances, `POST /records/backfill`** — slice 5. This means `uc_ingestion_records_total` gains its `entry_type` label here and its `origin` label there; record that as a residual divergence rather than half-building it.
- **Narrowing `UsageCollectorPluginError` to DESIGN §3.3's six variants, re-enabling the six `#[ignore]`d OpenAPI drift tests, scaffolding the plugin contract suite** — slice 6. **You add `AlreadyInvalidated` to the SPI and remove `UsageRecordAlreadyInactive`; you do not narrow the rest of the taxonomy.**
- **`accepted_at` and `acceptance_sequence`.** They do not exist on `UsageRecord` and no slice claims them, yet DESIGN says the plugin assigns `acceptance_sequence` monotonically and the `LATEST` tie-break resolves on it — and `models.rs:887` documents that tie-break against a field the record does not carry. It does not block this slice. **Flag it in your report**; it blocks a conformant storage plugin.
- **Renaming `value` to `quantity`.** Out of the handoff's slice list, same as in slice 3. The yaml says `quantity`; the code says `value`. Residual divergence, not this slice's.
- The usage feed, reconciliation, the ADR-0015 declaration mirror, ingestion quotas.
- **The TimescaleDB plugin.** Not a workspace member, invisible to `cargo check --workspace`, already on the pre-slice-2 model. Leave every file under `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/` untouched. **`git diff --stat` must show zero files from it.**
- `usage-collector-v1.yaml`, `DESIGN.md`, `DECOMPOSITION.md`, `docs/features/*`.
- Any crate version bump.
- **Fixing the dangling `@cpt-flow:` / `@cpt-algo:` / `@cpt-dod:` / `@cpt-state:` markers.** The DESIGN rework dropped whole id categories and a "does every marker resolve" gate would fail across the crate today. You will be **moving** several of these markers as you delete the deactivation and compensation flows. **Move them, do not try to fix them**, and do not invent new ids in the dropped categories.

### File structure

All paths relative to `gears/system/usage-collector/`.

| File | Responsibility after this slice | Task |
| --- | --- | --- |
| `usage-collector-sdk/src/models.rs` | `EntryType`; `ReasonCode`; `UsageRecord` / `CreateUsageRecord` carry `invalidates` + `reason_code` and no `status` / `corrects_id`; `UsageRecord::entry_type()`; both-or-neither in the projection; the filterable schema and the keyset allowlist. | 1 |
| `usage-collector-sdk/src/reason.rs` | `ConflictReason::AlreadyInvalidated`; the three new `ValidationReason` variants; the four retired conflict codes gone. | 2 |
| `usage-collector-sdk/src/error.rs` | The five invalidation constructors; the five compensation constructors gone; `UsageCollectorPluginError::AlreadyInvalidated` replaces `UsageRecordAlreadyInactive`. | 2 |
| `usage-collector-sdk/src/api.rs` | SDK trait loses `deactivate_usage_record`. | 3 |
| `usage-collector-sdk/src/plugin_api.rs` | SPI loses `deactivate_usage_record`; gains the fold-exclusion and ledger-read obligations. | 3, 6 |
| `usage-collector/src/domain/invalidation.rs` *(new)* | The pure faithful-copy comparator and the target verification it drives. | 4 |
| `usage-collector/src/domain/invalidation_tests.rs` *(new)* | Its tests. | 4 |
| `usage-collector/src/domain/validation.rs` | Keeps `validate_submit_record_metadata`; loses `SemanticsOutcome`, `validate_record_semantics`, `verify_l1_corrects_id`. | 4 |
| `usage-collector/src/domain/service.rs` | Loses `deactivate_usage_record`; the L1 fan-out becomes the invalidation-target fan-out; `record_kind_of` becomes the entry-type label. | 3, 5 |
| `usage-collector/src/domain/error.rs` | `AlreadyInvalidated` replaces `UsageRecordAlreadyInactive`; the deactivation lift goes. | 2, 3 |
| `usage-collector/src/domain/ports/metrics.rs` | `RecordKind` → `EntryType` labels; `RecordErrorCategory::InvalidationRule`; `DeactivationErrorCategory` and `record_deactivation_request` gone. | 3, 5 |
| `usage-collector/src/infra/metrics.rs` | The `uc_deactivation_*` instruments gone. | 3 |
| `usage-collector/src/infra/sdk_error_mapping.rs` | The retired reasons out, the new ones in. | 2 |
| `usage-collector/src/domain/authz.rs`, `src/gts/permissions.rs` | The `deactivate` action and its permission instance gone. | 3 |
| `usage-collector/src/domain/local_client.rs` | Loses the deactivate forward. | 3 |
| `usage-collector/src/domain/test_support.rs` | Doubles drop deactivation; gains the folding in-memory plugin. | 3, 6 |
| `usage-collector/src/api/rest/dto.rs` | Create DTO carries `invalidates` / `reason_code`; response DTO carries derived `entry_type` and no `status` / `corrects_id`. | 7 |
| `usage-collector/src/api/rest/handlers/usage_records.rs` | Loses the deactivate handler; the create projection carries the two new fields. | 3, 7 |
| `usage-collector/src/api/rest/routes/usage_records.rs` | Loses the deactivate route. | 3 |
| `plugins/noop-usage-collector-plugin/src/plugin.rs` | Implements the SPI without deactivation. | 3 |
| `DIVERGENCES.md` | Gains the sixth entry. | 8 |

---

## Task 1: The SDK entry shape

> **Landed, with amendments.** This task is complete. Read amendments A1-A3 above before reading the steps below: the shape shipped is one `Option<Invalidation>` rather than the flat pair these steps describe, the `map_value` claim in step 3's `UsageRecordQuery` sketch is **wrong** and was corrected in the code, and the keyset doc's grounding was rewritten. The steps are left as written so the review trail is legible; the amendments are what is true.

**Files:**
- Modify: `gears/system/usage-collector/usage-collector-sdk/src/models.rs`
- Modify: `gears/system/usage-collector/usage-collector-sdk/src/models_tests.rs`
- Modify: `gears/system/usage-collector/usage-collector-sdk/src/lib.rs` (re-exports)

This task changes the shape only. The rules that need a target lookup are tasks 4 and 5; the rule that needs no lookup — both-or-neither — lands here, in the same fallible projection that already rejects a sub-microsecond bound, because it is a property of the submission alone.

**Three design points, all load-bearing:**

- **`entry_type` is derived and never stored.** It is a method on `UsageRecord`, not a field. `#[serde(deny_unknown_fields)]` on `UsageRecord` means a serialized field would have to round-trip through every plugin, and a stored discriminator is exactly the thing that can disagree with the `invalidates` it claims to summarize. The wire projection in `api/rest/dto.rs` computes it (task 7).
- **`ReasonCode` is a validating newtype**, not a `String`, for the same reason `IdempotencyKey` is: the OAS constrains it (`minLength: 1`, `maxLength: 128`) and an unvalidated value would reach a plugin. It caps at 128 **bytes**, mirroring `IdempotencyKey`'s byte cap rather than the OAS's code-point cap — see the API-facts table; this is a known, fail-closed, pre-existing divergence family and fixing one side alone would make it worse.
- **`entry_type` goes on the filterable schema but NOT on the keyset allowlist.** DESIGN §3.1 and the yaml's `Filter` parameter both name it as a filterable field, so it must be nameable in `$filter`. It is not keyset-safe: it has no stored column at all, only an expression over `invalidates`, so a row-value keyset cannot key on it. The allowlist's current doc justifies exclusion by *domain optionality*, and a derived field satisfies that criterion vacuously — which is the exact hole `every_admissible_order_key_resolves_to_a_column` was written to watch for and **cannot** catch here, because `entry_type` *does* resolve through `from_name` once it is on the filterable schema. Step 1 widens that guard.

- [ ] **Step 1: Write the failing tests**

In `usage-collector-sdk/src/models_tests.rs`. Repoint the two existing guards first (do not add duplicates of them):

```rust
// REPOINT the existing allowlist anchor: `status` is gone, six names remain.
// The literal list is the point — the constant is interpolated verbatim into
// the `$orderby` 400 detail, so it IS the wire contract.
assert_eq!(
    crate::models::KEYSET_SAFE_RECORD_FIELDS,
    [
        "id",
        "window_start",
        "window_end",
        "tenant_id",
        "resource_id",
        "resource_type",
    ],
);
```

```rust
// REPOINT `keyset_unsafe_record_fields_are_the_domain_optional_ones`:
// `corrects_id` is gone and `invalidates` takes its place as the
// domain-optional reference.
for field in ["subject_id", "subject_type", "invalidates"] {
    assert!(
        !is_keyset_safe_record_field(field),
        "`{field}` is a domain-optional attribute and must NOT be keyset-safe",
    );
}
```

```rust
// WIDEN `every_admissible_order_key_resolves_to_a_column` with its
// converse. Resolving to a column is necessary but not sufficient:
// `entry_type` is on the filterable schema (DESIGN §3.1 and the yaml's
// `$filter` parameter both name it) and therefore resolves through
// `from_name`, while having no stored column to key on — it is an
// expression over `invalidates`. The existing guard passes on it. This
// one does not.
#[test]
fn a_derived_field_is_filterable_but_never_an_order_key() {
    assert!(
        crate::models::UsageRecordFilterField::from_name("entry_type").is_some(),
        "`entry_type` is a filterable field per the wire contract",
    );
    assert!(
        !is_keyset_safe_record_field("entry_type"),
        "`entry_type` is derived from `invalidates` and has no stored column, \
         so a row-value keyset cannot key on it however mandatory it is",
    );
}
```

Then the new shape tests:

```rust
#[test]
fn entry_type_is_derived_from_the_reference_it_summarizes() {
    let record = a_record();                       // no `invalidates`
    assert_eq!(record.entry_type(), EntryType::Record);

    let invalidation = an_invalidation_of(&record); // carries `invalidates`
    assert_eq!(invalidation.entry_type(), EntryType::Invalidation);
}

#[test]
fn an_entry_carries_no_serialized_entry_type() {
    // Derived means derived: a stored discriminator is a second place the
    // kind can be read, and the two can disagree. `deny_unknown_fields`
    // makes the negative assertion sharp — a submitted one is refused.
    let json = serde_json::to_value(a_record()).expect("serializes");
    assert!(json.get("entry_type").is_none());

    let mut with_marker = json.as_object().expect("object").clone();
    with_marker.insert("entry_type".to_owned(), serde_json::json!("invalidation"));
    serde_json::from_value::<UsageRecord>(serde_json::Value::Object(with_marker))
        .expect_err("a submitted entry_type must be refused as an unknown field");
}

#[test]
fn invalidates_and_reason_code_are_both_or_neither() {
    // Shape rule, so it belongs in the projection beside the period
    // preconditions — it needs no target lookup.
    let err = create_with(Some(TARGET_ID), None)
        .try_into_usage_record()
        .expect_err("a reference without a reason must be refused");
    assert!(matches!(
        err,
        UsageCollectorError::InvalidArgument { ref field, reason: ValidationReason::InvalidationReferenceIncomplete, .. }
            if field == "reason_code"
    ));

    let err = create_with(None, Some(reason_code("emitter_duplicate")))
        .try_into_usage_record()
        .expect_err("a reason without a reference must be refused");
    assert!(matches!(
        err,
        UsageCollectorError::InvalidArgument { ref field, reason: ValidationReason::InvalidationReferenceIncomplete, .. }
            if field == "invalidates"
    ));

    create_with(None, None).try_into_usage_record().expect("an ordinary record");
    create_with(Some(TARGET_ID), Some(reason_code("emitter_duplicate")))
        .try_into_usage_record()
        .expect("an invalidation");
}

#[test]
fn the_reference_does_not_reach_the_derived_identity() {
    // `cpt-cf-usage-collector-adr-record-identity-derivation` excludes the
    // entry type, so an invalidation derives its id from the same five
    // inputs as its target and departs only through its own idempotency
    // key. That is what makes a key reused across the pair collide loudly
    // instead of silently producing two entries.
    let target = create_with(None, None);
    let mut withdrawal = target.clone();
    withdrawal.invalidates = Some(TARGET_ID);
    withdrawal.reason_code = Some(reason_code("emitter_duplicate"));

    assert_eq!(
        target.clone().try_into_usage_record().expect("record").id,
        withdrawal.try_into_usage_record().expect("invalidation").id,
        "adding a reference must not move the derived identity",
    );

    let mut rekeyed = target.clone();
    rekeyed.idempotency_key = IdempotencyKey::new("a-different-key").expect("valid");
    assert_ne!(
        target.try_into_usage_record().expect("record").id,
        rekeyed.try_into_usage_record().expect("re-keyed").id,
        "the idempotency key is the one departure that does move it",
    );
}

#[test]
fn reason_code_rejects_what_the_wire_contract_rejects() {
    ReasonCode::new("").expect_err("empty");
    ReasonCode::new("x".repeat(129)).expect_err("over 128 bytes");
    ReasonCode::new("emitter\u{7f}duplicate").expect_err("DEL is a control character");
    ReasonCode::new("emitter\u{1f}duplicate").expect_err("0x1F is a control character");
    ReasonCode::new("x".repeat(128)).expect("128 bytes is the boundary, inclusive");
    ReasonCode::new("emitter_duplicate").expect("an ordinary code");
}
```

Verify the helper names against the file before writing: `models_tests.rs` already has fixtures for a `CreateUsageRecord`. **Reuse them, extend them with the two new fields, and do not add a parallel family.**

- [ ] **Step 2: Run to verify they fail**

```bash
cargo nextest run -p cf-gears-usage-collector-sdk --no-fail-fast
```

Expected: compile failure — `EntryType`, `ReasonCode`, `UsageRecord::entry_type`, `CreateUsageRecord::invalidates` and `ValidationReason::InvalidationReferenceIncomplete` do not exist. That reason variant is task 2's; add it here as the minimum needed to compile and let task 2 own the rest of the vocabulary. Say so in your report if you do.

- [ ] **Step 3: Implement**

In `models.rs`:

```rust
/// The closed discriminator between a measurement and a withdrawal.
///
/// **Derived, never stored and never submitted.** It is a projection of
/// [`UsageRecord::invalidates`], so there is no second place the kind can
/// be read and no way for a marker to disagree with the payload it marks
/// (`cpt-cf-usage-collector-adr-append-only-invalidation`). An entry is
/// never identified as a correction by the value or the sign of its
/// quantity: a zero or negative quantity is an ordinary measurement, and
/// an invalidation echoes the quantity it withdraws rather than negating
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryType {
    /// An ordinary measurement: the entry carries no `invalidates`.
    Record,
    /// A withdrawal: the entry names the record it invalidates.
    Invalidation,
}

impl EntryType {
    /// The wire spelling, shared by the REST projection and the
    /// `$filter` surface.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Record => "record",
            Self::Invalidation => "invalidation",
        }
    }
}
```

`ReasonCode` mirrors `IdempotencyKey` structurally — `Serialize`/`Deserialize` via the same `serde_helpers` pattern the file already uses for validating newtypes (check which: `IdempotencyKey` and `MetadataKey` do not use the identical mechanism, so copy the one that fits a `String` newtype needing validation on deserialize).

```rust
/// Maximum `reason_code` length, in bytes.
///
/// The wire contract's `ReasonCode` schema says `maxLength: 128`, which
/// counts code points; this counts bytes, exactly as
/// [`MAX_IDEMPOTENCY_KEY_LEN`] does against its own `maxLength: 256`. The
/// two newtypes carry one divergence rather than two spellings of the same
/// question, and both fail closed.
pub const MAX_REASON_CODE_LEN: usize = 128;
```

On `UsageRecord`, replace `corrects_id` and `status` with:

```rust
    /// The entry this one withdraws.
    ///
    /// Present exactly when [`Self::reason_code`] is, and its presence is
    /// what makes this entry an invalidation — hence
    /// [`Self::entry_type`], which reads it. Deliberately excluded from
    /// the derived identity
    /// (`cpt-cf-usage-collector-adr-record-identity-derivation`), so one
    /// idempotency key cannot stand for both a measurement and its
    /// withdrawal: reusing the target's key across the pair collides on
    /// all five dedup attributes instead of silently producing two
    /// entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalidates: Option<Uuid>,
    /// Why the withdrawal was issued. Present exactly when
    /// [`Self::invalidates`] is, and forbidden on an ordinary record —
    /// including one whose quantity is negative, which records real
    /// consumption rather than a correction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<ReasonCode>,
```

and add the derivation:

```rust
impl UsageRecord {
    /// This entry's kind, derived from the reference it carries.
    ///
    /// Not a field: a stored discriminator is a second place the kind can
    /// be read and the two can disagree
    /// (`cpt-cf-usage-collector-adr-append-only-invalidation`). Every
    /// surface that needs the value computes it here.
    #[must_use]
    pub const fn entry_type(&self) -> EntryType {
        if self.invalidates.is_some() {
            EntryType::Invalidation
        } else {
            EntryType::Record
        }
    }
}
```

`CreateUsageRecord` gains the identical pair, minus the identity paragraph. In `try_into_usage_record`, **before** the period preconditions (it is the cheaper check and needs no normalization):

```rust
        // Both-or-neither. A shape rule, so it lives beside the period
        // preconditions rather than in the gateway's target pre-check: it
        // is a property of the submission alone and needs no lookup. The
        // wire contract states it as `dependentRequired` on
        // `CreateUsageRecordRequest`; this is its in-process half, and it
        // covers the in-process caller the schema never sees.
        match (self.invalidates, self.reason_code.as_ref()) {
            (Some(_), None) => {
                return Err(UsageCollectorError::invalidation_reference_incomplete(
                    "reason_code",
                ));
            }
            (None, Some(_)) => {
                return Err(UsageCollectorError::invalidation_reference_incomplete(
                    "invalidates",
                ));
            }
            _ => {}
        }
```

Update the `UsageRecord` / `CreateUsageRecord` doc comments that name `status` and the "four-cell value matrix": the matrix is gone, and the sign of a quantity carries no structural meaning at all now.

Filterable schema (`UsageRecordQuery`): delete the `corrects_id` and `status` fields, add

```rust
    /// The entry an invalidation withdraws, or absent on an ordinary
    /// record. Filterable (`eq` / `in`) so a consumer folding entries
    /// itself can find a withdrawn pair; **not** an order key, because it
    /// is domain-optional — see [`is_keyset_safe_record_field`].
    #[odata(filter(kind = "Uuid"))]
    pub invalidates: Uuid,
    /// The derived `record` / `invalidation` discriminator. On the filter
    /// surface because the wire contract lists it there, and a plugin
    /// lowers it to a predicate over its `invalidates` column rather than
    /// to a stored column of its own — there is no such column. **Not** an
    /// order key for exactly that reason, mandatory though the value is.
    #[odata(filter(kind = "String"))]
    pub entry_type: String,
```

`KEYSET_SAFE_RECORD_FIELDS` drops `"status"` and nothing replaces it — six names. Rewrite the doc on `is_keyset_safe_record_field`: today it justifies exclusion by domain optionality alone, and that criterion is now insufficient. It must state **both** grounds — an attribute that can be absent, and an attribute with no stored column at all — and name `entry_type` as the second kind. Also update the file-level comment block above `UsageRecordQuery`, which currently explains `status`'s `String` filter encoding.

- [ ] **Step 4: Run to verify they pass**

```bash
cargo nextest run -p cf-gears-usage-collector-sdk --no-fail-fast
```

The host crate will not compile yet — tasks 3-7 repoint it. That is expected between tasks; **do not** stub host-side call sites to make `cargo check --workspace` green here. Report it as the known intermediate state and run the SDK package alone.

- [ ] **Step 5: Falsify**

Input shapes this task's code distinguishes — check you feed one of each before mutating: an entry with neither field; one with both; one with `invalidates` alone; one with `reason_code` alone; a name on the allowlist; a name on the filterable schema but not the allowlist (`entry_type`, `invalidates`); a name on neither.

Mutations to run (absolute paths; `grep` the mutated line before believing a pass; `--no-fail-fast`, unfiltered):

1. `entry_type()` returns `EntryType::Record` unconditionally → `entry_type_is_derived_from_the_reference_it_summarizes` must fail.
2. Both-or-neither: delete the `(None, Some(_))` arm → the second half of `invalidates_and_reason_code_are_both_or_neither` must fail. **Delete each arm separately** — one arm covers the other's shape in neither direction.
3. Add `"entry_type"` to `KEYSET_SAFE_RECORD_FIELDS` → both the literal anchor and `a_derived_field_is_filterable_but_never_an_order_key` must fail. **If only the anchor fails, the new guard is not earning its place — report that.**
4. Pass `invalidates` into `derive_usage_record_id`'s pre-image → `the_reference_does_not_reach_the_derived_identity` must fail. (This one needs a real edit to `id.rs`'s signature; if it is too invasive to apply cleanly, say so rather than skipping silently.)
5. `MAX_REASON_CODE_LEN` 128 → 129 → the boundary assertion must fail.

- [ ] **Step 6: Commit**

```bash
git add gears/system/usage-collector/usage-collector-sdk/src/models.rs \
        gears/system/usage-collector/usage-collector-sdk/src/models_tests.rs \
        gears/system/usage-collector/usage-collector-sdk/src/lib.rs
git commit -s
```

Subject: `feat(usage-collector)!: carry a correction as an appended reference, not a status`

Body: why an entry's kind is derived rather than stored; why the reference is excluded from the identity; why `entry_type` is filterable but not orderable. `BREAKING CHANGE:` trailer naming `UsageRecord.status`, `UsageRecordStatus`, `UsageRecord.corrects_id`, and the `KEYSET_SAFE_RECORD_FIELDS` value change — **and stating that the constant's change is silent for a consumer, because it is a value change on an exported slice with no signature break.**

---

## Task 2: The reason and error vocabularies

**Files:**
- Modify: `gears/system/usage-collector/usage-collector-sdk/src/reason.rs`, `reason_tests.rs`
- Modify: `gears/system/usage-collector/usage-collector-sdk/src/error.rs`
- Modify: `gears/system/usage-collector/src/domain/error.rs`, `error_tests.rs`
- Modify: `gears/system/usage-collector/src/infra/sdk_error_mapping.rs`, `sdk_error_mapping_tests.rs`

The table in "The error taxonomy this slice introduces" above is the specification. Implement exactly it.

- [ ] **Step 1: Write the failing tests**

In `reason_tests.rs`, the round-trip guard the file already runs per vocabulary — **extend the existing tables, do not add standalone duplicates of them**. The two sketches below (`already_invalidated_round_trips` and `the_three_invalidation_validation_reasons_round_trip`) contradict that instruction: they *are* standalone duplicates of the guard the prose tells you to extend. Follow the prose, fold their assertions into the existing per-vocabulary tables, and add only `the_retired_compensation_reasons_no_longer_model_themselves`, which asks a question no existing test asks. The sketches are left below as a statement of what must end up asserted, not of how many tests should assert it.

```rust
#[test]
fn the_retired_compensation_reasons_no_longer_model_themselves() {
    // The four compensation codes are gone from the vocabulary. They must
    // fall through to `Unknown` rather than silently keep a variant, and
    // `Unknown` must preserve the raw string so a consumer reading an old
    // stored envelope still sees what it said.
    for wire in [
        "ALREADY_INACTIVE",
        "CORRECTS_ID_TARGETS_COMPENSATION",
        "CORRECTS_ID_WRONG_SCOPE",
        "CORRECTS_ID_INACTIVE",
    ] {
        assert_eq!(
            ConflictReason::from_wire(wire),
            ConflictReason::Unknown(wire.to_owned()),
        );
        assert_eq!(ConflictReason::from_wire(wire).as_wire(), wire);
    }
}

#[test]
fn already_invalidated_round_trips() {
    assert_eq!(
        ConflictReason::from_wire("ALREADY_INVALIDATED"),
        ConflictReason::AlreadyInvalidated,
    );
    assert_eq!(ConflictReason::AlreadyInvalidated.as_wire(), "ALREADY_INVALIDATED");
}

#[test]
fn the_three_invalidation_validation_reasons_round_trip() {
    for (reason, wire) in [
        (ValidationReason::InvalidationReferenceIncomplete, "INVALIDATION_REFERENCE_INCOMPLETE"),
        (ValidationReason::InvalidationTargetNotRecord, "INVALIDATION_TARGET_NOT_RECORD"),
        (ValidationReason::InvalidationFieldMismatch, "INVALIDATION_FIELD_MISMATCH"),
    ] {
        assert_eq!(ValidationReason::from_wire(wire), reason);
        assert_eq!(reason.as_wire(), wire);
    }
}
```

In `sdk_error_mapping_tests.rs`, repoint whatever currently asserts the compensation conflicts map onto a `409` envelope, to the new codes. **Read each such test in the parent commit and rule on it individually** — some pin the mapping mechanism (keep, repointed) and some pin a specific retired condition (delete, with a verdict).

In `error_tests.rs` (host `domain/error.rs`):

```rust
#[test]
fn the_plugins_at_most_one_check_lifts_to_a_conflict() {
    // At-most-one-invalidation is the store's obligation, not the
    // gateway's: only the store can make the check atomic with the entry
    // it admits (`cpt-cf-usage-collector-adr-append-only-invalidation`).
    // So this arrives as a plugin error and must survive the two lifts
    // with its discrimination intact.
    let plugin_err = UsageCollectorPluginError::AlreadyInvalidated {
        id: TARGET_ID,
        invalidated_by: EXISTING_ID,
    };
    let domain: DomainError = plugin_err.into();
    let public = UsageCollectorError::from(domain);
    assert!(matches!(
        public,
        UsageCollectorError::Conflict { reason: ConflictReason::AlreadyInvalidated, ref name, .. }
            if name == &TARGET_ID.to_string()
    ));
}
```

- [ ] **Step 2: Run to verify they fail**

```bash
cargo nextest run -p cf-gears-usage-collector-sdk -p cf-gears-usage-collector --no-fail-fast
```

- [ ] **Step 3: Implement**

`reason.rs` — delete `ALREADY_INACTIVE`, `CORRECTS_ID_TARGETS_COMPENSATION`, `CORRECTS_ID_WRONG_SCOPE`, `CORRECTS_ID_INACTIVE` and their variants and both match arms each. Add:

```rust
/// A second withdrawal of an already-invalidated record. The store detects
/// it, atomically against the entry it admits.
pub const ALREADY_INVALIDATED: &str = "ALREADY_INVALIDATED";
```

and `ConflictReason::AlreadyInvalidated`. Add three `ValidationReason` consts and variants:

```rust
/// A REST submission carried a target reference without a reason code, or
/// a reason code without a target reference. Raised at the fold point
/// where the flat wire pair becomes one `Option<Invalidation>`; no
/// in-process caller can reach it, because the domain type makes the
/// half-shape unrepresentable.
pub const INVALIDATION_REFERENCE_INCOMPLETE: &str = "INVALIDATION_REFERENCE_INCOMPLETE";
/// An invalidation's target was itself an invalidation.
pub const INVALIDATION_TARGET_NOT_RECORD: &str = "INVALIDATION_TARGET_NOT_RECORD";
/// An invalidation departed from its target in a field it must copy.
pub const INVALIDATION_FIELD_MISMATCH: &str = "INVALIDATION_FIELD_MISMATCH";
```

`error.rs` — delete the five compensation constructors, **and `non_negative_counter_compensation` with them**. Task 1 made `models.rs` say the sign of a quantity carries no structural meaning, which leaves that constructor's own doc ("counter compensation requires value < 0") a false claim in shipped code.

**Correction, verified at `e6ede155a`:** an earlier draft of this plan said the constructor's production call site was inside `validate_record_semantics` and named `service_metrics_tests.rs` among its callers. Both are false. A repo-wide grep finds exactly two references, **both in `sdk_error_mapping_tests.rs` (lines 181 and 343)**, and no production caller at all — it was already dead code before this slice began. The deletion is still right; its urgency was overstated. Grep before you delete rather than trusting either account. `reason.rs:19`'s name for it goes too. Then add:

```rust
    /// A REST submission carried a reference without a reason code, or the
    /// reverse. The two are both-or-neither
    /// (`cpt-cf-usage-collector-adr-append-only-invalidation`): the
    /// reference is what makes the entry an invalidation, and the reason
    /// carries the intent, so half the pair describes nothing. `field`
    /// names the **missing** half, which is the one the caller has to add.
    ///
    /// Reachable from the REST fold point alone. The domain carries the
    /// pair as one `Option<Invalidation>`, so an in-process caller cannot
    /// construct the shape this rejects.
    #[must_use]
    pub fn invalidation_reference_incomplete(missing_field: &str) -> Self { … }

**Between task 1's commit and yours, the SDK can raise no modeled error for a half-shape at all.** Task 1's spec review flagged this explicitly: `invalidation_from_wire` returns a plain `String`, because the rejection happens inside a `Deserialize` and serde erases everything but the message. So a REST caller sending `invalidates` without `reason_code` currently gets whatever the DTO layer maps a serde failure onto — not an `InvalidArgument` carrying a reason. **The SDK can no longer originate that error; the REST layer must.** That is why the constructor comes back here and task 7 wires it.

**Task 1 added this variant and then removed it again when the fold made it unreachable in-process. You are re-adding it for the REST surface, and task 7 is its only caller — so it will be dead code between your commit and task 7's.** That is deliberate and it is the smallest of the available orderings: the alternative is task 7 editing the vocabulary file this task exists to own. Say in your commit body that the constructor lands ahead of its caller.

    /// An invalidation's `invalidates` resolved to nothing.
    ///
    /// `NotFound` rather than a conflict: the reference names an entry the
    /// ledger does not hold. Carries the target uuid in `name`, which is
    /// also what `classify_record_error` reads to tell this apart from an
    /// unresolvable meter — a `gts_type_id` never parses as a `Uuid` and an
    /// entry id always does.
    #[must_use]
    pub fn invalidation_target_not_found(target: Uuid) -> Self { … }

    /// An invalidation's target was itself an invalidation. A correction
    /// cannot be reversed: the model forbids invalidating an invalidation
    /// and caps withdrawal at one per entry.
    #[must_use]
    pub fn invalidation_target_not_record(target: Uuid) -> Self { … }

    /// An invalidation departed from its target in a field it must copy.
    ///
    /// `field` names **the field that differs**, which is the whole point
    /// of the diagnostic: the entry is a faithful copy in every
    /// caller-supplied field, departing only in its own idempotency key,
    /// `invalidates` and `reason_code`, so a rejection that only said
    /// "mismatch" would leave the emitter diffing two payloads by hand.
    #[must_use]
    pub fn invalidation_field_mismatch(field: &str, target: Uuid) -> Self { … }

    /// The target already carries an accepted invalidation. Raised from
    /// the store's atomic check, never from a gateway pre-read — a
    /// gateway-side pre-read cannot exclude a concurrent second
    /// submission, so it would be a check that fails exactly when it
    /// matters.
    #[must_use]
    pub fn already_invalidated(target: Uuid, invalidated_by: Uuid) -> Self { … }
```

Fill each body against the real variant fields (see the API-facts table). In `UsageCollectorPluginError`, replace `UsageRecordAlreadyInactive { id }` with

```rust
    /// A second withdrawal of a record that already carries one. This is
    /// the plugin's **one** invalidation obligation: only the store can
    /// make the check atomic with the entry it admits. Carries the
    /// existing invalidation's id, so the gateway's rejection can name the
    /// entry that already withdrew the target.
    #[error("usage record {id} is already invalidated by {invalidated_by}")]
    AlreadyInvalidated {
        /// The target the submission tried to withdraw.
        id: Uuid,
        /// The invalidation entry that already withdrew it.
        invalidated_by: Uuid,
    },
```

and fix `UsageRecordNotFound`'s doc, which currently names `deactivate_usage_record`.

`domain/error.rs` — rename the `UsageRecordAlreadyInactive` variant to `AlreadyInvalidated { id, invalidated_by }` and repoint its three arms: `From<UsageCollectorPluginError>`, `From<DomainError> for UsageCollectorError`, and the `#[allow(dead_code)]` exhaustiveness fence (`is_plugin_error_exhaustive_today`). **There is no "retryability arm" in this file** — an earlier draft said there was. `is_retryable` lives on `UsageCollectorError` in the SDK and dispatches on category, so it needs no change here.

- [ ] **Step 4: Run to verify they pass**

Same command as step 2. The host crate still will not fully compile until task 3; run what compiles and say so.

- [ ] **Step 5: Falsify**

1. `ConflictReason::AlreadyInvalidated.as_wire()` returns `"ALREADY_INACTIVE"` → the round-trip test must fail.
2. Drop the `AlreadyInvalidated` arm from the plugin→domain lift so it falls to `Internal` → `the_plugins_at_most_one_check_lifts_to_a_conflict` must fail.
3. `invalidation_field_mismatch` puts a literal `"invalidates"` in `field` instead of the argument. **An earlier draft of this note said no test in this task could catch it. That is false** — `invalidation_field_mismatch_attributes_the_field_that_differs` in `sdk_error_mapping_tests.rs` passes `"value"` and asserts `field == "value"`, which kills it directly. The note was written assuming the repointed tests would be thinner than they turned out to be. It survives only *accidentally*, because the host crate does not compile until task 3 — which is exactly why task 3 must re-run it for real. Do not inherit a gap that is already closed.

- [ ] **Step 6: Commit**

Subject: `feat(usage-collector)!: replace the compensation reasons with the invalidation ones`

`BREAKING CHANGE:` trailer naming the four removed `ConflictReason` variants, the five removed constructors and the renamed SPI variant — **and stating that because `ConflictReason` and `ValidationReason` are `#[non_exhaustive]`, the removals are silent for a downstream matcher rather than a compile break.**

---

## Task 3: Remove the deactivation surface

**Files:**
- Modify: `usage-collector-sdk/src/api.rs`, `plugin_api.rs`
- Modify: `usage-collector/src/domain/service.rs`, `service_tests.rs`, `service_metrics_tests.rs`
- Modify: `usage-collector/src/domain/local_client.rs`, `local_client_tests.rs`
- Modify: `usage-collector/src/domain/authz.rs`, `authz_tests.rs`
- Modify: `usage-collector/src/domain/ports/metrics.rs`, `ports/mod.rs`
- Modify: `usage-collector/src/infra/metrics.rs`, `metrics_tests.rs`
- Modify: `usage-collector/src/gts/permissions.rs`
- Modify: `usage-collector/src/api/rest/handlers/usage_records.rs`, `handlers/mod.rs`, `handlers/usage_records_tests.rs`
- Modify: `usage-collector/src/api/rest/routes/usage_records.rs`, `routes/usage_records_tests.rs`, `routes/registration_tests.rs`, `routes/openapi_contract_tests.rs`
- Modify: `usage-collector/src/domain/test_support.rs`
- Modify: `plugins/noop-usage-collector-plugin/src/plugin.rs`, `plugin_tests.rs`

The largest deletion in the slice. `cpt-cf-usage-collector-adr-append-only-invalidation` puts it plainly: "The model carries no status field, no lifecycle flag, and no active or inactive state anywhere," and "no dedicated correction endpoint, SDK method, or storage-plugin call exists." A withdrawal travels the ordinary ingestion path, so there is nothing left for this surface to do.

**You cannot run task 2's host tests — an earlier draft of this plan said you could, and it was wrong (amendment A4).** The host's test binary is one compilation unit and needs tasks 4, 5 and 7. **Task 7 now owns that obligation.** What follows is left here only so you know why the crate still fails after your work, and what is waiting on it.

**The obligation, now task 7's:** Task 2 wrote eight tests that have never executed, because the host crate has not compiled since task 1. A first green run of them is the actual verification of task 2's work, and three mutations against task 2's constructors currently survive with no diagnostic at all — one of them silently, guarding the `classify_record_error` discriminator. The eight:

| File | Test |
| --- | --- |
| `domain/error_tests.rs` | `the_plugins_at_most_one_check_lifts_to_a_conflict` |
| `domain/error_tests.rs` | `sdk_already_invalidated_is_not_retryable` |
| `infra/sdk_error_mapping_tests.rs` | `already_invalidated_maps_to_409_aborted_with_already_invalidated_reason` |
| `infra/sdk_error_mapping_tests.rs` | `invalidation_target_not_found_maps_to_404_naming_the_target_uuid` |
| `infra/sdk_error_mapping_tests.rs` | `invalidation_target_not_record_maps_to_400_attributing_the_reference_field` |
| `infra/sdk_error_mapping_tests.rs` | `invalidation_field_mismatch_attributes_the_field_that_differs` |
| `infra/sdk_error_mapping_tests.rs` | `invalidation_reference_incomplete_attributes_the_missing_half` |
| `infra/sdk_error_mapping_tests.rs` | `lift_record_covers_every_usage_record_surface_variant` |

Task 7 reports each one's first result individually, and runs the three mutations task 2 could not: `invalidation_field_mismatch` writing a literal `"invalidates"` into `field`; `already_invalidated` dropping `{invalidated_by}` from its `detail`; and `invalidation_target_not_found` writing `name: format!("invalidates:{target}")` instead of a bare uuid. The third matters most — it silently breaks `classify_record_error`'s uuid-parse discriminator and produces no compiler warning.

**Task 3 measured all eleven under a disclosed measurement patch (A5) and they passed: all eight green, all three mutations killed.** Treat that as encouraging, not as verification — the numbers describe the code plus a patch. One of the three had two killers under the patch and has only one in the shipped tree; the second killer is `service_metrics_tests::classify_record_error_maps_each_arm`, which becomes genuine once **task 5** repoints that table's `corrects_id_not_found` row onto `invalidation_target_not_found`. Task 5: make that repoint, and know that it is what closes the discriminator's second guard.

**This is the task where a per-test verdict matters most.** You will delete roughly **twenty** tests — an earlier draft said "on the order of a hundred", which counted the `corrects_id` / `status` fixture churn that tasks 4, 5 and 7 own. Task 3 deleted 18. For each: name it, say what it pinned, and say why that question no longer exists. Three shapes appear here and they are not the same:
- Tests of the deactivation flow itself — the question is gone with the flow.
- Tests of a *general* mechanism that merely used deactivation as their vehicle (the by-id `PermissionDenied`→`NotFound` collapse, the plugin-unready 503 lift, the metrics-emitted-exactly-once discipline). **These must be repointed onto a surviving by-id or ingestion surface, not deleted** — the mechanism outlives its vehicle.
- Tests asserting the route set or the operation count — repointed, not deleted.

- [ ] **Step 1: Write the failing tests**

Repoint the two surface guards first:

```rust
// `harness_sees_the_whole_rest_surface` in openapi_contract_tests.rs:
assert_eq!(
    registry_ops(&reg).len(),
    4,
    "expected 4 registered operations (create, list, get, aggregate); a \
     correction travels the ordinary ingestion path, so there is no \
     dedicated correction endpoint",
);
// The yaml count is UNCHANGED at 7 — the contract already documents no
// deactivate operation. Its 7 are create, list, get, aggregate, backfill,
// feed and reconciliation, three of which this gear does not serve yet.
assert_eq!(yaml_ops(&doc).len(), 7, "expected 7 documented operations");
```

```rust
// `registration_tests.rs`: drop the deactivate pair from `expected`. The
// test's own message already says "exactly the usage-record surface" —
// check it still reads true after the edit.
```

Then, for each general mechanism above, write its repointed test **before** deleting the original, so the coverage never lapses across a commit. For example, if `deactivate` was the vehicle for the by-id deny collapse, the surviving vehicle is `get_usage_record`:

```rust
#[tokio::test]
async fn a_denied_point_lookup_reads_as_not_found() {
    // The by-id surface must not act as an existence oracle: a caller who
    // is denied and a caller asking for a row that does not exist get the
    // same answer. Deactivation used to be the second by-id surface
    // carrying this rule; the point lookup is now the only one, so this
    // is where the rule is pinned.
    …
}
```

Search `service_tests.rs`, `handlers/usage_records_tests.rs` and `authz_tests.rs` for every deactivation test and classify it before writing anything.

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Implement**

Delete, in dependency order so the compiler walks you through it:

0. **Before anything else:** task 2 left `@cpt-begin`/`@cpt-end` markers at `infra/sdk_error_mapping.rs:140-141,158-159` naming `…algo-event-deactivation-atomic-outcome-mapping…inst-algo-outcome-already-inactive` and `…flow-usage-emission-compensation…inst-compensation-validate-fail`, wrapping a 409 arm whose prose it updated. It left them deliberately, because you own the deactivation flow. Move or drop them with the rest.
1. `plugins/noop-usage-collector-plugin/src/plugin.rs` — `deactivate_usage_record` impl, and its test.
2. `usage-collector-sdk/src/plugin_api.rs` — the SPI method and the `@cpt-*` markers on it. **Move** any marker whose id names a surviving concept; delete a marker only when its whole flow is gone. Do not invent replacement ids.
3. `usage-collector-sdk/src/api.rs` — the trait method and the `@cpt-begin/@cpt-end:…state-event-deactivation-record-lifecycle…inst-state-no-reactivation` pair around it. That state id names the deleted latch; the marker goes with it.
4. `usage-collector/src/domain/service.rs` — `Service::deactivate_usage_record` and everything reachable only from it, including `classify_deactivation_error` (or however it is spelled) and the `collapse_deny_to_not_found` call site. **`collapse_deny_to_not_found` itself survives** — the point lookup uses it. Its doc comment names both call sites; fix it. Same for `unrestricted_read_filter`. **Count its call sites with `grep -c` rather than trusting any prose, this plan's included** — it has three before your deletion and two after, and its own doc says "two" by counting logical sites where one bullet covers two functions. Leave the doc describing the state after this task; task 5 updates it again.
5. `usage-collector/src/domain/local_client.rs` — the forwarding method.
6. `usage-collector/src/domain/authz.rs` — `actions::DEACTIVATE`, and any `PdpOp` variant that exists only for it. Check `authorize_usage_record`'s doc, which names `DEACTIVATE` as one of its two actions.
7. `usage-collector/src/gts/permissions.rs` — the `usage_record_deactivate.v1` `gts_instance!` block **and** its entry in the inline `EXPECTED_PERMISSION_IDS` list.
8. `usage-collector/src/domain/ports/metrics.rs` — `DeactivationErrorCategory`, `record_deactivation_request`, its no-op impl, and the re-export in `ports/mod.rs`. `RequestOutcome`'s doc says its vocabulary is "shared by the query and deactivation request counters" — it is now the query counter's alone. Fix that sentence.
9. `usage-collector/src/infra/metrics.rs` — `uc_deactivation_requests_total` and `uc_deactivation_duration_seconds`. **Both are already absent from DESIGN §3.11.5's inventory**, so this is the code catching up to the contract, not a contract change. Say so in the commit body.
10. `usage-collector/src/api/rest/handlers/usage_records.rs` — `handle_deactivate_usage_record` and its `handlers/mod.rs` re-export.
11. `usage-collector/src/api/rest/routes/usage_records.rs` — the `OperationBuilder::post(".../{id}/deactivate")` block and its `@cpt-flow` / `@cpt-dod` markers.
12. `usage-collector/src/domain/test_support.rs` — `HappyPathPlugin`'s `deactivate_response` / `deactivate_input` fields, `set_deactivate_ok`, `last_deactivate_input`, and `MockPlugin`'s stub.

`parse_record_id` in the handler module is shared with `handle_get_usage_record`; it survives. Its doc says "get and deactivate handlers" — fix it.

- [ ] **Step 4: Run to verify they pass**

Full verification bar. The workspace should compile again here for the first time since task 1.

- [ ] **Step 5: Falsify**

This task removes behaviour, so mutation is the wrong instrument for most of it. Instead:

1. **Grep for survivors.** Shortest distinctive fragments, never phrases: `deactivat`, `Deactivat`, `inactive`, `Inactive`, `latch`, `cascade`, `one-way`. Across `--include="*.rs"` and excluding `plugins/timescaledb-`. Report the count and every remaining hit with a justification, or fix it.
2. **Re-add the route** and confirm `harness_sees_the_whole_rest_surface` and the `registration_tests` route list **both** fail. If only one fails, the other is not pinning what it claims.
3. For each repointed mechanism test, run its own mutation: e.g. make the point lookup return `PermissionDenied` rather than collapsing to `NotFound`, and confirm the repointed test fails. **A repointed test that does not fail under the mutation its original caught is a coverage loss disguised as a move — report it.**

- [ ] **Step 6: Commit**

Subject: `feat(usage-collector)!: delete the deactivation surface`

Body: a correction is an appended entry on the ordinary ingestion path, so no dedicated endpoint, SDK method, permission or storage call exists; the two deactivation metrics were already absent from the design's inventory.

`BREAKING CHANGE:` naming `UsageCollectorClientV1::deactivate_usage_record`, `UsageCollectorPluginV1::deactivate_usage_record`, `POST /usage-collector/v1/records/{id}/deactivate`, the `usage_record:deactivate` permission, and both metrics.

---

## Task 4: The faithful-copy comparator

**Files:**
- Create: `gears/system/usage-collector/usage-collector/src/domain/invalidation.rs`
- Create: `gears/system/usage-collector/usage-collector/src/domain/invalidation_tests.rs`
- Modify: `gears/system/usage-collector/usage-collector/src/domain/mod.rs`
- Modify: `gears/system/usage-collector/usage-collector/src/domain/validation.rs`, `validation_tests.rs`

A pure, synchronous module: given the submitted entry and the target the SPI returned, decide whether the entry is an admissible invalidation. No I/O, no metrics, no plugin handle. Task 5 wires it to the fan-out.

It replaces `SemanticsOutcome`, `validate_record_semantics` and `verify_l1_corrects_id` in `validation.rs`. **`validation.rs` keeps `validate_submit_record_metadata` and `DEFAULT_METADATA_SIZE_CAP_BYTES` and nothing else changes there** — but its module doc opens with three paragraphs about the compensation mechanism and "a later slice", and that whole preamble is now false. Rewrite it.

**One thing the comparator must NOT do, and it is a security rule rather than a style one.** A faithful-copy rejection may name **what differs, never what it differs from.**

`unrestricted_read_filter()` is `Expr::Value(Bool(true))`: the target lookup is a same-request shape check run *after* the submitter's own PDP authorization, and explicitly **not** a caller-scoped read — its own doc comment says so. The target row was therefore never authorized to this caller. A rejection saying "expected 42.5, got 7" turns a validation diagnostic into an **oracle**: submit a faithful copy with one field deliberately wrong, read the target's real value out of the 400, iterate per field, and you have reconstructed a record you have no scope for.

The field *name* leaks nothing — the caller sent that field, and they already know the target's id because they named it. The value is the only new information in the message and it is exactly the part that must not cross. Task 2's `invalidation_field_mismatch` detail gets this right deliberately; keep it that way, and put the rule in a comment so the next reader does not "improve" the diagnostic.

**Why a separate module rather than more of `validation.rs`:** `validation.rs` validates a submission against a *declaration*; this validates a submission against *another entry*. Different input, different failure vocabulary, and the file that has to be read whole to check the copy rule should be the copy rule alone.

**The comparison set is closed and it is the point.** Every caller-supplied field is compared; exactly three are exempt. Write it so that adding a field to `UsageRecord` and forgetting it here is *hard* — a destructuring binding, not a chain of `!=`:

- [ ] **Step 1: Write the failing tests**

Create `invalidation_tests.rs`. The input shapes this code distinguishes — you need **one test per row**, because a single "some field differs" test cannot tell a comparator that checks one field from one that checks eight:

| Shape | Expected |
| --- | --- |
| Entry equals target in every compared field | `Ok(())` |
| Target carries its own `invalidates` | `InvalidationTargetNotRecord`, field `invalidates` |
| `tenant_id` differs | mismatch naming `tenant_id` |
| `gts_type_id` differs | mismatch naming `gts_type_id` |
| `resource_ref.resource_id` differs | mismatch naming `resource_ref` |
| `resource_ref.resource_type` differs | mismatch naming `resource_ref` |
| `subject_ref` present vs target's absent | mismatch naming `subject_ref` |
| `subject_ref` absent vs target's present | mismatch naming `subject_ref` |
| `subject_ref` present on both, differing | mismatch naming `subject_ref` |
| `window_start` differs | mismatch naming `window_start` |
| `window_end` differs | mismatch naming `window_end` |
| `value` differs | mismatch naming `value` |
| `value` is the target's **negated** | mismatch naming `value` |
| `metadata` differs by one key's value | mismatch naming `metadata` |
| `metadata` has an extra key | mismatch naming `metadata` |
| `idempotency_key` differs | `Ok(())` — a permitted departure |
| `reason_code` set on the entry, absent on the target | `Ok(())` — a permitted departure |

Two of those rows are the ones an implementer skips, so write them explicitly:

```rust
#[test]
fn subject_presence_against_absence_is_a_mismatch() {
    // `cpt-cf-usage-collector-adr-append-only-invalidation` names this
    // case specifically: a comparator written as
    // `a.zip(b).all(|(x, y)| x == y)` passes it, because zipping a `Some`
    // with a `None` yields nothing to disagree about.
    let target = record_with_subject(Some(subject("s-1")));
    let entry = withdrawal_of(&target, |e| e.subject_ref = None);
    assert_mismatch(verify(&entry, &target), "subject_ref");

    let target = record_with_subject(None);
    let entry = withdrawal_of(&target, |e| e.subject_ref = Some(subject("s-1")));
    assert_mismatch(verify(&entry, &target), "subject_ref");
}

#[test]
fn the_quantity_is_echoed_and_never_negated() {
    // Echo, not compensation: the copied quantity restates what is
    // withdrawn so a reader of the withdrawal alone can see what it
    // removes. A negated one is the signed-compensation model this gear
    // rejected, and a fold that admitted it would double-count the
    // measurement it was meant to remove.
    let target = record_with_value(dec("42.5"));
    let entry = withdrawal_of(&target, |e| e.value = dec("-42.5"));
    assert_mismatch(verify(&entry, &target), "value");
}
```

And the guard that keeps the comparison set from silently shrinking:

```rust
#[test]
fn every_compared_field_has_a_case_above() {
    // The comparator destructures the target so a new `UsageRecord` field
    // is a compile error rather than a silently uncompared one. This
    // asserts the other half: that each compared field is exercised by a
    // case here, by walking the names the comparator can return.
    //
    // Read as a checklist, not a mechanism — it is a literal list, and it
    // is the thing a reviewer diffs against the struct.
    assert_eq!(
        COMPARED_FIELDS,
        [
            "tenant_id",
            "gts_type_id",
            "resource_ref",
            "subject_ref",
            "window_start",
            "window_end",
            "value",
            "metadata",
        ],
    );
}
```

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Implement**

```rust
//! The invalidation rules the gateway can decide from the target alone.
//!
//! An invalidation entry is a **faithful copy** of the entry it withdraws:
//! every caller-supplied field equals the target's, and the departures are
//! closed and are exactly three — the entry's own idempotency key, the
//! `invalidates` reference, and the `reason_code`
//! (`cpt-cf-usage-collector-adr-append-only-invalidation`).
//!
//! CORRECTED after task 4: **four** of the six invalidation rules are not
//! here, not two, and one of them is not where the original sketch said.
//! This module decides exactly two — the target is itself a record, and
//! the copy is faithful. Of the rest: both-or-neither and the reason code
//! are **structural** in-process, enforced by `Invalidation` carrying both
//! halves in one type — so no in-process path can produce a half-shape at
//! all. On the wire there are **two** boundaries, not one, and an earlier
//! draft of this correction named only the first: a JSON body decoded
//! straight into `CreateUsageRecord` is refused by the SDK's
//! deserialization shadow, and a REST body — which does **not** take that
//! path, per that shadow's own doc — carries the flat pair into
//! `CreateUsageRecordRequest` and is refused at the fold in
//! `record_request_into_domain` with `InvalidationReferenceIncomplete`.
//! Three boundaries, one rule, none redundant — the same sentence task 7
//! is told to write, and it belongs in the comparator's module doc too,
//! which is the authoritative inventory;
//! valid-reference is the caller's lookup; and at-most-one belongs to the
//! store, the only place it can be made atomic with the entry it admits —
//! a gateway-side pre-read cannot exclude a concurrent second submission,
//! so the gateway does not attempt one and lifts the plugin's
//! `AlreadyInvalidated` instead.

/// The caller-supplied fields an invalidation copies, in comparison order.
///
/// The order is the diagnostic order: a submission differing in several
/// fields is rejected naming the first of these it differs in, so the
/// message is deterministic across runs rather than dependent on a hash
/// iteration order.
pub(crate) const COMPARED_FIELDS: &[&str] = &[
    "tenant_id",
    "gts_type_id",
    "resource_ref",
    "subject_ref",
    "window_start",
    "window_end",
    "value",
    "metadata",
];

/// Names the first field in which `entry` departs from `target`, or `None`
/// when the copy is faithful.
///
/// Both sides are **destructured** rather than field-accessed, so adding a
/// field to either shape fails to compile here instead of silently
/// joining the uncompared set. **The two ignore-counts differ and that is
/// not a bug**: the submission ignores `idempotency_key` and
/// `invalidation` (two); the target ignores `id`, `idempotency_key` and
/// `invalidation` (three), because the target has an identity the
/// submission has not acquired yet. A destructure-audit that expects one
/// number on both sides will come out short and go looking for a defect
/// that is not there. The permitted departures and the
/// server-assigned fields are bound and discarded by name, each with the
/// reason it is not compared.
// CORRECTED after task 4. The original sketch took `entry: &UsageRecord`,
// which contradicts this function's own doc: with two `UsageRecord`s the
// ignore-counts are 3 and 3, not 2 and 3. The submission side is a
// `CreateUsageRecord` — it has no identity yet, which is the honest reason
// `id` is not compared. (The sketch's stated reason, that `id` "is equal
// whenever the five dedup inputs are", is false on either side: an
// invalidation's derived id necessarily differs from its target's, because
// the idempotency key differs.)
fn faithful_copy_mismatch(entry: &CreateUsageRecord, target: &UsageRecord) -> Option<&'static str> {
    let UsageRecord {
        // Server-assigned: derived from the five dedup inputs, so it is
        // equal whenever those are — comparing it would restate the
        // comparisons below.
        id: _,
        tenant_id,
        gts_type_id,
        resource_ref,
        subject_ref,
        window_start,
        window_end,
        value,
        metadata,
        // Permitted departure: the entry carries its own key, distinct
        // from the target's.
        idempotency_key: _,
        // Permitted departure: the target carries none, by the
        // no-invalidation-of-an-invalidation rule checked separately.
        // One binding, not two: the reference and its reason are one
        // field, which is the ADR's "exactly three departures" made
        // structural.
        invalidation: _,
    } = target;

    if &entry.tenant_id != tenant_id { return Some("tenant_id"); }
    // … one arm per COMPARED_FIELDS entry, in that order …
    None
}

/// Verifies a submitted invalidation against the target the SPI returned.
///
/// Runs the two rules a resolved target answers, in the order a caller can
/// act on: the target must itself be a record, then the copy must be
/// faithful.
///
/// CORRECTED after task 4. The original sketch justified the order by
/// saying a submission withdrawing an invalidation "would otherwise be
/// rejected for a field mismatch it cannot fix, because the target it
/// copied carries a `reason_code` and it does not". That is false:
/// `reason_code` lives inside `invalidation`, which **both** destructures
/// ignore, so a faithful copy of an invalidation target has no field
/// mismatch at all. The order still matters, for actionability — a
/// submission that is both wrong-target and unfaithful should hear about
/// the target, which is the error it must fix first. **A test whose entry
/// is a faithful copy of an invalidation target is an equivalent mutant
/// for the swap-the-checks falsification**; the entry must also depart in
/// some compared field for the swap to be observable.
///
/// # Errors
///
/// * [`UsageCollectorError::InvalidArgument`] with
///   `ValidationReason::InvalidationTargetNotRecord` when the target is
///   itself an invalidation.
/// * [`UsageCollectorError::InvalidArgument`] with
///   `ValidationReason::InvalidationFieldMismatch`, naming the field that
///   differs, when the copy is unfaithful.
pub(crate) fn verify_invalidation_target(
    entry: &UsageRecord,
    target: &UsageRecord,
) -> Result<(), UsageCollectorError> {
    if target.entry_type() == EntryType::Invalidation {
        return Err(UsageCollectorError::invalidation_target_not_record(target.id));
    }
    if let Some(field) = faithful_copy_mismatch(entry, target) {
        return Err(UsageCollectorError::invalidation_field_mismatch(field, target.id));
    }
    Ok(())
}
```

Take the `@cpt-dod:` markers that sat on `verify_l1_corrects_id` and decide each: `…dod-usage-emission-corrects-id-l1` names a retired concept and goes; `…dod-usage-emission-compensation-concurrency` likewise. **Do not carry a marker across just because a line survived.** State each decision in your report.

Then delete `SemanticsOutcome`, `validate_record_semantics` and `verify_l1_corrects_id` from `validation.rs`, and rewrite its module doc.

- [ ] **Step 4: Run to verify they pass**

`service.rs` will not compile — task 5 owns its call sites. Run `-p cf-gears-usage-collector-sdk` plus a targeted `cargo check` and report the intermediate state honestly. If the reviewer prefers a compiling tree per commit, fold tasks 4 and 5 into one commit; do **not** stub the service to fake it.

- [ ] **Step 5: Falsify**

The input-shape table above **is** the enumeration; confirm you have one test per row before mutating.

1. Drop one comparison arm at a time — **all eight, separately.** Each must break exactly its own test. Report any arm whose removal breaks nothing.
2. Change the `subject_ref` comparison to `entry.subject_ref.zip(target.subject_ref.as_ref()).is_none_or(|(a, b)| a == b)` — the classic presence-against-absence hole. Both `subject_presence_against_absence_is_a_mismatch` assertions must fail.
3. Compare `value.abs()` → `the_quantity_is_echoed_and_never_negated` must fail.
4. Swap the two checks in `verify_invalidation_target` so the copy check runs first → the target-not-record test must fail (it will report a field mismatch instead). **If it still passes, the test is asserting on the variant and not on the reason — fix the test.**
5. Return `Some("tenant_id")` unconditionally from a matching arm → the per-field tests must fail on the *name*, not just on the variant.

- [ ] **Step 6: Commit**

Subject: `feat(usage-collector): compare an invalidation against the entry it withdraws`

Body: why the comparator destructures (and that destructuring **both** shapes is what makes a field added to `CreateUsageRecord` alone a compile error — the strongest argument for the asymmetric signature); why the kind check precedes the copy check, in the actionability framing and not the retracted causation; and which **four** of the six rules live elsewhere and why.

---

## Task 5: The target pre-check fan-out

**Files:**
- Modify: `gears/system/usage-collector/usage-collector/src/domain/service.rs`, `service_tests.rs`, `service_metrics_tests.rs`
- Modify: `gears/system/usage-collector/usage-collector/src/domain/ports/metrics.rs`
- Modify: `gears/system/usage-collector/usage-collector/src/infra/metrics.rs`, `metrics_tests.rs`

**Read `resolve_l1_lookups` (`service.rs:475`) and `create_usage_record_inner`'s in-line lookup (`service.rs:807`) before writing anything.** They already solve the problem this task has — dedup by distinct target id, bounded fan-out, per-index projection into `results` or `eligible`, and a preserved `semantics → lookup → metadata` error-priority ordering. This is a **rewrite of a working shape**, not a new design. The single-record path and the batch path both need it, and they are two separate code paths today; keep them that way.

**Two things land on you that the file table did not list.**

**`domain/authz_tests.rs` is owned by no task and blocks the host from ever compiling.** Task 4 found 7 errors there — `UsageRecord` fixture literals still carrying `status` / `corrects_id`. It is a domain test file and you are the last domain task, so it is yours: repoint the fixtures to `invalidation: None`, and give a per-test verdict for anything you have to do beyond a mechanical fixture repoint.

**The comparator trusts you to pair correctly, and that trust is what stands between its diagnostic and an oracle.** `verify_invalidation_target` never checks that `entry.invalidation.target == target.id` — the ADR assigns reference resolution to the caller, so it takes the row it is handed. Task 4's reviewer probed it: an entry naming one uuid while a *different* row is passed in produces a 400 echoing **the row's** uuid, which the caller never supplied. Your per-index fan-out is the only place that mis-pairing can happen: the cache is keyed by target id, the results are projected back by input index, and a slip in either direction hands a caller a uuid from someone else's record. **Write a test that pins the pairing** — two pending invalidations against different targets, resolved in one batch, each rejection naming its own target — and mutate the projection to confirm it bites.

**You must thread the submission alongside its projection.** Task 4's `faithful_copy_mismatch` takes `entry: &CreateUsageRecord`, not `&UsageRecord` — the submission has no identity yet, which is the honest reason `id` is not compared, and it is what gives the two destructures their 2/3 ignore-counts. `try_into_usage_record` consumes `self`, so the invalidation path needs a clone of the submission, confined to the `invalidation.is_some()` branch. Task 4 verified the alternative is behaviourally equivalent (`time::OffsetDateTime`'s `PartialEq` compares instants, not offsets, so the projection's UTC normalisation cannot change a comparison) — but switching to `&UsageRecord` costs the asymmetry and forces a weaker justification for ignoring `id`. **Thread the clone unless you find a real problem with it, and say so either way.**

**What changes beyond renaming:**

- `PendingL1Lookup` → `PendingInvalidationTarget`; `L1LookupCache` → `InvalidationTargetCache`; `L1_LOOKUP_FANOUT_CONCURRENCY` → `TARGET_LOOKUP_FANOUT_CONCURRENCY` (same value, same reason — say the reason in the doc rather than pointing at the old constant).
- The trigger changes from `validate_record_semantics(&record)` returning `NeedsL1Lookup` to the record's own `invalidation`. There is no intermediate outcome enum any more: `record.invalidation` **is** the two-state value `SemanticsOutcome` was hand-rolling, carrying the target in its `Some` arm, so re-wrapping it would be pure ceremony. Delete `SemanticsOutcome` rather than renaming it (task 4 already did; this task removes its last use).
- The verification changes from `verify_l1_corrects_id` to `verify_invalidation_target`.
- The not-found error changes from `corrects_id_not_found` to `invalidation_target_not_found`.
- `record_kind_of` → `entry_type_of`, returning the metrics label for `uc_ingestion_records_total`. **The metric's label name changes from `record_kind` to `entry_type` and its values from `usage`/`compensation` to `record`/`invalidation`**, per DESIGN §3.11.5. The `origin` label the same table names is slice 5's; do not add it, and record the gap.
- **This plan asserts two things that conflict, and never says so.** Its error-taxonomy table routes "`invalidates` resolves to nothing" to `NotFound`, which carries no typed reason; DESIGN §3.11.5 assigns the reference rule to `invalidation_rule`. Both cannot hold. Task 5 discovered the undercount, decided it correctly (a bounded metric label must not be classified by substring-matching a caller-facing string), and recorded the reasoning in code — but the divergence entry landed in task 8, one task away from the evidence. **If you are re-running this task, name the conflict here and write the `DIVERGENCES.md` entry as this task's deliverable**, since this is where the evidence lives.
- `RecordErrorCategory` gains `InvalidationRule` (`"invalidation_rule"`) per §3.11.5, which "covers the copy, reference and at-most-one rules alone". `SemanticsViolation` stays — it still carries the metadata-adjacent and period rejections — but its doc names "an L1 `corrects_id` referential fault"; repoint it. `classify_record_error` routes the three new `ValidationReason` variants and `ConflictReason::AlreadyInvalidated` to `InvalidationRule`.
- `verify_invalidation_target` and `COMPARED_FIELDS` have **no non-test caller** until you wire them. A compiling host would warn `dead_code`; the host does not compile, so nothing surfaces today. **You close that — do not add an `#[allow]`.**
- `unrestricted_read_filter`'s doc enumerates its call sites. **It has two after task 3, not one** — an earlier draft of this bullet said one, while the API-facts table said two; the table is right and task 5 confirmed it by `grep -c` (3 occurrences, 2 physical call sites). This is ground rule 1 working as intended: the table's corrected count beat the task text's uncorrected one. Fix the doc, and keep its honest framing — this is a same-request shape check after the submitting caller's own PDP authorization succeeded, not a caller-scoped read.

**What does not change, and must be shown not to:** the error-priority ordering. A record that fails the target check *and* the metadata check reports the target failure. The existing code achieves that by deferring the metadata check behind the lookup; keep that structure and keep a test on it.

- [ ] **Step 1: Write the failing tests**

Input shapes this path distinguishes — one test each, on **both** the single-record and the batch path unless noted:

| Shape | Expected |
| --- | --- |
| Ordinary record, no `invalidates` | accepted; **no** `get_usage_record` dispatch at all |
| Invalidation whose target resolves and copies faithfully | accepted; exactly one `get_usage_record` dispatch |
| Invalidation whose target resolves to nothing | `NotFound` naming the target |
| Invalidation whose target is itself an invalidation | `InvalidationTargetNotRecord` |
| Invalidation differing from its target | `InvalidationFieldMismatch` naming the field |
| Invalidation the plugin rejects as `AlreadyInvalidated` | `Conflict(AlreadyInvalidated)` |
| Invalidation failing both the copy rule and the metadata rule | the **copy** rejection |
| Batch: two invalidations of the **same** target | exactly **one** `get_usage_record` dispatch (batch only) |
| Batch: one record, one good invalidation, one bad invalidation | index-aligned results, only the bad one rejected (batch only) |
| Plugin returns a transient error on the target lookup | `ServiceUnavailable`, not a rejection |

```rust
#[tokio::test]
async fn an_ordinary_record_costs_no_target_lookup() {
    // The fan-out is entered only by an entry carrying `invalidates`.
    // Asserting the *absence* of the dispatch is the point: a version
    // that looked up unconditionally would pass every acceptance test
    // above while doubling the plugin traffic of the common path.
    let plugin = HappyPathPlugin::new();
    …
    assert_eq!(plugin.get_usage_record_calls(), 0);
}

#[tokio::test]
async fn two_withdrawals_of_one_target_share_a_single_lookup() {
    // Dedup by distinct target id, which is what
    // `inst-algo-semantics-l1-dedup` bought and this rewrite must keep.
    // Both are rejected (the store admits at most one), but the rejection
    // is the plugin's, not a second read's.
    …
    assert_eq!(plugin.get_usage_record_calls(), 1);
}

#[tokio::test]
async fn a_copy_mismatch_outranks_a_metadata_rejection() {
    // Error priority: target rules before metadata rules. A submission
    // that breaks both is told about the copy, because the metadata it
    // was told to fix is metadata it must copy anyway.
    …
}
```

For the metrics, in `service_metrics_tests.rs`:

```rust
#[tokio::test]
async fn an_invalidation_counts_under_its_own_entry_type() {
    // `uc_ingestion_records_total` carries the correction share, which is
    // what makes a withdrawal visible in the ingestion profile at all.
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_ingestion_records_total", "entry_type", "invalidation"),
        1,
    );
}

#[tokio::test]
async fn an_invalidation_rule_rejection_carries_its_own_error_category() {
    assert_eq!(
        counter_sum_with_label(
            &exporter, "uc_ingestion_records_total", "error_category", "invalidation_rule",
        ),
        1,
    );
}
```

Check `counter_sum_with_label`'s real signature in `test_support.rs` before using it.

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Implement**

Rewrite `resolve_l1_lookups` in place:

```rust
/// Resolve every deferred invalidation-target check from
/// [`Service::create_usage_records`]'s validation loop.
///
/// Builds a request-local `Map<target_id, Result<UsageRecord, _>>` via a
/// bounded `get_usage_record` fan-out over the **distinct** targets — a
/// batch withdrawing one entry twice costs one read, not two — then for
/// every input index in `pending` runs
/// [`verify_invalidation_target`] and the deferred metadata check,
/// projecting the outcome into `results` (rejection) or `eligible`
/// (verified).
///
/// The metadata check stays behind the target check so a submission
/// breaking both is told about the copy: the metadata it would be told to
/// fix is metadata it has to copy from the target regardless.
///
/// At-most-one-invalidation is deliberately **not** checked here. Only the
/// store can make that check atomic with the entry it admits; a
/// gateway-side pre-read cannot exclude a concurrent second submission, so
/// it would be a check that fails exactly when it matters
/// (`cpt-cf-usage-collector-adr-append-only-invalidation`). The plugin's
/// `AlreadyInvalidated` is lifted on dispatch instead.
async fn resolve_invalidation_targets(…) { … }
```

In the batch loop, the trigger:

```rust
            // `invalidates` is the whole decision: its presence is what
            // makes the entry an invalidation, and there is no submitted
            // discriminator that could disagree with it. The lookup is
            // deferred to a post-loop dedup + bounded fan-out so a batch
            // withdrawing one target repeatedly reads it once.
            // CORRECTED after task 5: `Invalidation::target` is a public
            // FIELD, not a method, so `map(Invalidation::target)` does not
            // compile. Destructure or use a closure.
            if let Some(target_id) = record.invalidation.as_ref().map(|i| i.target) {
                pending_targets.push((index, record, target_id));
                continue;
            }
```

and the same in `create_usage_record_inner`, in-line, matching the existing single-record structure.

`record_kind_of` becomes:

```rust
/// `entry_type` label for a submitted entry: `invalidation` iff it names
/// the entry it withdraws, else `record`. Reads the submission's own
/// reference for the same reason the domain does — there is no submitted
/// discriminator, and the sign of a quantity carries no structural
/// meaning.
fn entry_type_of(record: &CreateUsageRecord) -> EntryType { … }
```

Reuse the SDK's `EntryType` as the label rather than declaring a second enum in `ports/metrics.rs`: one vocabulary, one spelling, and `EntryType::as_str` already exists for it. **If a `#[domain_model]` or orphan-rule constraint makes that impossible, say so and declare the label enum locally** — but check first, and report which way it went.

- [ ] **Step 4: Run to verify they pass**

Full verification bar. This is the first task since task 1 where the whole workspace and the whole suite should be green.

- [ ] **Step 5: Falsify**

Input shapes: the table in step 1 is the enumeration — **and run ground rule 2's "enumerate input shapes, not just mutations" over the method signature itself, which this task's own step-1 table failed to do.** `query_aggregated_usage_records` carries a caller `$filter` alongside the typed range, and both `invalidates` and `entry_type` are on the filterable schema. So `$filter=entry_type eq 'record'` selects a withdrawn target without its invalidation, and `$filter=invalidates eq <id>` selects the reverse — the one input shape that can split a pair, and the safety argument ("no `time_range` selects one without the other") does not cover it. The mutation list was thorough and the input list was blind, which is exactly the failure mode ground rule 2 describes.

1. Make every record enter the fan-out. **CORRECTED after task 5: this mutation is not expressible as written and its prediction is wrong.** An ordinary record has no target, so any such mutant must *invent* one — task 5 used `Uuid::nil()` and got **16** failures, not one. Run the faithful-to-intent variant instead: add a redundant unconditional lookup whose result is discarded. Task 5 measured that at **5** failures, **every one a lookup-count assertion and no acceptance test among them** — which is the finding the original prediction was reaching for, now demonstrated rather than asserted.
2. Collect pending targets into a `Vec` instead of a `HashSet` for the fan-out → `two_withdrawals_of_one_target_share_a_single_lookup` must fail.
3. Move the metadata check ahead of the target check → `a_copy_mismatch_outranks_a_metadata_rejection` must fail.
4. Map `ConflictReason::AlreadyInvalidated` to `RecordErrorCategory::PluginError` in `classify_record_error`. **CORRECTED after task 5: the victim named in an earlier draft was wrong.** This does *not* fail the end-to-end `invalidation_rule` metric test, because that test drives a **copy** rejection rather than an at-most-one one. The guard that actually bites is the `classify_record_error_maps_each_arm` table — exactly one failure. Predicting the wrong victim is how a real gap gets read as a passing mutation.
5. **CORRECTED after task 5: mutate the label *projection*, not `entry_type_of`.** Returning `EntryType::Record` from `entry_type_of` is caught by the single-record metric test, so it proves nothing about the batch path. The place the label can come apart from its entry is the per-index `zip` in the batch emit — replace `records.iter().map(entry_type_of)` with `map(|_| EntryType::Record)` there. Task 5's reviewer measured that mutation at **368 passed, 0 failed** against the tests the plan asked for, because both sketched metric tests drive `create_usage_record` alone.

   **The step-1 table's own rule was violated here, by this plan.** Its header says every shape gets a test on both the single-record and the batch path "unless noted", and two rows carry an explicit "(batch only)" marker — so the metric rows are both-path obligations by the header's own terms, and the sketches are single-path. Add a batch row: `[ordinary, withdrawal]` asserting `entry_type=record` is 1 and `entry_type=invalidation` is 1.
6. In the batch path, project a rejection to `results[0]` instead of `results[index]` → the index-alignment test must fail. If the suite has no batch test with a rejection at a non-zero index, **that is a missing input shape — add one before falsifying.**

- [ ] **Step 6: Commit**

Subject: `feat(usage-collector): resolve an invalidation's target before admitting it`

Body: the fan-out shape it inherits and why (dedup, bounded concurrency, preserved error priority); why at-most-one is not pre-checked; the metric label change and that the design's inventory already carries it.

---

## Task 6: The store's obligation and the fold

**Files:**
- Modify: `gears/system/usage-collector/usage-collector-sdk/src/plugin_api.rs`
- Modify: `gears/system/usage-collector/usage-collector/src/domain/test_support.rs`
- Modify: `gears/system/usage-collector/usage-collector/src/domain/service_tests.rs`
- Modify: `plugins/noop-usage-collector-plugin/src/plugin.rs`

Two halves, and the honest framing of each matters more than the code.

**Half one — the SPI obligations.** The gear folds nothing itself: `query_aggregated_usage_records` dispatches to the plugin and returns what it computes. So withdrawal exclusion is a **plugin obligation**, and this task's job is to state it normatively where a plugin author reads it, and to have the slice-6 contract suite be the thing that binds them to it. Do not dress a doc comment up as enforcement in your report.

**Half two — a reference implementation.** `test_support.rs` currently has no plugin double that stores anything: `HappyPathPlugin` replays a programmed response and `MockPlugin` refuses. That means nothing in this repo demonstrates the fold rule, and slice 6's contract suite would be written against nothing. Add a small in-memory folding double now, while the rule is fresh, and let slice 6 lift it into the contract suite.

**Be honest about what a test over that double proves.** It pins the reference semantics — the thing the contract suite will assert of every plugin — and it catches a *gear-side* regression that changed what the plugin is handed. It does not bind the TimescaleDB plugin to anything. Say exactly that in your report.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn a_withdrawn_pair_folds_to_nothing_while_both_stay_readable() {
    // The exclusion is load-bearing for correctness, not tidiness: a fold
    // that admitted the echoed quantity would double-count the very
    // measurement the withdrawal was meant to remove
    // (`cpt-cf-usage-collector-adr-append-only-invalidation`).
    //
    // Both entries carry one covered period, so no range selects one of
    // the pair without the other — which is why no placement of the
    // invalidation changes the result.
    let plugin = FoldingPlugin::new();
    plugin.store(measurement.clone());          // value 42.5
    plugin.store(other_measurement.clone());    // value 7.5
    plugin.store(withdrawal_of(&measurement));  // value 42.5, echoed

    let result = service.query_aggregated_usage_records(…, SUM, …).await.expect("folds");
    assert_eq!(single_bucket(&result), dec("7.5"));
    // CORRECTED after task 6: where the sketch below says the withdrawn-pair
    // answer is "zero", it is `None`. `AggregationBucket.value` is
    // `Option<BigDecimal>` and its own doc says `None` when no rows matched —
    // which is what SQL does for `SUM` over an empty set. `COUNT` is the
    // exception and returns `Some(0)`. A contract suite lifting this must
    // settle empty-set semantics deliberately rather than inherit the
    // implementer's judgement call.

    // …and the ledger path returns all three, as persisted. This is a
    // ledger, not a derived view: excluding a withdrawn pair from a
    // locally computed fold is the reader's obligation, and the
    // `invalidates` reference is what they read to do it.
    let page = service.list_usage_records(…).await.expect("lists");
    assert_eq!(page.items.len(), 3);
}

#[tokio::test]
async fn excluding_only_the_target_double_counts() {
    // The failure mode the rule exists to prevent, pinned as its own
    // case: a plugin that dropped the withdrawn record but kept the
    // echoed invalidation reports the measurement it was told to remove.
    // Under `SUM` over the pair alone, the wrong answer is 42.5 and the
    // right one is zero — so this fails loudly rather than by a rounding.
    …
}

// CORRECTED after task 6: **one withdrawn pair cannot test all five folds
// without a vacuous half.** With survivors `{7.5}` and one echoed quantity
// `q`, `MAX` moves only if `q > 7.5` and `MIN` only if `q < 7.5` — no single
// `q` satisfies both, so the loop below, taken literally with one pair, ships
// a `MIN` case that passes whether or not the pair leaked in. Withdraw **two**
// pairs whose quantities straddle the survivors, both ending later than either
// survivor so `LATEST` moves too.
//
// And do not write the loop as an `assert_eq!` per fold: the first failure
// aborts, so a mutation that breaks all five is only ever observed on `SUM`
// and the other four go unverified. Collect all five, then assert once. Task 6
// caught this in its own test before shipping it.
#[tokio::test]
async fn every_declared_fold_excludes_the_pair() {
    // The ADR's Confirmation asks for this across all five: withdrawal is
    // the primitive precisely because its meaning does not depend on what
    // a quantity means, and `MAX` / `MIN` / `LATEST` reverse under no
    // additional term at all.
    for fold in [SUM, COUNT, MAX, MIN, LATEST] { … }
}
```

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Implement**

On `UsageCollectorPluginV1::query_aggregated_usage_records`, add:

```rust
    /// **A withdrawn pair contributes nothing.** Where an accepted
    /// invalidation entry names a record, the plugin MUST exclude **both**
    /// from the fold — the withdrawn record and the invalidation itself
    /// (`cpt-cf-usage-collector-adr-append-only-invalidation`). Excluding
    /// only the record double-counts the measurement the withdrawal was
    /// meant to remove, because the invalidation echoes the quantity it
    /// withdraws rather than negating it. The rule holds under every
    /// declared fold, and needs no interpretation of what a quantity
    /// means.
    ///
    /// Both entries carry one covered period, so no `time_range` selects
    /// one of the pair without the other and no placement of the
    /// invalidation changes a result.
    ///
    /// A materialised aggregate MUST **recompute** over the affected
    /// range rather than absorb an appended term: no further term
    /// reverses `MAX`, `MIN` or `LATEST`. Append-only is a property of the
    /// ledger, not of a derived view.
```

On `list_usage_records` and `get_usage_record`, the contrary — **and write it in the normative mood.** An earlier draft of the sketch below was indicative ("a withdrawn pair *is returned* as persisted"), task 6 transcribed it faithfully, and the result reads as a description of the system sitting beside a real `MUST NOT` on the same method. This is the obligation a well-meaning backend author is most likely to break *helpfully* — hiding withdrawn rows from a ledger read is an obvious improvement to make. Say **MUST**.

```rust
    /// A withdrawn pair is returned **as persisted**, both entries, the
    /// invalidation naming its target. This is a ledger path, not a
    /// derived view, and it carries no filter that selects withdrawn
    /// entries in or out — the `invalidates` reference is what a consumer
    /// reads instead. Leaving a withdrawn pair out of a locally computed
    /// fold is the reader's obligation.
```

**`FoldingPlugin` lands in the host crate but its second life is in another one.** DESIGN §3.3 puts the plugin contract suite in `usage-collector-sdk`, so slice 6 lifts this fixture across a crate boundary this plan never named. That constrains what it may depend on: **keep it to `usage_collector_sdk`, `toolkit_odata` and the leaf numeric/uuid/time crates, and reach for no host-crate helper.** Task 6 happened to do this unprompted and the lift is therefore mechanical; nothing in the plan would have stopped it doing otherwise.

**`FoldingPlugin` must be a genuinely separate type.** `test_support.rs:1356` already aliases `RecordingPlugin = HappyPathPlugin`, so the programmable stub and the spy are one type wearing two names — pre-existing, and not something any task here should worsen. Do **not** add a third alias, and do **not** bolt a fold-exclusion flag onto `HappyPathPlugin`; that is the edit that tips this file from "two roles, documented" into "three roles, tangled".

Add `FoldingPlugin` to `test_support.rs`: a `Mutex<Vec<UsageRecord>>`, `store(record)`, `list_usage_records` selecting with `TimeRange::contains_window_end` and returning everything in range, and `query_aggregated_usage_records` folding after removing every entry that is an invalidation **or** is named by one. Keep it small; it is a fixture, not a backend. Do not implement `group_by` beyond what the tests need, and say in its doc what it does not implement so slice 6 knows what it is lifting.

The noop plugin needs no code change here, but its `query_aggregated_usage_records` returns empty buckets with no comment about why that is a conforming answer. Add one sentence.

- [ ] **Step 4: Run to verify they pass**

- [ ] **Step 5: Falsify**

**Before mutating, know that two of these are indistinguishable by any bucket value.** Task 6 measured it: excluding only the target and excluding only the invalidation produce *identical* fold results across all five folds, because the invalidation is a faithful copy and the two entries are interchangeable to a fold. They are not equivalent mutants — both are killed loudly — but no assertion on a value can tell them apart. **That is the ADR's echo showing through, and it is the reason running the two separately is the only way to know both directions are covered.**

Also note the plan's file list omits an edit this task needs: `recording_plugin_resolver`'s doc names `RecordingPlugin` as the reason it exists, and the folding double reuses it. Widen the doc rather than cloning a second resolver.

1. In `FoldingPlugin`, exclude only the invalidation and not its target → `a_withdrawn_pair_folds_to_nothing_while_both_stay_readable` must fail.
2. Exclude only the target and not the invalidation → `excluding_only_the_target_double_counts` must fail. **Run both separately** — one asymmetry is not the other, and a single "excludes the pair" test catches only one of them.
3. Make `list_usage_records` in the double filter out invalidations → the ledger half of test one must fail.
4. Apply mutation 1 and run the **whole** suite unfiltered. Report the number. If the only failures are the tests written in this task, say so plainly: it means the exclusion has exactly the coverage this task gave it and no more, which is the true state and the reason slice 6's contract suite exists.

- [ ] **Step 6: Commit**

Subject: `feat(usage-collector): state the fold's withdrawal exclusion as a plugin obligation`

Body: why the exclusion is load-bearing for correctness; why the ledger paths do the opposite; what the in-memory double does and does not prove.

---

## Task 7: The REST shapes

**Files:**
- Modify: `gears/system/usage-collector/usage-collector/src/api/rest/dto.rs`, `dto_tests.rs`
- Modify: `gears/system/usage-collector/usage-collector/src/api/rest/handlers/usage_records.rs`, `handlers/usage_records_tests.rs`

**One orphan lands on you, and it is the only place task 1's allowlist change actually broke something.** `handlers/usage_records_tests.rs:1541`, `orderby_on_a_mandatory_field_other_than_the_tiebreaker_is_accepted`, loops over `["tenant_id", "status"]` asserting that ordering by a mandatory field is accepted and still gains the canonical `(window_end, id)` suffix. Task 1 removed `status` from `KEYSET_SAFE_RECORD_FIELDS`, so the `status` arm now expects an accept and gets a rejection. Task 3 found it failing and correctly left it — the file is yours.

This is worth understanding rather than just fixing. The plan's API-facts table warned that changing that constant is **silent for a consumer**: it is a value change on an exported slice with no signature break. This test is the one place in the tree that proves the warning was not theoretical, and it stayed invisible for three tasks because the host crate could not compile. **Repoint the loop to a field that is still on the six-name allowlist** (`resource_id` or `resource_type`; `tenant_id` is already the other arm), keep the over-rejection guard the test exists for, and fix the comment, which currently calls `status` a mandatory NOT NULL column. Then confirm by mutation that the guard still bites.

The wire schemas already exist in `usage-collector-v1.yaml`. **You are conforming to them, not designing them.** Two properties of those schemas decide this task:

- `EntryType` is `readOnly: true` and appears on `UsageRecord` only. `CreateUsageRecordRequest` carries no discriminator at all, and the DTO is `#[serde(deny_unknown_fields)]`, so a submitted one is already refused — assert that rather than adding a check.
- `CreateUsageRecordRequest` has `dependentRequired: { invalidates: [reason_code], reason_code: [invalidates] }`. That is the schema's statement of both-or-neither.

**Where both-or-neither is enforced on this path, after amendment A1.** The domain type cannot hold a half-shape — `CreateUsageRecord` carries one `Option<Invalidation>` — so there is no projection-time check to surface. The REST DTO keeps the **flat pair**, because that is what the OAS declares, which makes `record_request_into_domain` the fold point and therefore the enforcement point on this path: two `Some`s build an `Invalidation`, two `None`s build `None`, and one of each is `invalidation_reference_incomplete` naming the missing half. That is one enforcement site, not a second one — the SDK's own shadow struct guards a different boundary (a JSON body deserialized straight into `CreateUsageRecord`, which no REST request does). Say which is which in a comment, because a reader finding two rejections for one rule will otherwise assume one is redundant.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_response_carries_a_derived_entry_type_and_no_status() {
    // `entry_type` is `readOnly` on the wire and derived in the domain, so
    // the projection computes it from the record's own reference — there
    // is nothing on `UsageRecord` to copy it from.
    let dto = UsageRecordDto::from(a_record());
    assert_eq!(dto.entry_type, "record");

    let dto = UsageRecordDto::from(an_invalidation());
    assert_eq!(dto.entry_type, "invalidation");
    assert_eq!(dto.invalidates, Some(TARGET_ID));
    assert_eq!(dto.reason_code.as_deref(), Some("emitter_duplicate"));

    // No status field survives anywhere on the wire: the model carries no
    // lifecycle flag and no row to rewrite.
    let json = serde_json::to_value(UsageRecordDto::from(a_record())).expect("serializes");
    assert!(json.get("status").is_none());
    assert!(json.get("corrects_id").is_none());
}

#[test]
fn a_submitted_entry_type_is_refused_as_an_unknown_field() {
    // `deny_unknown_fields` already does this; the test pins that the
    // ingestion shape stays discriminator-free, so a marker can never
    // disagree with the payload it marks.
    let body = serde_json::json!({ …a valid request…, "entry_type": "invalidation" });
    serde_json::from_value::<CreateUsageRecordRequest>(body)
        .expect_err("the ingestion shape accepts no discriminator");
}

#[tokio::test]
async fn a_reference_without_a_reason_is_a_400_naming_the_missing_half() {
    // Full-stack through the handler: the wire schema states this as
    // `dependentRequired`, the domain enforces it, and the handler must
    // surface it as a field violation the caller can act on rather than a
    // generic 400.
    let response = post_records(json!({ …, "invalidates": TARGET_ID })).await;
    assert_eq!(response.status(), StatusCode::MULTI_STATUS); // per-record rejection
    assert_field_violation(&body, "reason_code", "INVALIDATION_REFERENCE_INCOMPLETE");
}

#[tokio::test]
async fn an_unfaithful_copy_is_a_400_naming_the_field_that_differs() {
    // The diagnostic is the deliverable. A rejection saying only
    // "mismatch" leaves the emitter diffing two payloads by hand.
    …
    assert_field_violation(&body, "value", "INVALIDATION_FIELD_MISMATCH");
}
```

Check the real response-code convention in `handlers/usage_records_tests.rs` before asserting `MULTI_STATUS` — the batch surface returns `207` on a per-record rejection and a `Problem` on a request-wide one, and a single-record submission may differ. **Do not guess; read the existing assertions.**

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Implement**

`CreateUsageRecordRequest`: delete `corrects_id`, add

```rust
    /// The entry this submission withdraws. Supplying it makes the
    /// submission an invalidation and requires `reason_code`; omitting it
    /// makes the submission an ordinary record. There is no
    /// caller-supplied discriminator on this shape, so a marker cannot
    /// disagree with the payload it marks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalidates: Option<Uuid>,
    /// Why the withdrawal was issued. Both-or-neither with
    /// [`Self::invalidates`], which the wire contract states as
    /// `dependentRequired` and the domain projection enforces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
```

`UsageRecordDto`: delete `status` and `corrects_id`, add `invalidates: Option<Uuid>`, `reason_code: Option<String>`, and

```rust
    /// `record` or `invalidation`, **derived** from `invalidates` rather
    /// than stored. `readOnly` on the wire: it appears on this read shape
    /// and on no ingestion shape.
    pub entry_type: String,
```

with the `From<UsageRecord>` impl computing `value.entry_type().as_str().to_owned()` **before** it moves `invalidates` out of the record — order matters, and a borrow-after-move here is the compiler catching a real ordering bug rather than a nuisance.

`record_request_into_domain` in the handler: fold the flat pair into `Option<Invalidation>`, building the `ReasonCode` through `ReasonCode::new` and mapping its `UsageCollectorError` onto a `Problem` the same way the sibling newtype conversions in that function already do. The `(Some, None)` and `(None, Some)` arms raise `invalidation_reference_incomplete` naming the missing half; there is nowhere downstream that can, because the domain type makes the half-shape unrepresentable. Its doc comment says the function "is also where the record's `id` and initial `status` are stamped once" — false on both counts now; the id is stamped in the domain projection and there is no status. Fix it.

- [ ] **Step 4: Run to verify they pass**

Full verification bar, green.

- [ ] **Step 5: Falsify**

Input shapes: a request with neither field; with both; with `invalidates` alone; with `reason_code` alone; with a submitted `entry_type`; a response projected from a record; from an invalidation.

**Three corrections after task 7, all in this step.**

**D21 — `CreateUsageRecordRequest` has no `Serialize`.** `api_dto(request)` derives `Deserialize` only (`libs/toolkit-macros/src/api_dto.rs:74`), so "pin the exact top-level key set" cannot be a *serialization* assertion on the request shape. The real analogue is its **accepted**-key set on the deserialize side — the literal complement of `deny_unknown_fields`. Corollary: every `skip_serializing_if` on that struct is inert.

**D22 — `UsageRecordDto::from` has three call sites, not two.** An earlier draft cited `handlers/usage_records.rs:156, :258`. `grep -c` gives three: the point lookup, the list page, and `per_record_outcome` — the create-response path, where most full-stack assertions actually land. Another cardinality miscount of the kind this branch keeps producing.

**D23 — the `rust_decimal` equivalence is direction-specific, not crate-specific.** An earlier draft warned that removing `#[serde(with = "rust_decimal::serde::str")]` "may be an equivalent mutant in one crate and not the other". Task 7 measured it and the real rule is sharper: **both DTOs live in the same crate with the same feature set, and one *direction* is equivalent while the other is not.** Removing it from `UsageRecordDto::value` (serialize) is an equivalent mutant — the SDK's `serde-with-str` unifies in and plain `Decimal: Serialize` already emits a string. Removing it from `CreateUsageRecordRequest::value` (deserialize) is **killed**, because plain `Deserialize` also accepts a JSON number. Anyone reasoning per-crate gets this wrong.

**The mechanism, found by task 7's reviewer, states the rule better than either framing:** `libs/toolkit-macros/src/api_dto.rs:56,66` — `api_dto(request)` derives `Deserialize` only and `api_dto(response)` derives `Serialize` only. **So a serde attribute can only ever be live in one direction on any given DTO, and which direction depends on which macro form the struct carries.** Two corollaries, both measured: `skip_serializing_if` on a `request` struct is inert, and `#[serde(default)]` on a `response` struct is inert. Force a real change (`serialize_with` emitting `f64`) for the serialize side rather than removing an attribute.

**Before you mutate, pin the DTO wire encoding literally — task 1's blind spot is here too, one layer up.** `UsageRecordDto` and `CreateUsageRecordRequest` each carry their own `#[serde(with = "rust_decimal::serde::str")]` on `value`, and nothing pins *their* encoding against a literal. `UsageRecordDto` is what every REST response actually serializes (`handlers/usage_records.rs:156`, `:258`) — the SDK's `UsageRecord` never reaches an HTTP body. So the literal key-set-and-encoding assertion task 1 added to the SDK shapes covers none of what a client receives. Add the equivalent here for both DTOs: the exact top-level key set, `value` decoding as a JSON **string**, and the period bounds as RFC 3339.

**See D23 above for the actual rule.** An earlier draft explained this per-crate, which is wrong, and left the wrong explanation *after* its own correction in reading order — so a reader following the page top-to-bottom met the superseded version last. To falsify the encoding, force a real change (serialize the decimal as `f64`) rather than removing the attribute, and say which you did.

1. Hard-code `entry_type: "record".to_owned()` in the projection → the invalidation half of test one must fail.
2. Drop `#[serde(deny_unknown_fields)]` from `CreateUsageRecordRequest` → `a_submitted_entry_type_is_refused_as_an_unknown_field` must fail. **If it passes, the attribute is not where you think it is** — `api_dto` may apply its own, so check what the macro expands to.
3. Skip serializing `entry_type` when it is `"record"` → the wire assertion must fail. (`entry_type` is `required` on the OAS `UsageRecord`; it is never omitted.)
4. Have `record_request_into_domain` treat `(Some(target), None)` as an ordinary record — dropping the reference silently instead of rejecting → `a_reference_without_a_reason_is_a_400_naming_the_missing_half` must fail. **This is the mutation the fold made catchable and the flat pair did not**: before amendment A1 the domain re-checked it downstream, so a handler that dropped `reason_code` still produced the right status code for the wrong reason. Nothing downstream re-checks now, so this test is the only guard. Confirm that, and report if anything else fails alongside it.

- [ ] **Step 6: Commit**

Subject: `feat(usage-collector)!: carry the correction reference on the REST shapes`

`BREAKING CHANGE:` naming the removed `status` and `corrects_id` response fields and the removed `corrects_id` request field.

---

## Task 8: The sweep, the divergence, and the report

**Files:**
- Modify: any file the sweep finds
- Modify: `DIVERGENCES.md` (repo root)

No new behaviour. This task exists because slice 3's final review found five defects that a reader could have falsified by grep, all of them in the published SDK crate — the crate every task touched and no task owned. This task owns it.

- [ ] **Step 1: Check every hand-written codec has a *literal* wire assertion, not just a round-trip**

Task 1 learned this the expensive way and it is the one sweep that is not a grep. A round-trip proves the two halves of a codec agree **with each other**, never that either agrees with the contract — so a rename applied to *both* shadows of a type passed the entire suite, and so did dropping `rust_decimal::serde::str` from both, which turns `value` from a JSON string into a JSON number. That is precisely the precision bug the string encoding exists to prevent, called out in `UsageRecord::value`'s own doc and, until task 1's last amend, pinned by nothing.

Wherever this slice left a hand-written `Serialize` or `Deserialize` — task 1's four shadows, and anything task 7 added — confirm a **literal** key-set-and-encoding assertion sits beside the round-trip. Name any you find unguarded rather than fixing it silently, and say which of the six flat-shape declarations (see the follow-ups section) each assertion actually covers.

- [ ] **Step 2: Quarantine or repoint the e2e suite, and say which**

**Nothing in the `cargo` verification bar can see this, which is why it needs a step of its own.** `testing/e2e/suites/usage_collector/` is Python, invisible to `cargo check --workspace`, and it pins wire vocabulary this slice deleted:

- `test_integration_seams.py:109-136` (`test_deactivate_is_monotonic`) asserts `POST /records/{id}/deactivate` returns `204`, that a subsequent `GET` shows `body["status"] == "inactive"`, and that a second attempt is `409` with `problem["context"]["reason"] == "ALREADY_INACTIVE"`. All three are gone: the route (task 3), the field (task 1) and the wire constant (task 2).
- The same file's line ~39 asserts `body["status"] == "active"`.
- `test_authz_tenant_scoping.py` also references the retired vocabulary — grep it.

**The suite is already stale before this slice touches it**, which bounds what you owe it: `test_list_usage_types_includes_created` in the same file exercises usage-type catalog endpoints that slices 1-2 deleted, and `routes/` has had no usage-types module since. So this is pre-existing rot that this slice widens rather than creates.

Do **not** rewrite the suite — that is separate work and it needs a running deployment to verify. **Quarantine the affected tests explicitly**, with a skip reason naming the model change and this slice, so the next person reads a decision rather than a mystery. Then record in your report exactly which tests you quarantined, which were already broken before this slice, and what rewriting them would need. If you judge quarantine wrong, say what you did instead and why.

- [ ] **Step 3: Extract `TargetLookupDouble` from `HappyPathPlugin`**

`HappyPathPlugin` is now 18 fields and 23 methods under two names (`RecordingPlugin` aliases it), and task 5 added three fault knobs to it. Task 6 was required to keep its folding stand-in a genuinely separate type precisely so this file did not acquire a fourth role; that held, and this is the deferred half.

Task 5 identified the seam and it is clean: the `get_usage_record` half is self-contained — five fields (`get_record_response`, `get_record_by_id`, `get_usage_record_not_found`, the two transient maps) plus `get_usage_record_inputs` and `last_get_scope`, touched by exactly one SPI method and by nothing else in the stub. Give `TargetLookupDouble` those, have `HappyPathPlugin` hold one and delegate `get_usage_record` to it, and let the precedence order (transient → per-id transient → per-id row → not-found → shared row) live in one type instead of a doc comment on one setter.

Mechanical, no shared state to untangle. If it turns out not to be, stop and say so rather than half-doing it — this is a tidying step and it is the last thing that should put the suite at risk.

- [ ] **Step 4: Repoint the second allowlist orphan, which passes for the wrong reason**

`usage-collector/src/domain/query_tests.rs:650`, `an_order_on_a_domain_optional_attribute_is_rejected`, loops over `["subject_id", "subject_type", "corrects_id"]`. It **passes** — but `corrects_id` is refused as an *unrecognised name* by the fail-closed allowlist, not as the domain-optional attribute the test's own comment claims to be testing. So no test run will ever surface it, and the test asserts a true thing for a false reason.

It is the exact twin of the orphan task 7 fixed in `handlers/usage_records_tests.rs`, and task 7 found it while fixing that one but correctly left it — not its file. Repoint to `invalidates`, which is the field that is now genuinely domain-optional *and* on the filter surface. Confirm by mutation that the test distinguishes the two rejection grounds; if it cannot, say so rather than leaving a test that passes either way.

- [ ] **Step 5: Triage the 36 host `cargo doc` warnings that task 7 made visible**

`cargo doc --no-deps -p cf-gears-usage-collector` emits **36** warnings — unresolved intra-doc links and links to private items in `domain/authz.rs`, `module.rs`, `domain/validation.rs` and `routes/`. They are not new: the crate could not be documented at all between tasks 1 and 7, so nobody could see them. **This is the same "invisible while the crate could not compile" class as the two allowlist orphans**, and it is the last of that family.

Do not fix all 36 — most predate this slice. **Triage them**: how many name something this slice created, moved or deleted? Fix those; report the rest as a count with the categories, so a future reader knows the number is inherited rather than earned. If none is this slice's, say so plainly — that is a useful result and it takes one pass.

- [ ] **Step 6: Two one-phrase fixes the fold task surfaced but did not own**

**`error.rs:792` still reads "This is the plugin's *one* invalidation obligation", unqualified.** Task 6 scoped its new fold obligation from the *other* side ("a read-path obligation, distinct from the store's one admission-time invalidation rule"), which is a genuine resolution rather than papering over — the ADR's "one" counts the six *admission* rules ("Five rules at the gateway, one at the store"), and the ADR's own Traceability lists both the fold exclusion and the at-most-one check as SPI obligations. But a reader grepping the count still lands on an unqualified sentence in a second file, which is exactly ground rule 5's shape. Insert `admission-time` and the two sentences agree.

**And `DESIGN.md` §3.1:577 attributes Withdrawal exclusion to "Query Gateway + every plugin".** The Query Gateway enforces no part of it — `service.rs` composes the scope, dispatches, and caps the bucket count. Task 6's SPI doc says so outright ("The gear enforces neither"). This is the same shape as the `Period-end selection` row, which is also plugin-only in this code, so it is a **pre-existing DESIGN imprecision rather than something this slice introduced** — judge whether it earns a `DIVERGENCES.md` entry of its own or a sentence appended to an existing one, and say which you chose.

- [ ] **Step 7: Sweep the retired vocabulary**

Shortest distinctive fragments, never phrases (rustfmt does not reflow doc comments, so a multi-word grep undercounts by however many times an editor happened to wrap). Across `--include="*.rs"` and `--include="*.md"` under `gears/system/usage-collector/`, **excluding** `plugins/timescaledb-usage-collector-plugin/`:

```
corrects_id   CorrectsId   compensat    Compensat
status        Status       inactive     Inactive
deactivat     Deactivat    latch        cascade
one-way       four-cell    negat
```

`status` and `Status` will hit legitimately (`StatusCode`, HTTP status, `#[serde]` on unrelated types). Report the count, and for every remaining hit either fix it or justify it by name. **A justification of "unrelated" for more than a handful is a sign you did not read them.**

- [ ] **Step 8: Sweep the counts, not just the names**

**This is the sweep the earlier drafts of this plan had no instrument for, and task 3 proved the gap.** The prescribed grep fragments — `deactivat`, `inactive`, `latch`, `cascade`, `one-way` — are all retired *identifiers* and *mechanism verbs*. But a subtractive task's characteristic residue is **arithmetic**: "the SPI surface carries six methods" when it carries five; "one other call site" when there are two; "the per-verb vocabulary is identical" when one verb remains; "shared by the query and deactivation counters" when one counter is left. Task 3 fixed every retired identifier in its files and left four wrong counts, because nothing it was told to grep could see them.

Sweep doc comments in every file this slice touched:

```bash
grep -rnE '(^|[^a-z])(one|two|three|four|five|six|seven|eight|nine|ten|both|either|neither|each of|only other)([^a-z]|$)'   --include="*.rs" gears/system/usage-collector/ | grep -E '///|//!' | grep -v timescaledb
```

Check every hit against the tree. Report the count checked and every one you corrected.

- [ ] **Step 9: Sweep the mechanism-describing verbs**

The recurring defect shape is a rule stated in a second file at a different strength. Grep for the verbs, not the identifiers:

```
hands   enforces   needs no   guarantee   MUST   never   always   every read path
```

For each hit inside the usage-collector gear, ask: is this sentence still true after this slice? Five of slice 3's final findings were sentences that were true when written and became false three commits later.

Specific claims this slice makes false somewhere, as a starting list — **not an exhaustive one**:
- Anything saying the by-id surface has two enforcement layers (deactivation was the second).
- Anything saying `RequestOutcome` is shared by two counters.
- Anything describing the value-sign matrix, the counter/gauge distinction, or a negative quantity meaning a correction.
- Anything describing `KEYSET_SAFE_RECORD_FIELDS` as seven names.
- Anything saying keyset safety is decided by domain optionality alone.
- `models.rs`'s `LATEST` tie-break doc, which cites `acceptance_sequence` — a field the record does not carry. **Do not fix this one**; it is the gap flagged in "Deliberately not in this slice". Confirm it is still there and report it.

- [ ] **Step 10: Sweep the citations and the hedges**

- Every ADR cited by number in a shipped file (`ADR-0010`, `ADR-0007`, `ADR-0012`, …) → cite by `cpt-cf-usage-collector-adr-*` id. Six pre-existing `ADR-0012` citations for the retired catalog were found in slice 3 and left for "whoever finishes the slice-1/2 doc debt" (`config.rs:6`, `lib.rs:5`, `lib.rs:15`, `config_tests.rs:5`, `infra/sdk_error_mapping.rs:6`, `domain/error.rs:77`). **Check whether they are still there. If your work touched any of those files, fix that file's citation; otherwise report them and leave them.**
- Every hedge naming a later slice or a plan task. Name the state, not the plan. Task 6's SPI docs are the likeliest place one crept in.
- Every `@cpt-*` marker you moved. List them: id, from, to, why. Every marker you deleted: id, and why the flow it named is gone.

- [ ] **Step 11: Record the sixth divergence**

Append to `DIVERGENCES.md`, in the existing entry format (numbered heading, the sites, "What the code does", "Why the code is right", proposed wording, and whether it is load-bearing):

> **6. The invalidation rejections carry wire reasons the contract does not enumerate**

Content: `usage-collector-v1.yaml` names `IDEMPOTENCY_CONFLICT` and `VALIDATION` in prose and enumerates no invalidation-specific reason; DESIGN §3.11.5 names an `invalidation_rule` *metric* category but no *wire* reason. The gear now emits `INVALIDATION_REFERENCE_INCOMPLETE`, `INVALIDATION_TARGET_NOT_RECORD`, `INVALIDATION_FIELD_MISMATCH` and `ALREADY_INVALIDATED`. State that the code is the correct side (the ADR requires a rejection that names the field that differs, which a single `VALIDATION` code cannot express), that it is **load-bearing** for a client writing a typed matcher, and that slice 6 owns the final vocabulary pass.

**And a tenth entry — a client generated from the contract cannot submit a record at all.** This is the strongest-qualifying entry in the file and it is not yet in it.

> **10. The REST shapes and `usage-collector-v1.yaml` are incompatible, not merely divergent**

Assembled by task 7's quality review: `CreateUsageRecordRequest` in the yaml **requires `quantity`**; the DTO declares **`value`**; the DTO carries `#[serde(deny_unknown_fields)]`. A client generated from the published contract therefore gets a `400` for the unknown `quantity` *and* a missing-field failure for `value` — **it cannot submit a single record.** On the response side, `UsageRecord` in the yaml requires `accepted_at`, `acceptance_sequence` and `origin`, none of which exist on the type, and declares `additionalProperties: false` while the gear emits `value`. The emitted body is not a strict subset of the contract; it is a non-instance in two independent ways.

Mark it **load-bearing** — it clears that bar more decisively than any existing entry, because no generated client works at all. State that the `value`/`quantity` rename and the three server-assigned fields are out of this slice by explicit scope (`origin` is slice 5's; `accepted_at` / `acceptance_sequence` are claimed by no slice), and that `dto_tests.rs` now **pins the divergent shape in both directions**, so those assertions fail the day either gap closes — the tests and this document must say the same thing.

**And a ninth entry — `invalidation_rule` cannot cover the reference rule, because the reference rejection carries no typed reason.**

> **9. The `invalidation_rule` metric category under-counts the rules it claims**

Content: DESIGN §3.11.5 says `uc_ingestion_records_total`'s `invalidation_rule` category "covers the copy, reference and at-most-one rules alone". The copy and at-most-one rules are classifiable — they raise `ValidationReason::InvalidationFieldMismatch` / `InvalidationTargetNotRecord` and `ConflictReason::AlreadyInvalidated`. **The reference rule is not.** An unresolvable `invalidates` surfaces as `UsageCollectorError::NotFound`, and that variant carries **no typed reason** — nothing but `detail` prose separates it from an ordinary `usage_record_not_found`. Task 5 declined to classify a metric label off a message string and left it on `semantics_violation`, with the reasoning in the code.

State that the code is the correct side (classifying a bounded metric label by substring match on a caller-facing string is worse than a slightly wrong label), that the fix is a design decision — either `NotFound` grows a typed reason or §3.11.5's sentence narrows to the two rules it can actually cover — and that slice 6's reason-vocabulary pass is where it lands.

**And an eighth entry — the published quantity range is enforced and verified by nothing.**

> **8. The 28-digit quantity guarantee has no enforcement and no test**

Content: `usage-collector-v1.yaml`'s `UsageQuantity` publishes a precise range — at most 28 significant decimal digits and at most 28 after the point, magnitude below 10^28, smallest non-zero 1×10^-28, negative half included — and states *"Every storage plugin MUST round-trip that full range."* DESIGN §3.1's "Quantity fidelity" row repeats it, and §3.3 lists "quantity range" among the gateway responsibilities a plugin may therefore skip. **Nothing in the gear checks any of it**: `models.rs` and `validation.rs` validate `ReasonCode` length and metadata shape and say nothing about `Decimal` magnitude or scale, and no test exercises a boundary value. Found while fixing task 2's sign-rule doc — the same grep that showed nothing rejects a negative quantity showed nothing checks its range either.

State that this is a **gap in the code against three documents that agree with each other**, unlike the other entries where the code is the correct side; that it is pre-existing rather than introduced by this slice; and that slice 6's `quantity-round-trip` contract test is where the plugin half lands, leaving the gateway half unowned. Do not implement it here.

**And a seventh entry — `$orderby=entry_type` is refused, but both governing documents admit it.**

> **7. The order-key rule refuses a field the contract marks admissible**

Content: `usage-collector-v1.yaml`'s `OrderBy` parameter says *"Every key MUST name a field this shape marks required"* and gives the rejection list as *"(`subject_id`, `subject_type`, `invalidates`, a declared metadata property)"*. DESIGN §3.1's "Order admissibility" states the same rule with the same list. `entry_type` **is** in `UsageRecord.required` in the yaml and appears in neither rejection list — so per both documents `$orderby=entry_type` is admissible, while this slice makes it a `400` and pins that `400` with a test. State that the code is the correct side (amendment A3: the SDK carries no such attribute and obliges no plugin to materialize one, so it can promise no keyset over it), that it is **load-bearing** for a caller reading the contract to build an order, and propose adding `entry_type` to both rejection lists with the derived-field ground named. This one was missed when this plan was written — the plan checked the `Filter` parameter and never looked at the adjacent `OrderBy` — so it is a plan defect surfacing as a divergence, not something the implementation introduced.

Also update the file's "Evidence" section: it is pinned to `74e3d4429` and a test count of 724/6. Add this slice's HEAD and count as a second dated line rather than overwriting the first — the slice-3 evidence is what makes the first five entries checkable.

**Do not edit `DESIGN.md` or the yaml to resolve any of the six.** That is the spec owner's call.

- [ ] **Step 12: Final verification**

```bash
cargo check --workspace --all-targets
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk \
  -p cf-gears-noop-usage-collector-plugin --no-fail-fast
cargo clippy --workspace --all-targets --all-features
cargo +nightly fmt --check
git diff --stat main -- gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/
```

The last command **must print nothing.**

- [ ] **Step 13: Commit and report**

Subject: `docs(usage-collector): retire the compensation vocabulary the correction model replaced`

Then write the slice report. State, explicitly:

1. The verification-bar output and the test-count delta from the 606/6 baseline.
2. **Every falsification**: what you weakened, which test failed, the unfiltered `--no-fail-fast` count, and **anything that did not fail when you expected it to**.
3. **Every deleted test with its verdict** — name, what it pinned, why the question no longer exists. And every repointed one: what it used to be a test of, and what it is a test of now.
4. Any sketch in this plan that did not survive contact with the code.
5. Every `@cpt-*` marker moved or deleted.
6. **The residual divergences from the target contract this slice knowingly leaves.** At minimum:
   - `accepted_at` and `acceptance_sequence` absent, while `models.rs` documents the `LATEST` tie-break against `acceptance_sequence` and DESIGN says the plugin assigns it monotonically. **This blocks a conformant storage plugin** and no slice claims it.
   - `origin` absent from the record and from `uc_ingestion_records_total`'s label set — slice 5.
   - `value` not yet renamed to `quantity`.
   - The aggregate path still carrying `gts_type_id` / `$filter` / `metadata.<key>` as query parameters where `AggregationRequest` puts them in the body.
   - `entry_type` and `invalidates` on the filter surface but `origin` not yet, so `UsageRecordFilterField` is one field short of DESIGN §3.1's list.
7. The at-most-one decision and its reasoning, since it departs from `SLICE4.md`'s wording.

---

## Follow-ups this slice deliberately does not take

Recorded so they are decisions rather than oversights. None belongs to a task above.

- **Extract `authorize_batch` from `create_usage_records_inner`.** Still open from slice 3. The function is ~290 lines and this slice does not shrink it; the `inst-emit-batch-pdp` marker region is already contiguous and `fn`-shaped. It deserves its own reviewable commit rather than riding along with a model change.
- **One `ReadSelection { gts_type_id, time_range, query, metadata_filter }` shared by the paginated read SPI and the cursor fingerprint.** Still open from slice 3, and this slice does not touch the read SPI's signature, so the argument is unchanged: the correspondence between what the plugin selects on and what the fingerprint binds is conventional, and a future fifth row-selecting parameter can be threaded to the plugin without appearing in the fingerprint.
- **The byte-versus-code-point cap divergence**, now carried by three newtypes rather than two (`IdempotencyKey`, `MeterTypeId`, `ReasonCode`). All three fail closed. Fixing it means deciding which side is authoritative — the contract owner's call.
- **A gateway pre-check for at-most-one-invalidation.** Decided against; see "Two decisions taken before this plan". If the ADR is later amended to want one, it is one bounded `list_usage_records` fan-out over distinct targets and it belongs next to the target lookup.
- **The dangling `@cpt-flow` / `@cpt-algo` / `@cpt-dod` / `@cpt-state` markers.** Pre-existing, crate-wide, and it needs a decision — regenerate those id categories in the docs, or strip the retired marker classes. Explicitly out of scope until asked.
- **Refreshing `DECOMPOSITION.md` and `docs/features/*.md`**, both of which still describe event deactivation and compensation as the correction model. After this slice they describe a mechanism that exists nowhere in the code.
- **`FromStr` on `EntryType`.** Raised twice in task 1's reviews and narrowed each time, but it survives: `entry_type` is declared `String` on the filter surface, nothing in-process validates the value, and a plugin lowering `$filter=entry_type eq 'record'` must parse the vocabulary from a caller string. `AggregationFold` carries `FromStr` for exactly that reason. One call site is thinner than the two originally argued, but it is the one where a wrong answer is a silently wrong query rather than a compile error. Task 5 or the first real plugin will want it; not built here because nothing in this slice calls it.
- **The flat-shape declaration count.** After task 7 the same flat wire shape — `invalidates` and `reason_code` as siblings, both omitted on a record — is declared in **six** places: two SDK shadows on each of two entry types, plus the two REST DTOs. Task 1's literal-wire test pins two of the six. **Nothing pins the DTOs against the SDK**, and the two are converted by hand in `record_request_into_domain`. That is the shape of the next wire bug, and slice 6's re-enabled OpenAPI drift tests are its natural home — note it there rather than building a seventh declaration to check the other six.

## Self-review

**Spec coverage** — every slice-4 requirement from `SLICE4.md`, mapped to a task:

| Requirement | Task |
| --- | --- |
| Remove `status`, `UsageRecordStatus` | 1 |
| Remove `corrects_id` | 1 |
| Remove `deactivate_usage_record` | 3 |
| Remove the four compensation `ConflictReason` variants | 2 |
| Add `invalidates`, `reason_code` | 1 |
| `entry_type` derived from `invalidates.is_some()`, never stored, never submitted | 1 (domain), 7 (wire) |
| Add `AlreadyInvalidated` to `ConflictReason` | 2 |
| `invalidates` and `reason_code` both-or-neither | 1 (domain), 7 (wire, via the schema's `dependentRequired`) |
| Target resolves | 5 |
| Target is itself an ordinary record | 4 (rule), 5 (wiring) |
| Target not already invalidated | 2 (the lift), 6 (the obligation) — **plugin-enforced by decision** |
| Faithful copy, `subject_ref` presence against absence, rejection names the field | 4 |
| Quantity echoed, never negated | 4 |
| At most one invalidation per record | 2, 6 |
| Withdrawal exclusion in the fold; ledger reads return both | 6 |
| REST DTOs | 7 |
| `AlreadyInvalidated` on the SPI, without narrowing the rest of the taxonomy | 2 |

**Traps from `SLICE4.md`, mapped:**

| Trap | Where it is handled |
| --- | --- |
| `KEYSET_SAFE_RECORD_FIELDS` is public API, contains `status`, is interpolated into an error | Task 1, step 1 (both pinning tests) and step 6 (the trailer's "silent" note) |
| The `entry_type`-as-order-key guard | Task 1, step 1 — **and the finding that the existing guard cannot catch it**, hence the new converse test |
| Removing `UsageRecordQuery` fields is a loud compile break | API-facts table |
| `keyset_unsafe_record_fields_are_the_domain_optional_ones` needs repointing | Task 1, step 1 |
| The `validation.rs` surface to delete or replace | Task 4 |
| `resolve_l1_lookups` already solves the fan-out problem | Task 5, opening paragraph |
| The five error constructors naming the retired model | Task 2 |
| `#[non_exhaustive]` makes the removals silent | Task 2, step 6 |
| The wire schemas already exist; `EntryType` is `readOnly` | Task 7, opening |
| `accepted_at` / `acceptance_sequence` gap — flag, do not fix | Out-of-scope list, task 8 step 2 and step 6 |
| Three false-pass mechanisms in falsification | Ground rule 2 |
| Enumerate input shapes, not just mutations | Ground rule 2 and a shape table in tasks 4, 5, 7 |
| Grep on fragments, not phrases | Ground rule 5, task 8 steps 1-2 |
| Distrust the plan's sketches | Ground rule 1 |
| Falsification step per task | Every task, step 5 |
| Per-test verdicts on deletions | Ground rule 4, task 3 step 1 |
| Claims a reader can falsify by grep | Task 8, step 2 |
| Cite ADRs by id | Ground rule 5, task 8 step 3 |
| Hedges expire | Ground rule 5, task 8 step 3 |
| Do not commit while an implementer works | Ground rule 7 |

**Placeholders** — none. Every code step carries code or names the exact file and assertion to adapt. Where a sketch depends on a fixture name that must be read from the file first, the step says so.

**Type consistency** — `EntryType` / `EntryType::as_str` / `UsageRecord::entry_type()`, `ReasonCode::new` / `MAX_REASON_CODE_LEN`, `Invalidation::target` / `Invalidation::reason`, `ValidationReason::InvalidationReferenceIncomplete` / `InvalidationTargetNotRecord` / `InvalidationFieldMismatch`, `ConflictReason::AlreadyInvalidated`, `UsageCollectorPluginError::AlreadyInvalidated { id, invalidated_by }`, `faithful_copy_mismatch` / `verify_invalidation_target` / `COMPARED_FIELDS`, `resolve_invalidation_targets` / `PendingInvalidationTarget` / `TARGET_LOOKUP_FANOUT_CONCURRENCY`, `entry_type_of`, `RecordErrorCategory::InvalidationRule`, `FoldingPlugin` are spelled identically in every task that names them.

**One known inconsistency, deliberate:** task 1 adds `ValidationReason::InvalidationReferenceIncomplete` to make its tests compile, and task 2 owns the vocabulary. The implementer of task 1 is told to say so in their report rather than pretend the boundary is clean.

**Defects this plan shipped, found by task 1's reviews.** Recorded because ground rule 1 tells every implementer to distrust these sketches, and it is only honest to say where that paid off:

1. **The `map_value` lowering (task 1, step 3) was mechanically impossible** and reached two shipped doc comments before the quality review caught it. See amendment A2. Fifth defective sketch on this branch, first authored here.
2. **The `entry_type` order-key argument checked only half the contract** — the `Filter` parameter, never the adjacent `OrderBy` one, which admits any field the read shape marks required. The engineering call stands; it needed a `DIVERGENCES.md` row and this plan asserted it as uncontested. Now task 8's seventh entry.
3. **`non_negative_counter_compensation` was orphaned and assigned to no task** — task 1 made its doc false, task 4 deletes its only production caller, and no task owned deleting it. Now task 2's.
4. Four smaller sketch corrections the task-1 implementer pushed back on and won: `IdempotencyKey` and `MetadataKey` *do* share a serde mechanism; `MAX_REASON_CODE_LEN` is private, not `pub`; the fixture helper names did not exist; and one test name claimed the OAS rejects control characters when its `ReasonCode` schema carries no `pattern`.
