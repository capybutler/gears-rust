# Usage Collector Time Model Implementation Plan (slice 3)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the usage-collector's single `created_at` instant with a covered period `[window_start, window_end)`, derive the entry identity from ADR-0007's 5-tuple over that period, and make the read-path time range a typed parameter that selects on the period end.

**Architecture:** The covered period is caller-supplied on ingestion and validated before the identity derivation runs — a sub-microsecond bound and an inverted period are rejected, never truncated, because truncation would make a read-back entry derive an identifier different from the one it carries. The read paths stop carrying the window as a `$filter` conjunct: a validated `TimeRange` travels as a typed parameter on the SDK trait, the Plugin SPI and the REST surface, and selection is the single predicate `from <= window_end < to` on every path.

**Tech Stack:** Rust 2024, `time` 0.3.55 (`serde` + `formatting` + `parsing`, **no** `macros` feature), `uuid` v5, `toolkit-odata` (`ODataQuery` / `CursorV1` / `ODataOrderBy`), `toolkit-macros` `api_dto`, `cargo nextest`.

---

## Read this before you touch anything

### Authoritative documents

These carry the target model. Read the sections named, not the whole corpus.

| Document | What to read |
| --- | --- |
| `gears/system/usage-collector/docs/ADR/0007-cpt-cf-usage-collector-adr-record-identity-derivation.md` | All of it. **"The canonical pre-image"** and **"Confirmation"** are normative for tasks 2 and 3. |
| `gears/system/usage-collector/docs/ADR/0014-cpt-cf-usage-collector-adr-window-end-selection.md` | All of it. "Decision Outcome" + "Consequences" fix the selection rule. |
| `gears/system/usage-collector/docs/DESIGN.md` | §3.1 "Value Objects and Invariants" (period-end selection, point-event invariant, order admissibility, filter-surface reservation), §3.3 (SDK trait + Plugin SPI signatures, error taxonomy). |
| `gears/system/usage-collector/docs/usage-collector-v1.yaml` | `Timestamp`, `TimeRange`, `AggregationRequest`, `UsageRecord`, `CreateUsageRecordRequest`, `RangeFrom` / `RangeTo` / `OrderBy` parameters. |
| `gears/system/usage-collector/docs/schemas/usage_record.v1.schema.json` | `window_start`, `window_end`, `idempotency_key`, `id` property descriptions. |

**Do NOT implement from these — they are stale and describe a deleted model:**
`docs/DECOMPOSITION.md`, `docs/features/*.md`.

### Non-negotiable ground rules

1. **Distrust every code sketch in this plan.** Sketches were written against the tree at `add59d9b8` and are a starting point, not a specification. Verify each API against the real source before using it. If a sketch does not match reality, **push back and say so in your report** — do not bend code to make a sketch compile. Three of the defects found in slices 1-2 originated in plan sketches.
2. **Falsify every task before reporting done.** After the tests pass: deliberately weaken the branch the task exists to protect, confirm the corresponding test fails, restore, force a genuine rebuild, re-verify. Report the falsification honestly, including anything that did *not* fail when you expected it to.
   - Restoring with `mv` preserves mtime, so cargo skips the rebuild and your falsification silently passes. Use `cp`, then `touch` the file, and confirm a `Compiling cf-gears-…` line in the output before you trust the result. This produced one false pass in slice 2.
3. **Tests live in a sibling `*_tests.rs` file**, hooked with
   ```rust
   #[cfg(test)]
   #[cfg_attr(coverage_nightly, coverage(off))]
   #[path = "foo_tests.rs"]
   mod foo_tests;
   ```
   Never an inline `mod tests`.
4. **Deletions need a per-test verdict.** This slice deletes and repoints many tests. For every test you delete, state in your report: the test name, what it pinned, and why that question no longer exists. "Deleted the failing test" and "deleted the test whose question no longer exists" look identical in a diff.
5. **Comments must survive a grep.** When you rename a thing, grep for its old name in prose too. Three separate slice-1/2 reviews caught a comment naming something that had moved or vanished.
6. **Commits** are Conventional Commits with a `Signed-off-by: capybutler <capybutler@gmail.com>` trailer and a body that explains *why*, not what. A breaking change takes `!` in the subject and a `BREAKING CHANGE:` trailer. Do **not** bump crate versions — `chore: release` commits do that.

### Verification bar (every task ends here)

```bash
cargo check --workspace --all-targets
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk -p cf-gears-noop-usage-collector-plugin
cargo clippy --workspace --all-targets --all-features
cargo +nightly fmt
```

Clippy is deny-warnings in CI. Baseline entering this slice: **485 tests passing, 6 skipped** (the 6 are `#[ignore]`d OpenAPI drift tests that slice 6 re-enables — leave them alone).

### API facts verified against this tree — use these, do not re-derive them

| Fact | Consequence |
| --- | --- |
| `time::OffsetDateTime::to_utc()` returns `UtcDateTime`, a **different type** | Normalize with `.to_offset(time::UtcOffset::UTC)`, which returns `OffsetDateTime`. |
| The `time` dependency does **not** enable the `macros` feature | `format_description!` is unavailable. Build the canonical timestamp with `format!` and the component accessors. |
| `time`'s RFC 3339 parser turns a `:60` second at a valid leap-second position into `59.999999999` (`time-0.3.55/src/parsing/parsable.rs:858-880`), and rejects `:60` anywhere else | `time::Time` cannot represent second 60 at all, so ADR-0007's leap-second rejection is enforced *by the microsecond-precision check* — a stand-in always carries 999 999 999 ns. There is no separate branch to write, and a test must pin this end to end. |
| `u8::from(time::Month)` exists | Use it for the month component. |
| `gts::GtsTypeId` has no `FromStr` | `GtsTypeId::try_new(&str)`; in tests use `MeterTypeId::new(gts_id!("cf.core.uc.usage_record.v1~…~"))`. |
| `DomainError::internal(..)` does **not** exist | The bare `DomainError::Internal(String)` variant does. `UsageCollectorError::internal(..)` is a different type and does exist. |
| `toolkit_canonical_errors::CanonicalError` has no `is_not_found()` | Match `CanonicalError::NotFound { .. }`. |
| `toolkit_odata::short_filter_hash(Option<&ast::Expr>) -> Option<String>` | Returns `None` for `None`. FNV-1a over a normalized AST rendering. |
| `toolkit_odata::validate_cursor_against(&CursorV1, &ODataOrderBy, Option<&str>)` | The third argument is the effective filter hash; the check is skipped when either side is `None`. |
| `ODataQuery.filter_hash: Option<String>` is a public field | The gateway may overwrite it. `compose_query_with_scope` deliberately preserves it. |
| `impl TryFrom<LocalDto> for SdkType` is fine (precedent: `TryFrom<ResourceRefDto> for ResourceRef`) | But `TryFrom<LocalRequest> for Vec<ForeignType>` violates the orphan rule — use an inherent method (precedent: `QueryAggregatedUsageRecordsRequest::into_group_by`). |
| `HappyPathPlugin::calls()` / `last_fold()` count **only** `query_aggregated_usage_records` dispatches | For create-path assertions use `last_create_record_input()` / `last_create_records_input()`. Asserting `calls() == 1` in an ingestion test fails permanently. |
| `ServiceFixture` is the only fixture shape | `.with_source(..)`, `.with_cap(..)`, `.with_resolver(..)`, then `.build(..)` / `.build_with_default_resolver_handle(..)` / `.build_with_metrics(..)`. Do not reintroduce `service_with_*_and_*` functions. |
| `MeterTypeId::new` already rejects ASCII control characters and DEL | So the GTS type reference can never carry `0x1F`. Task 2 relies on this. |
| The 6 OpenAPI drift tests are `#[ignore]`d; `harness_sees_the_whole_rest_surface` asserts an operation **count** of 5 | Adding query parameters or body fields breaks no active contract test. Adding or removing a *route* would. This slice adds no route. |

### Deliberately not in this slice

Do not build these, and do not "while I'm here" them:

- `invalidates` / `reason_code` / `entry_type` / removing `status` and `corrects_id` — **slice 4**.
- `RecordOrigin`, `backfill_usage_records`, the live path's future/past tolerances — **slice 5**.
- Narrowing `UsageCollectorPluginError`, re-enabling the drift tests, the plugin contract suite — **slice 6**.
- `accepted_at` and `acceptance_sequence`. The target contract carries both, and no slice in the handoff claims them; they are feed machinery and the feed is out of scope. Leave them absent.
- Renaming `value` to `quantity`. Same reasoning — out of the handoff's slice list. Note it in your report if a reviewer asks.
- Moving `gts_type_id`, `$filter` and `metadata.<key>` from the aggregate path's query string into its request body. The target `AggregationRequest` puts all of them in the body; this slice moves **only** `time_range` there, because that is the field the time model owns. Record the residual divergence in your report.
- The usage feed, reconciliation, the ADR-0015 declaration mirror, ingestion quotas.
- Every file under `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/`. It is not a workspace member, `cargo check --workspace` cannot see it, and its production source already fails to compile against slice 2. **Touch nothing there.** `git diff --stat` must show zero files from that directory.
- Refreshing `docs/DECOMPOSITION.md` or `docs/features/*.md`.

### File structure

All paths are relative to `gears/system/usage-collector/`.

| File | Responsibility after this slice | Task |
| --- | --- | --- |
| `usage-collector-sdk/src/time_range.rs` *(new)* | `TimeRange` — a validated, UTC-normalized `[from, to)` read range, and the single spelling of ADR-0014's selection predicate. | 1 |
| `usage-collector-sdk/src/time_range_tests.rs` *(new)* | Its tests. | 1 |
| `usage-collector-sdk/src/id.rs` | ADR-0007's derivation over the 5-tuple, plus the canonical 27-character period-bound form. | 2 |
| `usage-collector-sdk/src/models.rs` | `UsageRecord` / `CreateUsageRecord` carry the covered period; the fallible projection that validates it before deriving identity; the filterable-field schema. | 2, 4 |
| `usage-collector-sdk/src/error.rs` | Gains the two period-precondition constructors and `invalid_time_range`; loses `missing_time_window`. | 1, 2, 3 |
| `usage-collector-sdk/src/reason.rs` | Loses `MISSING_TIME_WINDOW` / `ValidationReason::MissingTimeWindow`. | 3 |
| `usage-collector-sdk/src/api.rs` | SDK trait: `TimeRange` on both read methods. | 3 |
| `usage-collector-sdk/src/plugin_api.rs` | Plugin SPI: `TimeRange` on both read methods; period-end selection stated as a plugin obligation. | 3 |
| `usage-collector/src/domain/query.rs` | Loses `require_bounded_time_window` and its conjunct walker; keeps the reserved-field guard, with its forward-looking comment brought up to date. | 3, 4 |
| `usage-collector/src/domain/service.rs` | Create paths take the fallible projection; read paths take and forward the typed range. | 2, 3 |
| `usage-collector/src/domain/local_client.rs` | Forwards the typed range. | 3 |
| `usage-collector/src/domain/test_support.rs` | Doubles implement the new SPI and record the range they were handed. | 2, 3 |
| `usage-collector/src/api/rest/dto.rs` | Create DTOs carry the period; `TimeRangeDto`; the aggregate request body carries `time_range`. | 2, 3 |
| `usage-collector/src/api/rest/handlers/usage_records.rs` | Parses `from` / `to`, threads the range, keysets on `(window_end, id)`, binds the range into the cursor fingerprint. | 2, 3, 4, 5 |
| `usage-collector/src/api/rest/routes/usage_records.rs` | Declares the `from` / `to` query parameters. | 3 |
| `plugins/noop-usage-collector-plugin/src/plugin.rs` | Implements the new SPI signatures. | 3 |

---

## Task 1: `TimeRange` — the validated read range

**Files:**
- Create: `gears/system/usage-collector/usage-collector-sdk/src/time_range.rs`
- Create: `gears/system/usage-collector/usage-collector-sdk/src/time_range_tests.rs`
- Modify: `gears/system/usage-collector/usage-collector-sdk/src/lib.rs`
- Modify: `gears/system/usage-collector/usage-collector-sdk/src/error.rs`

Purely additive: nothing calls `TimeRange` yet. Task 3 threads it through.

Two design points to preserve, because both are load-bearing:

- The accessors are named `from_inclusive()` and `to_exclusive()`. Not `from()` / `to()` — an inherent method named `from` invites `clippy::should_implement_trait`, and the inclusivity is exactly the thing an off-by-one gets wrong. Encode it in the name.
- `contains_window_end` is the **only** place the predicate `from <= window_end < to` is spelled in the workspace. Test doubles and (in slice 6) the plugin contract suite call it rather than re-deriving it.

- [ ] **Step 1: Write the failing tests**

Create `usage-collector-sdk/src/time_range_tests.rs`:

```rust
use time::{Duration, OffsetDateTime, UtcOffset};

use crate::error::UsageCollectorError;
use crate::time_range::TimeRange;

fn at(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(secs).expect("in-range unix timestamp")
}

#[test]
fn new_accepts_an_ordered_range() {
    let range = TimeRange::new(at(1_700_000_000), at(1_700_003_600)).expect("ordered range");
    assert_eq!(range.from_inclusive(), at(1_700_000_000));
    assert_eq!(range.to_exclusive(), at(1_700_003_600));
}

#[test]
fn new_rejects_an_inverted_range() {
    let err = TimeRange::new(at(1_700_003_600), at(1_700_000_000))
        .expect_err("to < from must be rejected");
    let UsageCollectorError::InvalidArgument { field, .. } = err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert_eq!(field, "time_range");
}

#[test]
fn new_rejects_an_empty_range() {
    // `from == to` selects nothing at all: the predicate is
    // `from <= window_end < to`. Accepting it would report "no usage" for
    // what is really a caller passing one instant twice.
    TimeRange::new(at(1_700_000_000), at(1_700_000_000))
        .expect_err("from == to must be rejected");
}

#[test]
fn new_normalizes_both_bounds_to_utc() {
    let offset = UtcOffset::from_hms(5, 30, 0).expect("valid offset");
    let range = TimeRange::new(
        at(1_700_000_000).to_offset(offset),
        at(1_700_003_600).to_offset(offset),
    )
    .expect("ordered range");
    assert_eq!(range.from_inclusive().offset(), UtcOffset::UTC);
    assert_eq!(range.to_exclusive().offset(), UtcOffset::UTC);
    // Normalization moves the offset, never the instant.
    assert_eq!(range.from_inclusive(), at(1_700_000_000));
}

#[test]
fn selection_is_inclusive_at_the_lower_bound() {
    let range = TimeRange::new(at(1_700_000_000), at(1_700_003_600)).expect("range");
    assert!(range.contains_window_end(at(1_700_000_000)));
}

#[test]
fn selection_is_exclusive_at_the_upper_bound() {
    // ADR-0014: an entry whose period ends exactly on the upper bound
    // belongs to the NEXT range. This is what makes adjacent ranges sum
    // without double counting.
    let range = TimeRange::new(at(1_700_000_000), at(1_700_003_600)).expect("range");
    assert!(!range.contains_window_end(at(1_700_003_600)));
}

#[test]
fn a_point_event_on_the_lower_bound_is_selected() {
    // A point event is a zero-length period, not a separate shape: it has
    // window_start == window_end, and the one predicate selects it. The
    // rule ADR-0014 replaced dropped exactly this entry.
    let range = TimeRange::new(at(1_700_000_000), at(1_700_003_600)).expect("range");
    let instant = at(1_700_000_000);
    assert!(range.contains_window_end(instant));
}

#[test]
fn an_entry_wider_than_the_range_is_selected_by_neither_side() {
    // The predicate reads window_end and nothing else. An entry covering
    // [range.from - 1h, range.to + 1h) is invisible to the range, and to
    // the range before it.
    let range = TimeRange::new(at(1_700_000_000), at(1_700_003_600)).expect("range");
    let wide_entry_end = range.to_exclusive() + Duration::hours(1);
    assert!(!range.contains_window_end(wide_entry_end));

    let earlier = TimeRange::new(at(1_699_996_400), at(1_700_000_000)).expect("range");
    assert!(!earlier.contains_window_end(wide_entry_end));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo nextest run -p cf-gears-usage-collector-sdk time_range
```

Expected: compilation failure — `crate::time_range` does not exist.

- [ ] **Step 3: Add the error constructor**

In `usage-collector-sdk/src/error.rs`, next to the other `InvalidArgument` constructors:

```rust
    /// A read-path time range was empty or inverted (`to <= from`).
    ///
    /// The range is mandatory on every read path and selects an entry when
    /// `from <= window_end < to` (`cpt-cf-usage-collector-adr-window-end-selection`),
    /// so a range that is not strictly ordered selects nothing whatever is
    /// stored. Rejecting it names the caller's mistake instead of reporting
    /// an empty result that reads as "no usage".
    #[must_use]
    pub fn invalid_time_range(from: OffsetDateTime, to: OffsetDateTime) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "time_range".to_owned(),
            reason: ValidationReason::Validation,
            detail: format!(
                "time range requires from < to (got from={from}, to={to}); the range \
                 selects an entry when from <= window_end < to, so an empty or \
                 inverted range selects nothing"
            ),
        }
    }
```

Add `use time::OffsetDateTime;` to `error.rs` if it is not already imported.

`ValidationReason::Validation` (wire `VALIDATION`) is deliberate: the OpenAPI contract names no finer code for a malformed range, and slice 6 owns the final reason-vocabulary pass. Do not invent a new wire code here.

- [ ] **Step 4: Write the implementation**

Create `usage-collector-sdk/src/time_range.rs`:

```rust
//! The read-path covered-period range.
//!
//! Every read path takes a mandatory bounded range, and one rule says which
//! entries it selects: `from <= window_end < to`
//! (`cpt-cf-usage-collector-adr-window-end-selection`). The rule reads the
//! period **end** and nothing else, so it needs no case for a point event
//! (`window_start == window_end`), it partitions the entries so adjacent
//! ranges sum without double counting, and it lets a plugin serve a range
//! from a rollup keyed on one column.
//!
//! The range is a typed parameter on the SDK trait, the Plugin SPI and the
//! REST surface — never a `$filter` conjunct (DESIGN §3.3 rule 5). That is
//! why the covered-period fields are reserved on the filter surface: a
//! predicate over one would be a second, possibly contradictory, constraint
//! on something already fixed.

use time::{OffsetDateTime, UtcOffset};

use crate::error::UsageCollectorError;

/// A validated, UTC-normalized read range: `[from, to)`.
///
/// Constructed through [`TimeRange::new`], which is the only way to obtain
/// one — the fields are private, so an unordered range cannot reach a
/// storage plugin. `Copy`, so it passes by value on every surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    from: OffsetDateTime,
    to: OffsetDateTime,
}

impl TimeRange {
    /// Creates a range after normalizing both bounds to UTC and checking
    /// that it is strictly ordered.
    ///
    /// Normalization moves the offset, never the instant, so a caller that
    /// sends `13:00:00+01:00` and one that sends `12:00:00Z` obtain the same
    /// range — the same equivalence the covered period gets on ingestion.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorError::InvalidArgument`] when `to <= from`.
    pub fn new(from: OffsetDateTime, to: OffsetDateTime) -> Result<Self, UsageCollectorError> {
        let from = from.to_offset(UtcOffset::UTC);
        let to = to.to_offset(UtcOffset::UTC);
        if to <= from {
            return Err(UsageCollectorError::invalid_time_range(from, to));
        }
        Ok(Self { from, to })
    }

    /// The inclusive lower bound, in UTC.
    #[must_use]
    pub fn from_inclusive(&self) -> OffsetDateTime {
        self.from
    }

    /// The exclusive upper bound, in UTC.
    #[must_use]
    pub fn to_exclusive(&self) -> OffsetDateTime {
        self.to
    }

    /// Does this range select an entry whose covered period ends at
    /// `window_end`?
    ///
    /// The single spelling of `from <= window_end < to` in the workspace.
    /// Every surface that needs the predicate — test doubles, and the
    /// Plugin SPI contract suite — calls this rather than re-deriving it,
    /// because two spellings of one boundary rule is how the exclusive
    /// upper bound stops being exclusive on one path.
    #[must_use]
    pub fn contains_window_end(&self, window_end: OffsetDateTime) -> bool {
        let window_end = window_end.to_offset(UtcOffset::UTC);
        self.from <= window_end && window_end < self.to
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "time_range_tests.rs"]
mod time_range_tests;
```

In `usage-collector-sdk/src/lib.rs`, add the module (keep the module list alphabetical) and the re-export:

```rust
pub mod time_range;
```
```rust
pub use time_range::TimeRange;
```

Also add `TimeRange` to the crate-level doc comment's domain-models bullet, next to `UsageRecord`.

- [ ] **Step 5: Run the tests to verify they pass**

```bash
cargo nextest run -p cf-gears-usage-collector-sdk time_range
```

Expected: 8 tests pass.

- [ ] **Step 6: Falsify**

Weaken `contains_window_end` to `window_end <= self.to` (making the upper bound inclusive), rebuild, and confirm `selection_is_exclusive_at_the_upper_bound` fails. Then weaken `new` to `if to < from`, and confirm `new_rejects_an_empty_range` fails. Restore with `cp` + `touch`, confirm a `Compiling` line, re-run.

- [ ] **Step 7: Verification bar and commit**

Run the four commands from "Verification bar". Then:

```bash
git add gears/system/usage-collector/usage-collector-sdk/src/time_range.rs \
        gears/system/usage-collector/usage-collector-sdk/src/time_range_tests.rs \
        gears/system/usage-collector/usage-collector-sdk/src/lib.rs \
        gears/system/usage-collector/usage-collector-sdk/src/error.rs
git commit -s -m "feat(usage-collector): add the typed read-path TimeRange" -m "$(cat <<'BODY'
Every read path takes a mandatory bounded range, and ADR-0014 fixes one
rule for what it selects: from <= window_end < to. The rule reads the
period end alone, so it needs no case for a point event, it partitions
the entries so adjacent ranges sum without double counting, and it lets a
plugin serve a range from a rollup keyed on one column.

TimeRange carries that range as a validated value object: private
bounds, UTC-normalized on construction, strictly ordered, and
contains_window_end as the single spelling of the predicate in the
workspace. An empty or inverted range is rejected rather than served,
because it selects nothing whatever is stored and an empty result reads
as "no usage".

Additive for now. Threading it through the SDK trait, the Plugin SPI and
the REST surface — and retiring the $filter conjunct it replaces — is the
next commit.
BODY
)"
```

---

## Task 2: the covered period, and identity derived over it

**Files:**
- Modify: `usage-collector-sdk/src/id.rs`, `usage-collector-sdk/src/id_tests.rs`
- Modify: `usage-collector-sdk/src/models.rs`, `usage-collector-sdk/src/models_tests.rs`
- Modify: `usage-collector-sdk/src/error.rs`
- Modify: `usage-collector-sdk/src/lib.rs`
- Modify: `usage-collector/src/domain/service.rs`
- Modify: `usage-collector/src/api/rest/dto.rs`, `usage-collector/src/api/rest/dto_tests.rs`
- Modify: `usage-collector/src/api/rest/handlers/usage_records.rs`, `.../usage_records_tests.rs`
- Modify: `usage-collector/src/domain/{service_tests,service_metrics_tests,authz_tests,validation_tests}.rs`
- Modify: `usage-collector/src/domain/test_support.rs`
- Modify: `plugins/noop-usage-collector-plugin/src/plugin_tests.rs`

This is the largest task in the slice. It is one commit because the record shape, the derivation and the validation cannot be separated: a derivation that ran before validation would defeat the whole point of rejecting rather than truncating.

**The record `id` changes in this commit.** Slice 2 already changed it once. The gear is pre-release with no installations, so no migration is needed — but the golden vectors must be right, and they are given below as values computed **independently of this codebase** (Python's `uuid.uuid5`, which is RFC 4122 `SHA-1(namespace.bytes || name)`). Re-derive them yourself with the snippet in Step 1 before you trust them, and if your implementation disagrees with them, the implementation is wrong until proven otherwise.

### The canonical pre-image (ADR-0007, normative)

```text
id = UUIDv5(NS, tenant_id ⟨0x1F⟩ gts_type_id ⟨0x1F⟩ idempotency_key ⟨0x1F⟩ window_start ⟨0x1F⟩ window_end)
```

- `NS` = `56313026-863b-4de8-b32b-1f96b67306ed`, entering the digest as its **16 raw bytes**, never its text form. Unchanged — `Uuid::new_v5` already does this.
- `tenant_id`: lowercase hyphenated 36-character RFC 4122 form, UTF-8.
- `gts_type_id`: the wire string byte-exact, **terminator `~` included**.
- `idempotency_key`: the wire string byte-exact.
- `window_start`, `window_end`: `YYYY-MM-DDTHH:MM:SS.ffffffZ` — exactly 27 characters, uppercase `T` and `Z`, always six fraction digits, after UTC normalization.

Note the field **order** changed as well as the field set: the key now precedes the timestamps, where the old 4-tuple put `created_at` third. Both bounds enter, in start-then-end order.

Two preconditions are validation errors, not truncation:

- A bound finer than microsecond precision is rejected. Truncating would make a read-back entry derive an identifier different from the one it carries, which breaks offline reproduction at the point an emitter needs it.
- A second value of `60` is rejected. See the API-facts table: `time` normalizes a valid leap-second stand-in to `59.999999999`, so the precision check subsumes this and there is no second branch to write. A test must pin it anyway.

- [ ] **Step 1: Re-derive the golden vectors independently**

Run this and keep the output next to you. It does not read the Rust code.

```bash
python3 - <<'PY'
import uuid, datetime
NS = uuid.UUID("56313026-863b-4de8-b32b-1f96b67306ed")
US = "\x1f"
T1 = "11111111-1111-1111-1111-111111111111"
T2 = "22222222-2222-2222-2222-222222222222"
G1 = "gts.cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~"
G2 = "gts.cf.core.uc.usage_record.v1~cf.mini_chat._.messages_sent.v1~"

def ts(epoch_secs, micros=0):
    dt = datetime.datetime.fromtimestamp(epoch_secs, datetime.timezone.utc)
    s = "%04d-%02d-%02dT%02d:%02d:%02d.%06dZ" % (
        dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second, micros)
    assert len(s) == 27, (s, len(s))
    return s

def derive(tenant, gts, key, ws, we):
    return uuid.uuid5(NS, US.join([tenant, gts, key, ws, we]))

WS, WE = ts(1_700_000_000), ts(1_700_003_600)
print("window_start          ", WS)
print("window_end            ", WE)
print("base                  ", derive(T1, G1, "idem-1", WS, WE))
print("point event (ws == we)", derive(T1, G1, "idem-1", WS, WS))
print("key idem-2            ", derive(T1, G1, "idem-2", WS, WE))
print("other tenant          ", derive(T2, G1, "idem-1", WS, WE))
print("other gts type        ", derive(T1, G2, "idem-1", WS, WE))
print("bounds swapped        ", derive(T1, G1, "idem-1", WE, WS))
print("ws + 1 microsecond    ", derive(T1, G1, "idem-1", ts(1_700_000_000, 1), WE))
PY
```

Expected output (these are the values the plan was written against):

| Vector | `window_start` | `window_end` | Expected `id` |
| --- | --- | --- | --- |
| base, key `idem-1` | `2023-11-14T22:13:20.000000Z` | `2023-11-14T23:13:20.000000Z` | `5b075acb-e2e8-55a8-aedf-4c7d01b60284` |
| point event | `2023-11-14T22:13:20.000000Z` | *(same)* | `64902762-6487-570c-83c3-8975a2e1adb4` |
| key `idem-2` | base | base | `f7295cc9-530f-5c93-8020-9f1cd7c78498` |
| tenant `2222…` | base | base | `560fbb23-4eb6-508c-ab8d-4d2187acb69b` |
| type `…messages_sent.v1~` | base | base | `86f9359f-2d26-5fb3-b88a-d256910c6462` |
| bounds swapped | `…23:13:20` | `…22:13:20` | `6a75ef43-2619-58a0-b75c-53b08bd96a78` |
| `window_start` + 1 µs | `2023-11-14T22:13:20.000001Z` | base | `bf2e9ea3-746e-5521-8145-a99c37b54924` |

If your run disagrees with this table, **stop and report it** — do not proceed on either value.

- [ ] **Step 2: Write the failing derivation tests**

Rewrite `usage-collector-sdk/src/id_tests.rs`. Keep the existing helpers (`tenant()`, `gts()`, `key()`), replace `at()` with a two-bound pair, and replace every `created_at_micros` test — that function is deleted in Step 4, because validation replaces truncation.

```rust
use time::{OffsetDateTime, UtcOffset};
use toolkit_gts::gts_id;
use uuid::Uuid;

use crate::id::{USAGE_RECORD_ID_NAMESPACE, canonical_period_bound, derive_usage_record_id};
use crate::models::{IdempotencyKey, MeterTypeId};

fn tenant() -> Uuid {
    Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap()
}
fn gts() -> MeterTypeId {
    MeterTypeId::new(gts_id!(
        "cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~"
    ))
    .unwrap()
}
fn key(s: &str) -> IdempotencyKey {
    IdempotencyKey::new(s).unwrap()
}
/// `2023-11-14T22:13:20.000000Z`
fn ws() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}
/// `2023-11-14T23:13:20.000000Z`
fn we() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_003_600).unwrap()
}
fn expect(raw: &str) -> Uuid {
    Uuid::parse_str(raw).unwrap()
}

#[test]
fn derive_is_deterministic() {
    assert_eq!(
        derive_usage_record_id(tenant(), &gts(), &key("idem-1"), ws(), we()),
        derive_usage_record_id(tenant(), &gts(), &key("idem-1"), ws(), we()),
    );
}

#[test]
fn derive_matches_golden_vector() {
    // UUIDv5(NS, "11111111-1111-1111-1111-111111111111" 0x1F
    //            "gts.cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~" 0x1F
    //            "idem-1" 0x1F
    //            "2023-11-14T22:13:20.000000Z" 0x1F
    //            "2023-11-14T23:13:20.000000Z")
    //
    // Computed independently of this crate (RFC 4122 UUIDv5 over the
    // ADR-0007 pre-image) — see the plan's derivation snippet. DO NOT
    // hand-edit: regenerate both sides and reconcile.
    assert_eq!(
        derive_usage_record_id(tenant(), &gts(), &key("idem-1"), ws(), we()),
        expect("5b075acb-e2e8-55a8-aedf-4c7d01b60284"),
    );
}

#[test]
fn derive_produces_a_v5_uuid() {
    assert_eq!(
        derive_usage_record_id(tenant(), &gts(), &key("idem-1"), ws(), we()).get_version_num(),
        5,
    );
}

#[test]
fn namespace_is_pinned() {
    // Fixed forever: a change re-maps every identifier the gear has issued.
    assert_eq!(
        USAGE_RECORD_ID_NAMESPACE,
        expect("56313026-863b-4de8-b32b-1f96b67306ed"),
    );
}

#[test]
fn distinct_keys_yield_distinct_ids() {
    assert_eq!(
        derive_usage_record_id(tenant(), &gts(), &key("idem-2"), ws(), we()),
        expect("f7295cc9-530f-5c93-8020-9f1cd7c78498"),
    );
}

#[test]
fn distinct_tenants_yield_distinct_ids() {
    let other = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
    assert_eq!(
        derive_usage_record_id(other, &gts(), &key("idem-1"), ws(), we()),
        expect("560fbb23-4eb6-508c-ab8d-4d2187acb69b"),
    );
}

#[test]
fn distinct_gts_ids_yield_distinct_ids() {
    let other = MeterTypeId::new(gts_id!(
        "cf.core.uc.usage_record.v1~cf.mini_chat._.messages_sent.v1~"
    ))
    .unwrap();
    assert_eq!(
        derive_usage_record_id(tenant(), &other, &key("idem-1"), ws(), we()),
        expect("86f9359f-2d26-5fb3-b88a-d256910c6462"),
    );
}

#[test]
fn a_different_covered_period_yields_a_different_id() {
    // The point of the 5-tuple: one stable per-meter idempotency key covers
    // many periods, and two periods are two entries rather than a collision.
    let one_micro_later = ws() + time::Duration::microseconds(1);
    assert_eq!(
        derive_usage_record_id(tenant(), &gts(), &key("idem-1"), one_micro_later, we()),
        expect("bf2e9ea3-746e-5521-8145-a99c37b54924"),
    );
    assert_ne!(
        derive_usage_record_id(tenant(), &gts(), &key("idem-1"), ws(), we()),
        derive_usage_record_id(tenant(), &gts(), &key("idem-1"), ws(), we() + time::Duration::microseconds(1)),
        "window_end is an input too",
    );
}

#[test]
fn the_two_bounds_enter_in_a_fixed_order() {
    // A concatenation that treated the bounds symmetrically would derive one
    // id for [22:13, 23:13) and [23:13, 22:13). The second is rejected
    // upstream, but the derivation must not be order-blind: an order-blind
    // digest would also collapse two legitimate periods that happen to
    // mirror each other around a shared instant.
    assert_eq!(
        derive_usage_record_id(tenant(), &gts(), &key("idem-1"), we(), ws()),
        expect("6a75ef43-2619-58a0-b75c-53b08bd96a78"),
    );
}

#[test]
fn a_point_event_derives_over_equal_bounds() {
    // ADR-0007 confirmation: a point event is a zero-length period, and the
    // derivation needs no separate case for it.
    assert_eq!(
        derive_usage_record_id(tenant(), &gts(), &key("idem-1"), ws(), ws()),
        expect("64902762-6487-570c-83c3-8975a2e1adb4"),
    );
}

#[test]
fn equivalent_spellings_of_one_instant_derive_one_id() {
    // ADR-0007 confirmation: the canonical form collapses UUID case, a
    // non-UTC offset, and a fraction of zero / three / six digits onto one
    // pre-image. All three pairs must derive the base vector.
    let base = expect("5b075acb-e2e8-55a8-aedf-4c7d01b60284");

    let upper_tenant = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
        .unwrap();
    assert_eq!(
        derive_usage_record_id(upper_tenant, &gts(), &key("idem-1"), ws(), we()),
        base,
        "the tenant enters in its lowercase hyphenated form",
    );

    let plus_five_thirty = UtcOffset::from_hms(5, 30, 0).unwrap();
    assert_eq!(
        derive_usage_record_id(
            tenant(),
            &gts(),
            &key("idem-1"),
            ws().to_offset(plus_five_thirty),
            we().to_offset(plus_five_thirty),
        ),
        base,
        "a non-UTC offset normalizes before the bound is formatted",
    );

    assert_eq!(
        derive_usage_record_id(
            tenant(),
            &gts(),
            &key("idem-1"),
            ws().replace_nanosecond(0).unwrap(),
            we().replace_nanosecond(0).unwrap(),
        ),
        base,
        "a zero fraction pads to six digits",
    );
}

// ── canonical_period_bound: the 27-character form ───────────────────────────

#[test]
fn canonical_period_bound_is_twenty_seven_characters() {
    let rendered = canonical_period_bound(ws());
    assert_eq!(rendered, "2023-11-14T22:13:20.000000Z");
    assert_eq!(rendered.len(), 27);
}

#[test]
fn canonical_period_bound_always_carries_six_fraction_digits() {
    let with_micros = ws() + time::Duration::microseconds(1);
    assert_eq!(
        canonical_period_bound(with_micros),
        "2023-11-14T22:13:20.000001Z"
    );
    assert_eq!(
        canonical_period_bound(ws() + time::Duration::milliseconds(500)),
        "2023-11-14T22:13:20.500000Z"
    );
}

#[test]
fn canonical_period_bound_normalizes_a_non_utc_offset() {
    let plus_one = UtcOffset::from_hms(1, 0, 0).unwrap();
    assert_eq!(
        canonical_period_bound(ws().to_offset(plus_one)),
        canonical_period_bound(ws()),
    );
}
```

Then delete these tests, whose question no longer exists (record the verdict in your report):

- `sub_microsecond_created_at_truncates_to_same_id` — the gear now **rejects** sub-microsecond input instead of truncating it, so a test asserting that two sub-microsecond-apart instants derive one id asserts the opposite of the contract. Its replacement is `try_into_usage_record_rejects_a_sub_microsecond_bound` in Step 5.
- `created_at_micros_projects_whole_seconds_to_the_microsecond_count`, `created_at_micros_floors_sub_microsecond_nanos`, `created_at_micros_is_exact_for_pre_epoch_instants`, `created_at_micros_is_offset_invariant` — the function they cover is deleted; the canonical form replaces the integer-microsecond projection. Offset invariance survives as `canonical_period_bound_normalizes_a_non_utc_offset`.
- `separator_in_key_does_not_alias` — see Step 4: a key carrying a control character is now rejected at construction, so the aliasing vector is unconstructible. Its replacement is a rejection test in `models_tests.rs`.

- [ ] **Step 3: Run the tests to verify they fail**

```bash
cargo nextest run -p cf-gears-usage-collector-sdk id
```

Expected: compilation failure — `canonical_period_bound` does not exist and `derive_usage_record_id` takes four arguments.

- [ ] **Step 4: Rewrite the derivation**

Replace the body of `usage-collector-sdk/src/id.rs`. The module doc must state ADR-**0007** (the current identity ADR — the existing doc says ADR-0014, which is now the *selection* ADR and points at the wrong document).

```rust
//! Deterministic derivation of the ledger-entry identity.
//!
//! The entry `id` is not an independent field: it is a deterministic
//! projection of the dedup identity
//! `(tenant_id, gts_type_id, idempotency_key, window_start, window_end)`
//! (`cpt-cf-usage-collector-adr-record-identity-derivation`). The Ingestion
//! Gateway derives it at one choke point for every surface, and an emitter
//! reproduces the same value offline — which is what lets a correction name
//! its target before submission, with no round-trip.
//!
//! The identity and the dedup identity read the same five inputs, so the two
//! can never disagree about what one entry is. `invalidates` is deliberately
//! excluded: admitting it would let one idempotency key stand for both a
//! measurement and its withdrawal, so an emitter defect that reused a key
//! would produce both entries silently instead of surfacing a conflict.

use time::{OffsetDateTime, UtcOffset};
use uuid::Uuid;

use crate::models::{IdempotencyKey, MeterTypeId};

/// Fixed namespace for the entry-identity derivation (`UUIDv5`).
///
/// NEVER change this value: it admits no rotation, no per-deployment value
/// and no versioned variant, because each of those re-maps every identifier
/// the gear has ever issued.
pub const USAGE_RECORD_ID_NAMESPACE: Uuid =
    Uuid::from_u128(0x5631_3026_863b_4de8_b32b_1f96_b673_06ed);

/// ASCII unit separator between the dedup-identity fields.
///
/// The concatenation stays injective only while no input carries this byte.
/// Three inputs cannot carry it by construction: the tenant is a UUID and
/// both bounds are fixed-width timestamps. The other two are
/// caller-supplied, and both newtypes reject every ASCII control character
/// ([`MeterTypeId::new`], [`IdempotencyKey::new`]) — which is what keeps two
/// distinct dedup identities from concatenating to one pre-image.
const FIELD_SEPARATOR: u8 = 0x1F;

/// Renders a covered-period bound in the canonical 27-character form
/// `YYYY-MM-DDTHH:MM:SS.ffffffZ`.
///
/// The form is frozen with the namespace constant: a change to the fraction
/// width, the case of `T` / `Z`, or the text encoding re-maps every
/// identifier the gear has issued. Six digits is the microsecond, and it is
/// the precision ceiling of the derivation — a caller that sends
/// `12:00:00Z`, `12:00:00.000Z` or `13:00:00+01:00` reaches one canonical
/// form here.
///
/// This function **truncates** anything below the microsecond, and callers
/// MUST NOT hand it a finer value: the ingestion path rejects one before the
/// derivation runs ([`crate::CreateUsageRecord::try_into_usage_record`]), so
/// inside the gear the two can never disagree. Truncating an unvalidated
/// bound would make a read-back entry derive an identifier different from
/// the one it carries.
#[must_use]
pub fn canonical_period_bound(bound: OffsetDateTime) -> String {
    let utc = bound.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
        utc.microsecond(),
    )
}

/// Derives the entry identity from the 5-tuple dedup identity:
/// `id = UUIDv5(NS, tenant_id ⟨0x1F⟩ gts_type_id ⟨0x1F⟩ idempotency_key ⟨0x1F⟩ window_start ⟨0x1F⟩ window_end)`.
///
/// `tenant_id` enters in its lowercase hyphenated 36-character form,
/// `gts_type_id` and `idempotency_key` byte-exact (terminator `~`
/// included), and both bounds in the canonical form
/// [`canonical_period_bound`] renders. `NS` enters as its 16 raw bytes, per
/// RFC 4122.
///
/// A point event derives over a zero-length period, where the two bounds
/// are equal; the derivation needs no separate case for it.
#[must_use]
pub fn derive_usage_record_id(
    tenant_id: Uuid,
    gts_type_id: &MeterTypeId,
    idempotency_key: &IdempotencyKey,
    window_start: OffsetDateTime,
    window_end: OffsetDateTime,
) -> Uuid {
    let mut input = Vec::new();
    input.extend_from_slice(tenant_id.to_string().as_bytes());
    input.push(FIELD_SEPARATOR);
    input.extend_from_slice(gts_type_id.as_str().as_bytes());
    input.push(FIELD_SEPARATOR);
    input.extend_from_slice(idempotency_key.as_str().as_bytes());
    input.push(FIELD_SEPARATOR);
    input.extend_from_slice(canonical_period_bound(window_start).as_bytes());
    input.push(FIELD_SEPARATOR);
    input.extend_from_slice(canonical_period_bound(window_end).as_bytes());
    Uuid::new_v5(&USAGE_RECORD_ID_NAMESPACE, &input)
}
```

Delete `created_at_micros` entirely, and remove it from `lib.rs`'s `pub use id::{…}` list (add `canonical_period_bound` there).

**Tighten `IdempotencyKey::new`** in `models.rs`. ADR-0007's Decision Outcome makes a control-character-free key a *precondition of the derivation*, and its Confirmation list requires a test that a `0x1F` key is rejected before the derivation runs. Today the newtype rejects only NUL, and the wire schema (`usage-collector-v1.yaml`, `IdempotencyKey`) declares `pattern: '^[^\x00-\x1F\x7F]+$'` with `maxLength: 256`. Bring the newtype up to the wire contract:

```rust
/// Ceiling on the wire length of an idempotency key, from the
/// `IdempotencyKey` schema in `docs/usage-collector-v1.yaml`.
const MAX_IDEMPOTENCY_KEY_LEN: usize = 256;
```

and in `new`, after the empty check, replace the NUL check with:

```rust
        if raw.len() > MAX_IDEMPOTENCY_KEY_LEN {
            return Err(UsageCollectorError::invalid_idempotency_key(
                "idempotency_key must be at most 256 bytes",
            ));
        }
        // `char::is_ascii_control()` covers U+007F (DEL) alongside
        // U+0000..=U+001F. The exclusion is load-bearing rather than
        // cosmetic: the entry-identity derivation concatenates this value
        // with the other dedup-identity inputs under a `0x1F` separator
        // (ADR-0007), and the key is no longer the final field, so a
        // control character inside it would inject a separator into the
        // middle of the pre-image.
        if raw.chars().any(|c| c.is_ascii_control()) {
            return Err(UsageCollectorError::invalid_idempotency_key(
                "idempotency_key must not contain ASCII control characters",
            ));
        }
```

Update the newtype's `# Validation` doc block to match, and delete the claim that the plugin dedups on `(tenant_id, gts_type_id, idempotency_key)` — the dedup identity is the 5-tuple.

- [ ] **Step 5: Write the failing model tests**

In `usage-collector-sdk/src/models_tests.rs`, replace the `created_at` fixtures with the two bounds and add these tests (adapt to the file's existing fixture helpers rather than inventing new ones):

```rust
#[test]
fn try_into_usage_record_derives_the_id_over_the_five_tuple() {
    let submission = sample_create_record();
    let expected = derive_usage_record_id(
        submission.tenant_id,
        &submission.gts_type_id,
        &submission.idempotency_key,
        submission.window_start,
        submission.window_end,
    );
    let record = submission.try_into_usage_record().expect("valid period");
    assert_eq!(record.id, expected);
}

#[test]
fn try_into_usage_record_rejects_a_sub_microsecond_window_start() {
    // ADR-0007: a bound finer than the microsecond is REJECTED, not
    // truncated. Truncating would persist a period whose read-back derives
    // an id different from the one the entry carries, which breaks offline
    // reproduction at the point an emitter needs it.
    let mut submission = sample_create_record();
    submission.window_start = submission.window_start.replace_nanosecond(500).unwrap();
    let err = submission
        .try_into_usage_record()
        .expect_err("sub-microsecond bound must be rejected");
    let UsageCollectorError::InvalidArgument { field, .. } = err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert_eq!(field, "window_start");
}

#[test]
fn try_into_usage_record_rejects_a_sub_microsecond_window_end() {
    let mut submission = sample_create_record();
    submission.window_end = submission.window_end.replace_nanosecond(1).unwrap();
    let err = submission
        .try_into_usage_record()
        .expect_err("sub-microsecond bound must be rejected");
    let UsageCollectorError::InvalidArgument { field, .. } = err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert_eq!(field, "window_end");
}

#[test]
fn try_into_usage_record_accepts_equal_bounds_as_a_point_event() {
    let mut submission = sample_create_record();
    submission.window_end = submission.window_start;
    let record = submission.try_into_usage_record().expect("point event is valid input");
    assert_eq!(record.window_start, record.window_end);
}

#[test]
fn try_into_usage_record_rejects_an_inverted_covered_period() {
    let mut submission = sample_create_record();
    submission.window_end = submission.window_start - time::Duration::seconds(1);
    let err = submission
        .try_into_usage_record()
        .expect_err("window_end < window_start must be rejected");
    let UsageCollectorError::InvalidArgument { field, .. } = err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert_eq!(field, "window_end");
}

#[test]
fn try_into_usage_record_normalizes_both_bounds_to_utc() {
    let offset = time::UtcOffset::from_hms(-7, 0, 0).unwrap();
    let mut submission = sample_create_record();
    let instant = submission.window_start;
    submission.window_start = instant.to_offset(offset);
    submission.window_end = submission.window_end.to_offset(offset);
    let record = submission.try_into_usage_record().expect("valid period");
    assert_eq!(record.window_start.offset(), time::UtcOffset::UTC);
    assert_eq!(record.window_start, instant);
}

#[test]
fn one_instant_in_two_offsets_derives_one_id() {
    // The retry invariant: an emitter that resends the same period in a
    // different offset must not surface a false IdempotencyConflict.
    let offset = time::UtcOffset::from_hms(2, 0, 0).unwrap();
    let utc = sample_create_record().try_into_usage_record().expect("valid");
    let mut shifted = sample_create_record();
    shifted.window_start = shifted.window_start.to_offset(offset);
    shifted.window_end = shifted.window_end.to_offset(offset);
    assert_eq!(utc.id, shifted.try_into_usage_record().expect("valid").id);
}

#[test]
fn idempotency_key_rejects_a_unit_separator() {
    // ADR-0007 confirmation: the derivation concatenates the key under a
    // 0x1F separator and the key is not the final field, so a key carrying
    // that byte would inject a separator mid-pre-image. Rejected at
    // construction, before any derivation can run.
    IdempotencyKey::new("idem\u{1f}1").expect_err("0x1F in a key must be rejected");
    IdempotencyKey::new("idem\u{7f}1").expect_err("DEL in a key must be rejected");
    IdempotencyKey::new("a".repeat(257)).expect_err("over-long key must be rejected");
    IdempotencyKey::new("a".repeat(256)).expect("256 bytes is the ceiling, not past it");
}
```

- [ ] **Step 6: Change the record models**

In `usage-collector-sdk/src/models.rs`:

1. `UsageRecord`: replace `created_at` with two fields, both `#[serde(with = "time::serde::rfc3339")]`:
   ```rust
       /// Inclusive start of the emitter-supplied covered period (RFC 3339
       /// on the wire). The covered period is the only emitter-supplied time
       /// attribution an entry carries. Persisted at microsecond precision,
       /// UTC-normalized by
       /// [`CreateUsageRecord::try_into_usage_record`].
       #[serde(with = "time::serde::rfc3339")]
       pub window_start: time::OffsetDateTime,
       /// Exclusive end of the covered period. At or after
       /// [`Self::window_start`]; equal bounds mark a point event, not an
       /// error. **This is the bound every read path selects on** —
       /// `from <= window_end < to`, whatever the length of the period
       /// (`cpt-cf-usage-collector-adr-window-end-selection`) — and the raw
       /// path's keyset order is `(window_end, id)`.
       #[serde(with = "time::serde::rfc3339")]
       pub window_end: time::OffsetDateTime,
   ```
2. `CreateUsageRecord`: the same two fields, documented as caller-supplied and part of the dedup identity.
3. Rewrite the `UsageRecord::id` doc to name the 5-tuple and ADR-0007 (it currently says "4-tuple … ADR-0014").
4. Replace `into_usage_record` with:

```rust
    /// Projects this submission into the persisted [`UsageRecord`] shape,
    /// validating the covered period first.
    ///
    /// This is the single point at which a submission acquires its identity,
    /// and the validation is inseparable from it: ADR-0007 requires both
    /// period preconditions to be rejected **before** the derivation runs,
    /// so the projection is fallible rather than the caller's obligation.
    ///
    /// In order: each bound must carry at most microsecond precision, both
    /// are normalized to UTC, the period must be ordered
    /// (`window_start <= window_end`, equal bounds being a point event), and
    /// only then is `id` derived over
    /// `(tenant_id, gts_type_id, idempotency_key, window_start, window_end)`.
    /// `status` is initialized to [`UsageRecordStatus::Active`] and every
    /// other field is forwarded verbatim.
    ///
    /// Neither precondition truncates. A truncated bound would be persisted
    /// under an `id` derived from the truncated value while the emitter
    /// reproduces the id it submitted, so the two would disagree about the
    /// same entry.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorError::InvalidArgument`] when a bound is
    /// finer than microsecond precision, or when the period is inverted.
    pub fn try_into_usage_record(self) -> Result<UsageRecord, UsageCollectorError> {
        require_microsecond_precision("window_start", self.window_start)?;
        require_microsecond_precision("window_end", self.window_end)?;

        let window_start = self.window_start.to_offset(time::UtcOffset::UTC);
        let window_end = self.window_end.to_offset(time::UtcOffset::UTC);

        if window_end < window_start {
            return Err(UsageCollectorError::inverted_covered_period(
                window_start,
                window_end,
            ));
        }

        let id = crate::id::derive_usage_record_id(
            self.tenant_id,
            &self.gts_type_id,
            &self.idempotency_key,
            window_start,
            window_end,
        );

        Ok(UsageRecord {
            id,
            gts_type_id: self.gts_type_id,
            tenant_id: self.tenant_id,
            resource_ref: self.resource_ref,
            subject_ref: self.subject_ref,
            metadata: self.metadata,
            value: self.value,
            idempotency_key: self.idempotency_key,
            corrects_id: self.corrects_id,
            status: UsageRecordStatus::Active,
            window_start,
            window_end,
        })
    }
```

with the helper next to it:

```rust
/// Rejects a covered-period bound carrying finer than microsecond
/// precision.
///
/// The microsecond is the precision ceiling of the identity derivation
/// (ADR-0007's canonical bound form is a fixed six-digit fraction), so a
/// finer value has no representation there. A leap second arrives here as
/// the same failure: `time`'s RFC 3339 parser renders a `:60` second at a
/// valid stand-in position as `59.999999999`, and `time::Time` cannot
/// represent second 60 at all, so this one check is where ADR-0007's
/// leap-second rejection lands.
fn require_microsecond_precision(
    field: &'static str,
    bound: time::OffsetDateTime,
) -> Result<(), UsageCollectorError> {
    if bound.nanosecond() % 1_000 == 0 {
        Ok(())
    } else {
        Err(UsageCollectorError::sub_microsecond_period_bound(
            field, bound,
        ))
    }
}
```

Leave `UsageRecordQuery` (the filterable-field schema) and `is_keyset_safe_record_field` alone — task 4 owns the query surface, and dragging it in here would put the read path in a half-migrated state across two commits instead of one.

- [ ] **Step 7: Add the two error constructors**

In `usage-collector-sdk/src/error.rs`:

```rust
    /// A covered-period bound carried finer than microsecond precision.
    #[must_use]
    pub fn sub_microsecond_period_bound(field: &str, bound: OffsetDateTime) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: field.to_owned(),
            reason: ValidationReason::Validation,
            detail: format!(
                "covered-period bound `{field}` carries finer than microsecond \
                 precision ({} ns); the entry identity derivation reads a \
                 fixed-width microsecond form, so a finer value is rejected \
                 rather than truncated",
                bound.nanosecond()
            ),
        }
    }

    /// The covered period was inverted (`window_end < window_start`).
    #[must_use]
    pub fn inverted_covered_period(
        window_start: OffsetDateTime,
        window_end: OffsetDateTime,
    ) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "window_end".to_owned(),
            reason: ValidationReason::Validation,
            detail: format!(
                "covered period requires window_start <= window_end (got \
                 window_start={window_start}, window_end={window_end}); equal \
                 bounds are a point event and are valid"
            ),
        }
    }
```

- [ ] **Step 8: Thread the fallible projection through the service**

`usage-collector/src/domain/service.rs`, single path (`create_usage_record_inner`, around line 674): the projection now returns a `Result`.

```rust
        // The service is the guaranteed choke point for every caller (REST +
        // in-process). The create surface is identity-free
        // (`CreateUsageRecord`); the entry acquires its deterministic
        // dedup-identity-derived `id` HERE, and only after its covered
        // period has been validated — ADR-0007 requires both period
        // preconditions to be rejected before the derivation runs.
        let record = record.try_into_usage_record()?;
```

Batch path (`create_usage_records_inner`, around line 981): the current code maps every submission infallibly and then relies on `records` being index-aligned. Replace the map with a conversion loop that routes a rejection to its own input index, and carry the index explicitly from there on. **Verify this against the real function before editing** — it is ~200 lines and the sketch names only the five touch points.

```rust
        let submission_count = records.len();
        let mut results: Vec<Option<Result<UsageRecord, UsageCollectorError>>> =
            (0..submission_count).map(|_| None).collect();

        // Identity derivation is per-submission and fallible (ADR-0007's
        // period preconditions), so a bad period surfaces at its own input
        // index instead of failing the batch. Every later pass carries the
        // input index explicitly rather than re-`enumerate()`ing, because
        // the surviving vector is no longer index-aligned with the input.
        let mut derived: Vec<(usize, UsageRecord)> = Vec::with_capacity(submission_count);
        for (index, submission) in records.into_iter().enumerate() {
            match submission.try_into_usage_record() {
                Ok(record) => derived.push((index, record)),
                Err(e) => results[index] = Some(Err(e)),
            }
        }
```

Then, in order down the function:

- the attribution-tuple grouping loop becomes `for (index, record) in &derived { … .push(*index); }`;
- `pdp_allowed` stays `vec![true; submission_count]`;
- `distinct_gts_type_ids` becomes `derived.iter().filter(|(idx, _)| pdp_allowed[*idx]).map(|(_, r)| r.gts_type_id.clone()).collect()`;
- the main validation loop becomes `for (index, record) in derived { if !pdp_allowed[index] { continue; } … }`;
- the existing `results` initialization further down is deleted (it moved above the conversion loop).

The tail's "every per-record slot is filled" guard already covers the new rejection path — a converted-and-rejected slot is `Some(Err(..))`. Leave it as is.

Also update the two doc comments that describe the old behaviour: `create_usage_records`' "eligible records carry their caller-supplied `created_at` through to persistence, where `into_usage_record` truncates it to microsecond precision (ADR-0014)" is now wrong twice over (no truncation, wrong ADR), and the `inst-emit-batch-record-eligible` comment says the same thing.

- [ ] **Step 9: Change the REST create surface**

`usage-collector/src/api/rest/dto.rs`:
- `CreateUsageRecordRequest`: `created_at` → `window_start` + `window_end`, both `#[serde(with = "time::serde::rfc3339")]`. Update the `idempotency_key` doc, which names the 4-tuple and ADR-0014.
- `UsageRecordDto`: the same two fields; update the struct doc (it says "`created_at` is emitted as RFC 3339").
- `From<UsageRecord> for UsageRecordDto`: map both bounds.

`usage-collector/src/api/rest/handlers/usage_records.rs`, `record_request_into_domain` (around line 754): replace `created_at: req.created_at` with the two bounds.

- [ ] **Step 10: Update every remaining `created_at` site**

```bash
cd gears/system/usage-collector
grep -rn "created_at" --include='*.rs' usage-collector usage-collector-sdk plugins/noop-usage-collector-plugin
```

Every hit is either a record fixture (give it a period — `window_start: OffsetDateTime::UNIX_EPOCH, window_end: OffsetDateTime::UNIX_EPOCH + Duration::hours(1)` keeps existing intent), a read-path `$filter` string (leave those alone; task 3 deletes them), or prose. Two hits need thought rather than mechanical replacement:

- `usage-collector/src/domain/authz_tests.rs:153` — the test asserting the PDP attribution tuple ignores fields outside it. It currently varies `created_at`; repoint it to vary `window_start` **and** `window_end`, so it still proves the tuple key is blind to the covered period.
- `usage-collector/src/domain/service_tests.rs` around lines 2621-2730 — the id-derivation tests naming the 4-tuple in their assertion messages. Repoint them to the 5-tuple and to `try_into_usage_record`.

Add one end-to-end wire test in `usage-collector/src/api/rest/handlers/usage_records_tests.rs`:

```rust
#[tokio::test]
async fn a_leap_second_period_bound_is_rejected() {
    // `time`'s RFC 3339 parser renders 23:59:60 at a valid stand-in
    // position as 23:59:59.999999999, so a leap second reaches the gear as
    // a sub-microsecond bound and the precision precondition rejects it
    // (ADR-0007: "A second value of 60 is rejected on the same path, so no
    // leap second enters the derivation"). `time::Time` cannot represent
    // second 60 at all, so this is the only place the rule can be observed.
    // A `:60` anywhere else fails at the parser instead.
}
```

Implement it against the existing batch-create handler tests' shape: post one record whose `window_end` is `"2016-12-31T23:59:60Z"`, assert the per-record outcome is `Rejected`, and assert the field violation names `window_end`. Add a sibling asserting `"2026-05-29T12:00:60Z"` (not a valid stand-in) is rejected at deserialization.

- [ ] **Step 11: Run the tests to verify they pass**

```bash
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk -p cf-gears-noop-usage-collector-plugin
```

Expected: all pass. Report the new total against the 485 baseline and account for the difference.

- [ ] **Step 12: Falsify**

Three separate falsifications, each restored before the next:

1. Make `require_microsecond_precision` truncate instead of rejecting (return `Ok(())` unconditionally and round the bound in `try_into_usage_record`). Confirm `try_into_usage_record_rejects_a_sub_microsecond_window_start` fails. This is the branch the whole task exists to protect.
2. Swap the order of `idempotency_key` and `window_start` in the concatenation. Confirm `derive_matches_golden_vector` fails — proving the golden vector actually pins the field order, not merely the field set.
3. Drop the `to_offset(UtcOffset::UTC)` normalization in `canonical_period_bound`. Confirm `equivalent_spellings_of_one_instant_derive_one_id` fails.

Each time: restore with `cp`, `touch` the file, confirm a `Compiling` line, re-run.

- [ ] **Step 13: Verification bar and commit**

```bash
git commit -s -m "feat(usage-collector)!: carry the covered period and derive identity over it" -m "$(cat <<'BODY'
An entry measures a period, not an instant. created_at becomes
[window_start, window_end) on both the persisted and the ingestion shape,
and the identity derivation moves to ADR-0007's 5-tuple: tenant, type,
key, and both bounds, in that order, under the 0x1F separator, with each
bound in the fixed 27-character YYYY-MM-DDTHH:MM:SS.ffffffZ form. The
namespace constant is unchanged. Golden vectors were recomputed
independently of this crate and cross-checked against the code.

The two period preconditions are validation errors rather than
truncation, which is why the projection into the persisted shape is now
fallible: a truncated bound would be persisted under an id derived from
the truncated value while the emitter reproduces the id it submitted, so
the two would disagree about the same entry. A leap second arrives as the
same failure - time's RFC 3339 parser renders 23:59:60 as
59.999999999 and time::Time cannot hold second 60 - so the precision
check is where ADR-0007's leap-second rule lands, and no separate branch
exists.

IdempotencyKey::new now rejects every ASCII control character and caps
at the wire schema's 256 bytes. The key is no longer the final field of
the pre-image, so a control character inside it would inject a separator
mid-concatenation; ADR-0007 carries the exclusion as a precondition of
the derivation and its confirmation list requires the test.

The read path still carries its window as a $filter conjunct; the typed
range replaces it in the next commit.

BREAKING CHANGE: UsageRecord and CreateUsageRecord replace created_at
with window_start and window_end, CreateUsageRecord::into_usage_record
becomes the fallible try_into_usage_record, created_at_micros is gone,
and every entry id changes. The gear is pre-release with no
installations, so no migration path is provided.
BODY
)"
```

---

## Task 3: the time range as a typed parameter on both read paths

**Files:**
- Modify: `usage-collector-sdk/src/api.rs`, `usage-collector-sdk/src/plugin_api.rs`
- Modify: `usage-collector-sdk/src/error.rs`, `usage-collector-sdk/src/reason.rs`, `usage-collector-sdk/src/reason_tests.rs`
- Modify: `usage-collector/src/domain/query.rs`, `usage-collector/src/domain/query_tests.rs`
- Modify: `usage-collector/src/domain/service.rs`, `.../service_tests.rs`, `.../service_metrics_tests.rs`
- Modify: `usage-collector/src/domain/local_client.rs`, `.../local_client_tests.rs`
- Modify: `usage-collector/src/domain/test_support.rs`
- Modify: `plugins/noop-usage-collector-plugin/src/plugin.rs`
- Modify: `usage-collector/src/api/rest/dto.rs`, `.../dto_tests.rs`
- Modify: `usage-collector/src/api/rest/handlers/usage_records.rs`, `.../usage_records_tests.rs`
- Modify: `usage-collector/src/api/rest/routes/usage_records.rs`

One commit: changing the SPI signature forces the doubles, the noop plugin, the service and the REST handlers in the same compile unit.

Where the range travels:

| Surface | Carrier |
| --- | --- |
| `GET /usage-collector/v1/records` | mandatory `from` / `to` query parameters (`RangeFrom` / `RangeTo` in the contract) |
| `POST /usage-collector/v1/records/aggregate` | mandatory `time_range: {from, to}` in the request **body** (`AggregationRequest.time_range`) |
| SDK trait, Plugin SPI, `Service` | `time_range: TimeRange`, by value (it is `Copy`) |

A posture note to carry into the doc comments: the old `require_bounded_time_window` ran **after** the PDP call, so an unauthorized caller was denied regardless of window shape. A malformed range is now rejected at the edge, when the typed parameter is parsed — the same place and the same order as a malformed `gts_type_id`, which has always been parsed pre-PDP. Do not claim post-authz ordering anywhere.

- [ ] **Step 1: Write the failing service tests**

In `usage-collector/src/domain/service_tests.rs`:

```rust
#[tokio::test]
async fn list_forwards_the_typed_time_range_to_the_plugin() {
    // DESIGN §3.3 rule 5: the time range is a typed parameter and never a
    // $filter conjunct. The assertion is on what the SPI was handed, not on
    // the call succeeding — a range dropped between the gateway and the
    // plugin is an unbounded scan that still returns 200.
    let plugin = HappyPathPlugin::new();
    plugin.set_list_usage_records_response(ODataPage::empty(0));
    let (svc, _handle) = ServiceFixture::new()
        .with_source(declaring_source())
        .build_with_default_resolver_handle(Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>)
        .await;

    let range = TimeRange::new(at(1_700_000_000), at(1_700_003_600)).expect("range");
    svc.list_usage_records(&authenticated_ctx(), meter_type_id(), range, &ODataQuery::default(), &[])
        .await
        .expect("list succeeds");

    assert_eq!(plugin.last_list_time_range(), Some(range));
}

#[tokio::test]
async fn aggregate_forwards_the_typed_time_range_to_the_plugin() {
    // …mirror of the above against query_aggregated_usage_records and
    // last_aggregate_time_range().
}

#[tokio::test]
async fn list_no_longer_requires_a_time_window_inside_the_filter() {
    // The window is a typed parameter, so an empty $filter is a complete
    // request. Before this slice the same call was a 400 MISSING_TIME_WINDOW.
    let plugin = HappyPathPlugin::new();
    plugin.set_list_usage_records_response(ODataPage::empty(0));
    // …build, call with ODataQuery::default(), assert Ok.
}
```

Adapt the fixture calls to the real helpers in that file (`ServiceFixture`, `declaring_source()` or its local equivalent, `authenticated_ctx()`). Do **not** add a new `service_with_*` helper.

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo nextest run -p cf-gears-usage-collector list_forwards_the_typed_time_range
```

Expected: compilation failure — `list_usage_records` takes four arguments and `last_list_time_range` does not exist.

- [ ] **Step 3: Change the two trait surfaces**

`usage-collector-sdk/src/api.rs` — both read methods gain `time_range: TimeRange` immediately after `gts_type_id`, matching DESIGN §3.3:

```rust
    /// Aggregated query over one meter and one range.
    ///
    /// Carries no aggregation parameter: the fold is resolved from the
    /// queried type's declaration. An entry is selected when the end of its
    /// covered period falls in `time_range` — `from <= window_end < to` —
    /// whatever the length of that period. A withdrawn record and its
    /// invalidation each contribute nothing.
    async fn query_aggregated_usage_records(
        &self,
        ctx: &SecurityContext,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorError>;

    /// Keyset-paginated ledger read over `(window_end, id)`.
    async fn list_usage_records(
        &self,
        ctx: &SecurityContext,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorError>;
```

`usage-collector-sdk/src/plugin_api.rs` — the same two insertions. Replace the paragraph on `query_aggregated_usage_records` that says "The time window is expressed inside `query.filter` as a `created_at ge … and created_at lt …` predicate; there is no separate typed parameter" — it now states the opposite of the truth. Put the selection obligation on the plugin instead:

```rust
    /// Compute the given fold over the authorized scope and `time_range`.
    ///
    /// The fold arrives as a parameter: declarations never reach the SPI, so
    /// the plugin stays pure persistence and never resolves a type itself.
    ///
    /// `time_range` is a typed parameter and never appears in
    /// `query.filter`. The plugin MUST select an entry when
    /// `from <= window_end < to`
    /// (`cpt-cf-usage-collector-adr-window-end-selection`): the predicate
    /// reads the period end alone, needs no case for a point event
    /// (`window_start == window_end`), and MUST NOT match by overlap or by
    /// containment — those make adjacent ranges double count or drop
    /// entries. No selection predicate reads `window_start`.
```

and on `list_usage_records`, replace the `(created_at asc, id asc)` default claim:

```rust
    /// `query.order` is guaranteed non-empty (the gateway appends the
    /// canonical unique `(window_end, id)` suffix in the caller's sort
    /// direction), so plugins MUST honour it for stable pagination. The
    /// filter column and the page order are the same column, so one index
    /// serves both.
```

- [ ] **Step 4: Delete the retired window guard**

`usage-collector/src/domain/query.rs`: delete `require_bounded_time_window`, the `CREATED_AT_FIELD` constant and `visit_top_level_conjuncts` (its only caller). Update the module doc, which lists `require_bounded_time_window` as one of the helpers the module holds.

`usage-collector-sdk/src/error.rs`: delete `missing_time_window`.

`usage-collector-sdk/src/reason.rs`: delete the `MISSING_TIME_WINDOW` constant, the `ValidationReason::MissingTimeWindow` variant, and both match arms. Delete its row from the `reason_tests.rs` round-trip table. `ValidationReason` is `#[non_exhaustive]`, so removing a variant is source-breaking for a matcher on the variant but not a wildcard match — note it in the `BREAKING CHANGE` trailer.

Also fix `AGGREGATION_RESULT_TOO_LARGE`'s doc and `aggregation_result_too_large`'s detail string: both tell the caller to "narrow the `created_at` window", which now names nothing. "Narrow the time range" is the replacement.

`usage-collector/src/domain/query_tests.rs`: delete the `require_bounded_time_window` block — `assert_missing_window`, `query_with_filter` if it becomes unused, and every test whose subject is the window guard (`equality_alone_does_not_bound_the_window`, `negated_window_is_rejected`, and their siblings). Per-test verdicts in your report: each one pinned a rule about *where the window lives in `$filter`*, and the window no longer lives in `$filter` at all. Keep `the_window_bounds_are_not_filterable` — that rule survives and gets stronger in task 4.

- [ ] **Step 5: Thread the range through the service and the local client**

`usage-collector/src/domain/service.rs`, both read methods: add the parameter, delete the `require_bounded_time_window(query)?;` call and the comment above it, and pass `time_range` to the SPI call. Update the numbered step lists in both doc comments — step 4 of `list_usage_records` currently says "The `[from, to)` time window flows through `query.filter` as a `created_at` predicate … the gateway no longer accepts a separate `TimeWindow`", which inverts the new contract.

`usage-collector/src/domain/local_client.rs`: forward the parameter on both methods.

- [ ] **Step 6: Update the plugin implementations and doubles**

`plugins/noop-usage-collector-plugin/src/plugin.rs`: add `_time_range: TimeRange` to both read methods.

`usage-collector/src/domain/test_support.rs`:
- `MockPlugin`: add the parameter.
- `HappyPathPlugin`: add the parameter, plus two recorders so a test can assert what the SPI was handed:
  ```rust
      /// The `time_range` passed to the most-recent `list_usage_records`
      /// dispatch. The range is a typed parameter rather than a `$filter`
      /// conjunct, so nothing in the `ODataQuery` a test inspects would
      /// reveal a range the gateway dropped on the way to the SPI.
      list_time_range: Mutex<Option<TimeRange>>,
      aggregate_time_range: Mutex<Option<TimeRange>>,
  ```
  with `last_list_time_range()` / `last_aggregate_time_range()` accessors returning `Option<TimeRange>`. Follow the file's existing `Mutex` + `.lock().expect("mutex")` idiom.

- [ ] **Step 7: Change the REST read surface**

`usage-collector/src/api/rest/dto.rs` — add the wire projection and the aggregate body field:

```rust
/// Wire projection of [`usage_collector_sdk::TimeRange`] — the mandatory
/// bounded range on the aggregate path (`AggregationRequest.time_range` in
/// `docs/usage-collector-v1.yaml`). The raw path carries the same range as
/// the `from` / `to` query parameters instead, because a `GET` has no body.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct TimeRangeDto {
    /// Inclusive lower bound (RFC 3339, UTC).
    #[serde(with = "time::serde::rfc3339")]
    pub from: OffsetDateTime,
    /// Exclusive upper bound (RFC 3339, UTC).
    #[serde(with = "time::serde::rfc3339")]
    pub to: OffsetDateTime,
}

impl TryFrom<TimeRangeDto> for TimeRange {
    type Error = UsageCollectorError;

    fn try_from(value: TimeRangeDto) -> Result<Self, Self::Error> {
        TimeRange::new(value.from, value.to)
    }
}
```

`QueryAggregatedUsageRecordsRequest` gains a mandatory `pub time_range: TimeRangeDto` (no `#[serde(default)]` — the contract marks it required, and the DTO is `deny_unknown_fields`). Update the struct doc, which currently claims the body carries only the group-by dimensions.

`usage-collector/src/api/rest/handlers/usage_records.rs`:

1. Split the typed-parameter allowlist in two. `from` / `to` are list-path parameters only; leaving them in one shared constant would make the aggregate path silently accept and ignore them, which is the drift these allowlists exist to stop.
   ```rust
   /// Typed query parameters on the raw path that are NOT part of the
   /// `OData` surface. The covered-period range travels here rather than in
   /// `$filter` (DESIGN §3.3 rule 5).
   const TYPED_LIST_PARAMS: &[&str] = &["gts_type_id", "from", "to"];

   /// Typed query parameters on the aggregate path. The range is in the
   /// request body there (`AggregationRequest.time_range`), so `from` / `to`
   /// are not accepted in the query string — an accepted-and-ignored
   /// parameter is exactly the drift this allowlist exists to stop.
   const TYPED_AGGREGATE_PARAMS: &[&str] = &["gts_type_id"];
   ```
   `reject_unknown_aggregate_params` switches to `TYPED_AGGREGATE_PARAMS` (in the check *and* in its error message).
2. Add the parser, next to `parse_required_gts_type_id`:
   ```rust
   /// Extracts the mandatory `from` / `to` query parameters into a validated
   /// [`TimeRange`].
   ///
   /// Both are parsed as RFC 3339, which rejects an offset-less timestamp —
   /// `docs/usage-collector-v1.yaml`'s `Timestamp` requires an offset, and a
   /// bare local time would silently attribute usage to whatever offset the
   /// server happened to assume. A duplicate occurrence is rejected so
   /// last-wins ambiguity cannot mask a caller bug.
   fn parse_required_time_range(
       params: &[(String, String)],
   ) -> Result<TimeRange, CanonicalError> {
       let from = parse_range_bound(params, "from")?;
       let to = parse_range_bound(params, "to")?;
       TimeRange::new(from, to).map_err(usage_collector_error_to_canonical)
   }

   fn parse_range_bound(
       params: &[(String, String)],
       key: &'static str,
   ) -> Result<OffsetDateTime, CanonicalError> {
       let raw = require_single_value(params, key)?;
       OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339).map_err(|err| {
           UsageRecordResource::invalid_argument()
               .with_field_violation(
                   key,
                   format!("`{key}` must be an RFC 3339 UTC timestamp: {err}"),
                   "VALIDATION",
               )
               .create()
       })
   }
   ```
3. `PreparedListRequest` becomes `(MeterTypeId, TimeRange, Vec<MetadataFilter>, ODataQuery)` and `PreparedAggregateRequest` gains `TimeRange` in the same position; both `prepare_*_request` functions call the new parser (the aggregate one via `TimeRange::try_from(req.time_range)`), and both handlers pass it to the service.
4. Rewrite the two handler doc comments. `handle_list_usage_records` currently documents the window as a `$filter` predicate, `MISSING_TIME_WINDOW`, and a `(created_at, id)` keyset; `handle_query_aggregated_usage_records` says the `$filter` carries the window. Both must describe the typed parameter. Leave the keyset wording to task 4 if you prefer, but do not leave a claim that is false in both directions.

`usage-collector/src/api/rest/routes/usage_records.rs` — declare the two parameters on the GET route, next to `gts_id`:

```rust
        .query_param(
            "from",
            true,
            "Inclusive lower bound of the covered-period range (RFC 3339 UTC, mandatory)",
        )
        .query_param(
            "to",
            true,
            "Exclusive upper bound of the covered-period range (RFC 3339 UTC, mandatory)",
        )
```

The aggregate route needs no parameter change — the range is in its declared request body.

- [ ] **Step 8: Write the failing handler tests**

In `usage-collector/src/api/rest/handlers/usage_records_tests.rs`, add:

- `list_rejects_a_missing_from_parameter` / `..._to_parameter` — 400, field violation naming the parameter.
- `list_rejects_an_offsetless_from_parameter` — `"2026-01-01T00:00:00"` is a 400, not a silent UTC assumption.
- `list_rejects_an_inverted_range` — `from=2026-02-01…&to=2026-01-01…` is a 400 on `time_range`.
- `list_rejects_a_duplicate_from_parameter` — the `require_single_value` path.
- `aggregate_rejects_a_body_without_a_time_range` — the request body is required and `deny_unknown_fields`.
- `aggregate_rejects_a_from_query_parameter` — the new `TYPED_AGGREGATE_PARAMS` allowlist; the parameter must be named in the 400 rather than ignored.

Existing tests in that file that pass a bounded-window `$filter` keep working (the conjunct is now inert), but the mandatory `from` / `to` must be added to every list-path request or they will 400. Prefer a single local helper that builds the two parameters over editing each call site by hand.

- [ ] **Step 9: Run the tests to verify they pass**

```bash
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk -p cf-gears-noop-usage-collector-plugin
```

- [ ] **Step 10: Falsify**

1. In `Service::list_usage_records`, construct a throwaway `TimeRange` covering all time and pass *that* to the SPI instead of the caller's. Confirm `list_forwards_the_typed_time_range_to_the_plugin` fails. If it passes, the test is asserting the call succeeded rather than what the plugin received — fix the test.
2. Add `"from"` to `TYPED_AGGREGATE_PARAMS`. Confirm `aggregate_rejects_a_from_query_parameter` fails.

Restore with `cp` + `touch`, confirm a `Compiling` line, re-run.

- [ ] **Step 11: Verification bar and commit**

```bash
git commit -s -m "feat(usage-collector)!: carry the read-path time range as a typed parameter" -m "$(cat <<'BODY'
The mandatory range stopped being a $filter conjunct. DESIGN §3.3 rule 5
makes it a typed parameter on both read paths, so it reaches the SDK
trait, the Plugin SPI and the Service as a validated TimeRange: from and
to query parameters on GET /records, AggregationRequest.time_range in the
aggregate body.

That retires require_bounded_time_window and MISSING_TIME_WINDOW
outright rather than relocating them. The guard existed because a
one-sided or absent created_at predicate inside $filter would drive an
unbounded scan; a typed mandatory parameter cannot be one-sided or
absent, and TimeRange::new rejects an inverted or empty range at
construction. The rejection also moves earlier - to where the typed
parameter is parsed, alongside gts_type_id, which has always been parsed
before the PDP call.

Both bounds are parsed as RFC 3339, which rejects an offset-less
timestamp: a bare local time would attribute usage to whatever offset
the server happened to assume. from and to are accepted on the raw path
only, in their own allowlist, so the aggregate path names them in a 400
instead of accepting and ignoring them.

The plugin SPI doc now carries the selection obligation it was missing:
select when from <= window_end < to, no case for a point event, never by
overlap or containment, and no predicate reads window_start.

BREAKING CHANGE: UsageCollectorClientV1 and UsageCollectorPluginV1 both
take time_range: TimeRange on list_usage_records and
query_aggregated_usage_records. ValidationReason::MissingTimeWindow, the
MISSING_TIME_WINDOW wire code and UsageCollectorError::missing_time_window
are removed.
BODY
)"
```

---

## Task 4: period-end selection on the query surface

**Files:**
- Modify: `usage-collector-sdk/src/models.rs`, `.../models_tests.rs`
- Modify: `usage-collector/src/domain/query.rs`, `.../query_tests.rs`
- Modify: `usage-collector/src/api/rest/handlers/usage_records.rs`, `.../usage_records_tests.rs`

The range is typed now, but the filter/order surface still names `created_at`: it is a filterable field on a schema whose record no longer has it, it is the keyset-safe primary time key, and the gateway appends it as the canonical tiebreaker. All three move to `window_end`.

Why the covered-period fields are on the filterable schema at all, given that a `$filter` may not name them: the schema is also the plugin's field-to-column vocabulary and the `$orderby` vocabulary. `window_end` must be nameable for the keyset order and the cursor tokens to resolve to a column. `reject_reserved_filter_fields` is what keeps it out of `$filter` — and that guard already lists both bounds, precisely so this task could not open a hole.

- [ ] **Step 1: Write the failing tests**

In `usage-collector-sdk/src/models_tests.rs`:

```rust
#[test]
fn the_covered_period_end_is_keyset_safe() {
    // The raw path paginates over (window_end, id) and both are mandatory
    // on every entry, so both are sound leading keyset columns.
    assert!(is_keyset_safe_record_field("window_end"));
    assert!(is_keyset_safe_record_field("window_start"));
    assert!(is_keyset_safe_record_field("id"));
}

#[test]
fn created_at_is_no_longer_a_record_field() {
    // An entry carries a covered period, not a creation instant. A stale
    // order key must fail closed rather than resolve to something.
    assert!(!is_keyset_safe_record_field("created_at"));
}

#[test]
fn optional_attributes_stay_keyset_unsafe() {
    assert!(!is_keyset_safe_record_field("subject_id"));
    assert!(!is_keyset_safe_record_field("corrects_id"));
}
```

In `usage-collector/src/api/rest/handlers/usage_records_tests.rs` (adapting the existing `prepare_list_query` tests, which assert a `(created_at, id)` normalization):

```rust
#[test]
fn an_absent_orderby_normalizes_to_the_canonical_keyset() {
    // (window_end asc, id asc): the filter column and the page order are
    // the same column, so one index serves both.
}

#[test]
fn a_descending_caller_order_gains_the_suffix_in_its_own_direction() {
    // Row-value keyset comparison needs a uniform direction; pairing a
    // descending caller order with an ascending suffix cannot compose.
}

#[test]
fn an_order_on_the_covered_period_end_is_accepted() {
    // $orderby=window_end
}

#[test]
fn an_order_on_created_at_is_rejected() {
    // 400. Do not over-specify the message: created_at is off the
    // filterable schema, so the rejection may arrive from the OData layer
    // as an unknown field or from the keyset-safety guard, and either is a
    // correct fail-closed outcome.
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo nextest run -p cf-gears-usage-collector-sdk keyset
cargo nextest run -p cf-gears-usage-collector orderby
```

- [ ] **Step 3: Change the filterable-field schema**

In `usage-collector-sdk/src/models.rs`, in `UsageRecordQuery`, replace the `created_at` field with:

```rust
    /// `usage_records.window_start` — the inclusive start of the covered
    /// period. On the schema so a plugin has a column mapping and a caller
    /// can order by it; **not** filterable — the range travels as a typed
    /// parameter, and `reject_reserved_filter_fields` rejects any predicate
    /// naming it.
    #[odata(filter(kind = "DateTimeUtc"))]
    pub window_start: time::OffsetDateTime,
    /// `usage_records.window_end` — the exclusive end of the covered
    /// period, and the column every read path selects on
    /// (`from <= window_end < to`). It is also the primary key of the raw
    /// path's `(window_end, id)` keyset, so the filter column and the page
    /// order are one column. Reserved on the `$filter` surface for the same
    /// reason as `window_start`.
    #[odata(filter(kind = "DateTimeUtc"))]
    pub window_end: time::OffsetDateTime,
```

Update the file-level comment above the struct: the paragraph beginning "`created_at` and `id` ARE on the schema: the gateway treats the `[from, to)` time window as an ordinary `created_at ge …` predicate inside `$filter` (no separate `TimeWindow` typed parameter)" now says the opposite of the truth. Replace it with why the bounds are on the schema and off `$filter`.

Then `is_keyset_safe_record_field`:

```rust
    matches!(
        name,
        "id" | "window_start" | "window_end" | "tenant_id" | "resource_id" | "resource_type"
            | "status"
    )
```

and update its doc, which enumerates `created_at` among the mandatory attributes.

- [ ] **Step 4: Move the canonical keyset**

In `usage-collector/src/api/rest/handlers/usage_records.rs`:

```rust
/// The canonical unique keyset suffix appended to every raw-list order.
/// `window_end` is the primary time key — the same column the range selects
/// on, so one index serves both the filter and the page order — and `id` is
/// the globally-unique final tiebreaker.
const CANONICAL_TIEBREAKER_FIELDS: &[&str] = &["window_end", "id"];
```

Update every `(created_at, id)` mention in `prepare_list_query`'s inline comments and in the handler doc comment.

- [ ] **Step 5: Bring the reserved-field guard's comment up to date**

`usage-collector/src/domain/query.rs`'s `RESERVED_FILTER_FIELDS` doc says the covered-period bounds are "landing with the record-model slice after this plan — reserved here regardless", and `is_reserved_filter_field`'s doc says they "become real filterable fields in the record-model slice after this plan, at which point a case-varied spelling would otherwise resolve as a legitimate field". Both predictions came true in this commit. Rewrite them in the present tense: the bounds *are* filterable-schema fields now, which is exactly why the case-insensitive reservation is load-bearing rather than theoretical.

- [ ] **Step 6: Run the tests to verify they pass**

```bash
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk -p cf-gears-noop-usage-collector-plugin
```

- [ ] **Step 7: Falsify**

1. Add `"window_end"` back to a bare `contains`-style check by weakening `is_reserved_filter_field` to a case-sensitive comparison, then assert a `$filter` naming `WINDOW_END` is still rejected. Confirm the reserved-field test fails. Restore.
2. Revert `CANONICAL_TIEBREAKER_FIELDS` to `["created_at", "id"]` and confirm `an_absent_orderby_normalizes_to_the_canonical_keyset` fails.

- [ ] **Step 8: Verification bar and commit**

```bash
git commit -s -m "feat(usage-collector)!: select and paginate on the covered-period end" -m "$(cat <<'BODY'
The filter and order surface still named created_at: a filterable field
on a schema whose record no longer has one, the keyset-safe primary time
key, and the tiebreaker the gateway appends to every raw order. All three
move to the covered-period end.

Both bounds are on the filterable schema and neither is filterable. The
schema is also the plugin's field-to-column vocabulary and the $orderby
vocabulary, so window_end has to be nameable for the keyset order and
the cursor tokens to resolve to a column at all;
reject_reserved_filter_fields is what keeps a predicate off it, and that
guard already listed both bounds so this commit could not open a hole.
Its case-insensitive comparison stops being theoretical here: a
case-varied spelling would now resolve as a legitimate field rather than
dead-ending as an unknown one.

The page order is the column the range selects on, so one index serves
both, and LATEST already resolves by the greatest window_end.
window_start stays on the entry and in the identity derivation; no
selection predicate reads it.

BREAKING CHANGE: created_at is no longer a filterable or orderable field
on the raw read path, and the canonical keyset is (window_end, id). A
caller ordering by created_at is rejected.
BODY
)"
```

---

## Task 5: bind the time range into the cursor fingerprint

**Files:**
- Modify: `usage-collector/src/api/rest/handlers/usage_records.rs`, `.../usage_records_tests.rs`

Why this task exists: `CursorV1.f` and `ODataQuery.filter_hash` exist so that a caller who changes their `$filter` between pages is rejected with `FILTER_MISMATCH` instead of being served a keyset continuation minted over a different row set. Until this slice the mandatory window lived *inside* `$filter`, so the fingerprint covered it for free. Task 3 moved the range out, which silently dropped that protection: a page-2 request can now carry the same cursor with a different `from` / `to` and be served. Restoring the property is one function.

This is a gateway concern, so it lives where cursor validation already lives — the REST handler. In-process SDK callers own their own cursor discipline, exactly as they do today; nothing in the service validates a cursor.

- [ ] **Step 1: Write the failing tests**

In `usage-collector/src/api/rest/handlers/usage_records_tests.rs`, in the module that already tests `prepare_list_query` and builds `CursorV1` by hand:

```rust
#[test]
fn a_cursor_minted_under_a_different_range_is_rejected() {
    // The keyset continuation is only meaningful over the row set that
    // minted it. Since the range left $filter it has to enter the
    // fingerprint explicitly, or page 2 of a January query happily
    // continues from a cursor minted over February.
    let range_a = TimeRange::new(at(1_700_000_000), at(1_700_003_600)).expect("range");
    let range_b = TimeRange::new(at(1_800_000_000), at(1_800_003_600)).expect("range");

    let mut minted = ODataQuery::default();
    minted.filter_hash = Some(effective_filter_hash(&minted, range_a));

    let mut follow_up = ODataQuery::default();
    follow_up.cursor = Some(CursorV1 {
        k: vec!["2023-11-14T23:13:20.000000Z".into()],
        o: SortDir::Asc,
        s: "+window_end,+id".into(),
        f: minted.filter_hash.clone(),
        d: "fwd".into(),
    });

    let err = prepare_list_query(follow_up, range_b)
        .expect_err("a cursor from another range must be refused");
    // assert the canonical reason is the filter-mismatch envelope, using
    // whatever assertion the neighbouring
    // `cursor_filter_hash_mismatch_surfaces_filter_mismatch_reason` test uses.
}

#[test]
fn a_cursor_minted_under_the_same_range_is_accepted() {
    // …same construction, one range, expect Ok, and assert the returned
    // query carries the reconstructed (window_end, id) order.
}

#[test]
fn the_fingerprint_covers_both_the_filter_and_the_range() {
    let range_a = TimeRange::new(at(1_700_000_000), at(1_700_003_600)).expect("range");
    let range_b = TimeRange::new(at(1_800_000_000), at(1_800_003_600)).expect("range");
    let plain = ODataQuery::default();
    let filtered = query_with_filter("tenant_id eq 11111111-1111-1111-1111-111111111111");

    assert_ne!(
        effective_filter_hash(&plain, range_a),
        effective_filter_hash(&plain, range_b),
        "the range must move the fingerprint",
    );
    assert_ne!(
        effective_filter_hash(&plain, range_a),
        effective_filter_hash(&filtered, range_a),
        "the filter must move the fingerprint",
    );
    assert_eq!(
        effective_filter_hash(&filtered, range_a),
        effective_filter_hash(&filtered, range_a),
        "and it must be stable",
    );
}
```

Build the `$filter`-carrying query with whatever helper the file already has for parsing an OData filter; if there is none, parse with `toolkit_odata`'s filter parser as the neighbouring tests do.

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo nextest run -p cf-gears-usage-collector cursor
```

Expected: compilation failure — `effective_filter_hash` does not exist and `prepare_list_query` takes one argument.

- [ ] **Step 3: Implement**

```rust
/// The effective filter fingerprint a cursor is validated against, and the
/// value the plugin mints into the next cursor.
///
/// `CursorV1.f` exists so that a caller who changes their query between
/// pages is refused rather than served a keyset continuation minted over a
/// different row set. Until the covered period became a typed parameter the
/// mandatory window lived inside `$filter`, so
/// `toolkit_odata::short_filter_hash` covered it for free. It no longer
/// does, so the range enters the fingerprint here — otherwise a page-2
/// request carrying the same cursor with a different `from` / `to` is
/// served a continuation that means nothing over its own row set.
///
/// The range contributes its canonical bound rendering rather than a second
/// hash: the value is opaque to callers, and one fewer hashing primitive is
/// one fewer thing that can disagree with itself. The PDP scope is
/// deliberately absent — it is server-injected, not caller-controlled, and
/// `compose_query_with_scope` documents why it must stay out.
fn effective_filter_hash(query: &ODataQuery, time_range: TimeRange) -> String {
    let filter = toolkit_odata::short_filter_hash(query.filter()).unwrap_or_default();
    format!(
        "{filter}~{}~{}",
        usage_collector_sdk::id::canonical_period_bound(time_range.from_inclusive()),
        usage_collector_sdk::id::canonical_period_bound(time_range.to_exclusive()),
    )
}
```

`prepare_list_query(mut query: ODataQuery, time_range: TimeRange)` sets

```rust
    query.filter_hash = Some(effective_filter_hash(&query, time_range));
```

**before** the cursor-validation block (step 3 in that function), so `validate_cursor_against` compares against the range-bound value. `prepare_list_request` passes the range it just parsed.

Check the round trip while you are here: `compose_query_with_scope` clones `filter_hash` through to the plugin, the plugin mints `CursorV1.f` from it, and the next request recomputes the same string — so the fingerprint is self-consistent across a page boundary. Say so in the function doc.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo nextest run -p cf-gears-usage-collector cursor
```

- [ ] **Step 5: Falsify**

Drop the range from `effective_filter_hash` (return the bare `short_filter_hash`). Confirm `a_cursor_minted_under_a_different_range_is_rejected` fails. Restore with `cp` + `touch`, confirm a `Compiling` line, re-run.

- [ ] **Step 6: Verification bar and commit**

```bash
git commit -s -m "fix(usage-collector): bind the read-path time range into the cursor fingerprint" -m "$(cat <<'BODY'
CursorV1.f and ODataQuery.filter_hash exist so a caller who changes their
query between pages is refused rather than served a keyset continuation
minted over a different row set. While the mandatory window lived inside
$filter, short_filter_hash covered it for free. Moving the range to a
typed parameter dropped that protection silently: a page-2 request could
carry the same cursor with a different from / to and be served.

The gateway now folds the canonical rendering of both bounds into the
effective fingerprint before validating the cursor, so a range change
between pages surfaces as the same filter-mismatch 400 a $filter change
always did. The value round-trips: composition carries filter_hash to the
plugin, the plugin mints it into the next cursor, and the follow-up
request recomputes the same string. The PDP scope stays out for the
reason compose_query_with_scope already documents - it is
server-injected, so hashing it would embed a value the gateway's own
recomputation can never match.
BODY
)"
```

---

## Task 6: retire the `created_at` vocabulary and verify the slice

**Files:** whatever the greps below turn up, plus the report.

No production behaviour changes here. This exists because renaming across a large diff reliably leaves prose that a reader can falsify with one grep, and three slice-1/2 reviews each caught one.

- [ ] **Step 1: Run the sweep**

```bash
cd gears/system/usage-collector

# 1. The retired field name, anywhere in the live crates.
grep -rn "created_at" --include='*.rs' usage-collector usage-collector-sdk plugins/noop-usage-collector-plugin

# 2. The retired reason code.
grep -rn "MISSING_TIME_WINDOW\|MissingTimeWindow\|missing_time_window" --include='*.rs' .

# 3. The retired identity shape.
grep -rn "4-tuple\|created_at_micros\|into_usage_record\b" --include='*.rs' usage-collector usage-collector-sdk

# 4. Wrong ADR references: 0007 is the identity derivation, 0014 is
#    window-end selection. Every ADR-0014 citation about identity is stale.
grep -rn "ADR-0014\|ADR-0007" --include='*.rs' usage-collector usage-collector-sdk plugins/noop-usage-collector-plugin

# 5. Prose that describes the old model.
grep -rniE "record creation timestamp|TimeWindow|time window .*filter|unbounded (full-table )?scan" --include='*.rs' usage-collector usage-collector-sdk
```

Expected end state: (1) (2) (3) return nothing; (4) returns only citations that name the right document for what the surrounding code does — prefer the `cpt-cf-usage-collector-adr-*` id over the number, which is how the rest of the gear cites ADRs; (5) returns nothing that claims the window is a filter conjunct.

- [ ] **Step 2: Confirm the out-of-scope boundaries held**

```bash
cd /Users/binarycode/code/virtuozzo/gears-rust
git diff --stat main...HEAD -- gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/
git diff --stat main...HEAD -- gears/system/usage-collector/docs/DECOMPOSITION.md gears/system/usage-collector/docs/features/
git diff --stat main...HEAD -- '*Cargo.toml'
```

All three must be empty. The third catches an accidental version bump.

- [ ] **Step 3: Run the full verification bar and record the numbers**

```bash
cargo check --workspace --all-targets
cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk -p cf-gears-noop-usage-collector-plugin
cargo clippy --workspace --all-targets --all-features
cargo +nightly fmt
```

The 6 `#[ignore]`d OpenAPI drift tests must still be skipped, not failing and not re-enabled. Report the passing count against the 485 baseline and account for the delta: tests added, tests deleted (with the per-test verdicts from tasks 2 and 3), tests repointed.

- [ ] **Step 4: Commit anything the sweep changed**

```bash
git commit -s -m "docs(usage-collector): retire the created_at vocabulary from the gear" -m "$(cat <<'BODY'
Prose left behind by the rename. Each of these is falsifiable by one
grep: a doc citing ADR-0014 for the identity derivation (0014 is
window-end selection; 0007 is the derivation), a comment describing the
mandatory window as a $filter conjunct, and the dedup identity called a
4-tuple.
BODY
)"
```

- [ ] **Step 5: Report**

State, explicitly:

1. The verification-bar output and the test-count delta.
2. Every falsification: what you weakened, which test failed, and anything that did **not** fail when you expected it to.
3. Every deleted test with its verdict.
4. Any sketch in this plan that did not survive contact with the code.
5. The residual divergences from the target contract this slice knowingly leaves: `accepted_at` / `acceptance_sequence` absent, `value` not yet renamed to `quantity`, and the aggregate path still carrying `gts_type_id` / `$filter` / `metadata.<key>` as query parameters where `AggregationRequest` puts them in the body.

---

## Self-review

**Spec coverage** — every slice-3 bullet from the handoff, mapped to a task:

| Requirement | Task |
| --- | --- |
| `created_at` becomes `window_start` / `window_end` | 2 |
| Read paths select by period end; no path reads `window_start` to select; no overlap or containment | 1 (predicate), 3 (SPI obligation), 4 (column) |
| `TimeRange` a typed parameter on both read paths; stops being a `$filter` conjunct | 1, 3 |
| `require_bounded_time_window` and `ValidationReason::MissingTimeWindow` removed | 3 |
| Page cursor keyset moves to `(window_end, id)` | 4 |
| Equal bounds are a valid point event, not an error | 1 (selection), 2 (ingestion) |
| Identity over ADR-0007's 5-tuple, 27-character bound form, `0x1F` separator, namespace unchanged | 2 |
| Sub-microsecond precision rejected, not truncated | 2 |
| Second value of `60` rejected | 2 (subsumed by the precision check — see the API-facts table) |
| Golden vectors regenerated and verified independently against the ADR | 2, step 1 |

Two additions beyond the handoff's bullet list, both stated with their reasons in-place: the cursor fingerprint (task 5) closes a protection that task 3 would otherwise drop silently, and `IdempotencyKey`'s control-character rejection (task 2) is a precondition ADR-0007 states and whose test its Confirmation section requires.

**Placeholders** — none. Every code step carries the code; test steps that adapt to an existing fixture say which fixture and what to assert.

**Type consistency** — `TimeRange::new` / `from_inclusive` / `to_exclusive` / `contains_window_end`, `canonical_period_bound`, `derive_usage_record_id(tenant, &gts, &key, window_start, window_end)`, `try_into_usage_record`, `effective_filter_hash(&query, time_range)`, `TYPED_LIST_PARAMS` / `TYPED_AGGREGATE_PARAMS`, `last_list_time_range` / `last_aggregate_time_range` are spelled identically in every task that names them.
