# Spec divergences in the usage-collector

Eleven places where `usage-collector-v1.yaml`, `DESIGN.md` or
`DECOMPOSITION.md` describe behaviour the code does not have.

They were left unedited on purpose. Correcting a governing document is the
spec owner's call, not the implementer's.

**Entries 1-5** came out of slice 3, the time-model slice
(`usage-collector/implementation-change`, 23 commits ending `74e3d4429`).
**Entries 6-11** came out of slice 4, the append-only correction-model slice
(seven commits ending `531227f45`, plus the sweep commit that added them).

In **eight of the eleven the code is the correct side** and the document is
imprecise or stale. Three are not that shape, and saying so matters more than a
tidy summary:

- **Entry 8** — the three documents agree with each other and the code
  implements none of it. The published 28-digit quantity range is enforced by
  nothing and exercised by no test.
- **Entry 10** — neither side is simply right. The gear cannot serve the
  contract's shape and the contract does not describe the gear's; closing it is
  a spec decision plus a scheduled slice, and the entry says so rather than
  picking a winner.
- **Entry 9(c)** — `origin` is unemitted because the slice that builds it has
  not run. Unbuilt scope, not a document defect.

**Seven** are load-bearing rather than cosmetic, and each is marked below. The
sharpest is entry 10: a client generated from the published contract cannot
submit a single record, in two independent ways.

---

## 1. The keyset guarantee is membership, not append position

**Three sites, one claim.**

`gears/system/usage-collector/docs/usage-collector-v1.yaml:462` — the `OrderBy`
parameter:

> the server **appends the canonical unique `(window_end, id)` tiebreaker** in
> that direction.

`gears/system/usage-collector/docs/DESIGN.md:587` — §3.1 "Order admissibility":

> The gateway **appends `(window_end, id)`** in the caller's direction before
> dispatch, so the plugin always receives a gap-free, uniform-direction,
> never-null keyset.

`gears/system/usage-collector/docs/DESIGN.md:971` and `:1054` — the SDK trait and
Plugin SPI blocks, identically:

> `/// Keyset-paginated ledger read **over `(window_end, id)`**.`

**What the code does.** `ODataOrderBy::ensure_tiebreaker` appends only a field
that is *missing*, at the end, and skips one already named. So a caller ordering
by `id` — which is on the filterable schema and is keyset-safe — is handed on as
`(id, window_end)`. Neither canonical field is last when the caller names one
early: `$orderby=tenant_id,id` yields `(tenant_id, id, window_end)`.

**Why the code is right.** Every guarantee that matters still holds: the order is
non-empty, single-direction, never-null, and globally unique because `id` is
always present. Position is not needed for any of that, and forcing it would mean
discarding a key the caller asked for. The gear says so in
`usage-collector/src/domain/query.rs` (`CANONICAL_KEYSET_FIELDS`, renamed from
`…_SUFFIX` precisely because every doc that inherited "suffix" inherited "ends
in" with it), and `plugin_api.rs` states the non-claim explicitly: *"Those two
names are guaranteed to be present, not to be last."*

**Load-bearing.** §3.3's contract-test table is what a plugin author implements
against. A test written literally from these sentences — assert the effective
order ends in `(window_end, id)` — fails on `$orderby=id` against conforming
code.

**Proposed wording**, for all three sites:

> the server ensures the effective order names both `window_end` and `id`,
> appending whichever is missing in the caller's direction; a key the caller
> named keeps its position.

---

## 2. A cursor binds more than the order and the filter

`gears/system/usage-collector/docs/usage-collector-v1.yaml`, the `Cursor`
parameter:

> It binds **the order and filter** it was minted under: a malformed token, a
> changed `$filter`, or an accompanying `$orderby` is rejected `400` with a
> `cursor` field violation (`INVALID_CURSOR`, `FILTER_MISMATCH`,
> `ORDER_WITH_CURSOR`). Continue a page by resending the same query, cursor
> apart.

**What the code does.** The fingerprint binds **every row-selecting parameter**:
the meter (`gts_type_id`), the read range, the caller's `$filter`, and the
metadata filter. A token whose fingerprint differs in any of them is refused. A
token carrying **no** fingerprint is refused too.

**Why the code is right.** Before this slice the mandatory time window lived
inside `$filter`, so a filter hash covered it for free. Once the range became a
typed parameter that stopped being true, and `gts_type_id` and `metadata_filter`
were never inside `$filter` at all — so a caller could continue a cursor against
a different meter and be served a page from a different type as a `200`. The
description's own last sentence, *"resending the same query, cursor apart"*, is
the faithful rule; the enumeration listing only order and filter is the
imprecise half.

**Proposed wording:**

> It binds the query it was minted under — the order, the `$filter`, the GTS
> type, the read range, and any metadata filter. Continue a page by resending
> the same query, cursor apart; any difference, a malformed token, an
> accompanying `$orderby`, or a token carrying no binding is rejected `400` with
> a `cursor` field violation.

---

## 3. The cursor-rejection cause lists are two causes short

`gears/system/usage-collector/docs/DESIGN.md:1177`:

> A malformed token, a changed filter, or an order supplied alongside a cursor is
> rejected as `InvalidArgument` with a `cursor` field violation —
> `INVALID_CURSOR`, `FILTER_MISMATCH`, `ORDER_WITH_CURSOR`, and, in process only
> where a caller can set both at once, `ORDER_MISMATCH`.

The `Cursor` parameter in the yaml carries the same three-cause list.

**Two causes are missing**, both added by this slice and both reusing codes the
documents already enumerate — so there is no code-level divergence, only an
incomplete list:

- **A continuation whose bound order is not a sound keyset** — mixed directions,
  a nullable key, or a retired field name. Refused as `INVALID_CURSOR`. Reachable
  only from a forged token or a non-conforming plugin.
- **A continuation carrying no fingerprint at all.** Refused as
  `FILTER_MISMATCH`. This is the shape a plugin that never learned to carry
  `query.filter_hash` into `next_cursor.f` produces.

One nuance worth folding in while editing: `ORDER_MISMATCH` is described as
in-process-only "where a caller can set both at once". As of this slice the
in-process path takes a continuation's order **from the token** rather than from
the caller, so that condition is no longer reachable — the order the caller
supplies alongside a cursor is now overwritten, not compared.

---

## 4. `uc_query_requests_total` lists a label that cannot fire and omits one that can

`gears/system/usage-collector/docs/DESIGN.md:1780`, §3.11.5:

> `error_category` (`none`, `missing_security_context`, `authz`,
> `unresolved_type`, `cursor_decode`, `undeclared_field`, **`missing_time_range`**,
> `query_budget`, `plugin_error`)

Three problems:

- **`missing_time_range` is unreachable.** The range is a typed, mandatory
  parameter constructed at the edge, so a request that omits or inverts it is
  rejected before the service is entered and never reaches the counter. The
  category existed for the retired `require_bounded_time_window` guard.
- **`filter_mismatch` is missing.** It is now reachable: a continuation whose
  fingerprint does not match the current query is refused in the service, and
  `classify_query_result` maps it to `QueryErrorCategory::FilterMismatch`.
- **`undeclared_field` is never emitted** by any path. Pre-existing.

**Load-bearing.** An operator can build an alert on a label that will never
fire, and will not see the one condition this slice made reachable.

Two smaller notes for the same edit. A continuation refused for an *unsound
order* currently folds into `query_budget`, because neither `cursor_decode` (a
genuine decode failure) nor `order_mismatch` (a caller `$orderby`) describes it;
`order_mismatch` is arguably its right home and needs no new label. And the
`$orderby` refusals that moved into the domain in this slice — a mixed-direction
order, an inadmissible order key from an in-process caller — also land on
`query_budget` today.

---

## 5. `DECOMPOSITION.md` still declares the retired time model normatively

`gears/system/usage-collector/docs/DECOMPOSITION.md:312`:

> Bounded time window — expressed as `created_at ge … and created_at lt …`
> predicates in the `ODataQuery` `$filter` on both `list_usage_records` and
> `query_aggregated_usage_records` (mandatory; a lower and an upper bound are
> required, else `MISSING_TIME_WINDOW`). There is no free-standing `TimeWindow`
> entity — `created_at` is a first-class `UsageRecordFilterField`.

Every clause is now false. The window is a typed `TimeRange` parameter and never
a `$filter` conjunct; the covered-period bounds are *reserved* on the filter
surface and a predicate naming one is rejected; `MISSING_TIME_WINDOW` and the
guard that raised it are deleted; and `created_at` is not a field of the record
or of the filterable schema.

`DECOMPOSITION.md` and `docs/features/*.md` were already known to be stale — the
slice-3 handoff named them as such and put refreshing them out of scope. This
entry exists because that paragraph is *specifically* about the thing slice 3
replaced, so it is the one most likely to mislead someone reading for the time
model.

---

## 6. The invalidation rejections carry wire reasons the contract does not enumerate

`gears/system/usage-collector/docs/usage-collector-v1.yaml` names
`IDEMPOTENCY_CONFLICT` (`:910`, `:935`) and `VALIDATION` (`:465`) in prose and
enumerates **no** invalidation-specific reason anywhere.
`gears/system/usage-collector/docs/DESIGN.md:1779` (§3.11.5) names an
`invalidation_rule` *metric* category, which is a label vocabulary, not a wire
one.

**What the code does.** The append-only correction model emits four wire reason
codes the contract does not list:

- `INVALIDATION_REFERENCE_INCOMPLETE` — `invalidates` without `reason_code`, or
  the reverse, raised at the REST fold point.
- `INVALIDATION_TARGET_NOT_RECORD` — the target is itself an invalidation.
- `INVALIDATION_FIELD_MISMATCH` — a caller-supplied field differs from the
  target's; `field` names the one that differs.
- `ALREADY_INVALIDATED` — the target already carries an invalidation
  (plugin-detected, lifted onto `context.reason`).

**Why the code is right.**
`cpt-cf-usage-collector-adr-append-only-invalidation` requires a rejection that
*names the field that differs*. A single `VALIDATION` code cannot express that,
and a client cannot tell "you withdrew the wrong thing" from "your period
bounds are bad" without one. Collapsing the four back into `VALIDATION` would
make the ADR's rule unobservable at the boundary that enforces it.

**Load-bearing.** A client writing a typed matcher on `context.reason` has
nothing in the published contract to match against; it must read the source.

**Proposed wording:** enumerate the four alongside `IDEMPOTENCY_CONFLICT` and
`VALIDATION`, with `INVALIDATION_FIELD_MISMATCH` documented as carrying the
differing field in `field`. Slice 6 owns the final reason-vocabulary pass and
may rename; the gap is what needs recording, not the exact spellings.

---

## 7. The order-key rule refuses a field the contract marks admissible

`gears/system/usage-collector/docs/usage-collector-v1.yaml:455`, the `OrderBy`
parameter:

> Every key MUST name a field this shape marks required, and all keys MUST
> share one sort direction; … An optional field (`subject_id`,
> `subject_type`, `invalidates`, a declared metadata property) or a mixed
> direction is rejected `400` (`VALIDATION`).

`gears/system/usage-collector/docs/DESIGN.md:587` — §3.1 "Order admissibility"
states the same rule with the same rejection list.

`entry_type` **is** in `UsageRecord.required` (yaml `:719`) and appears in
neither rejection list. Per both documents, `$orderby=entry_type` is
admissible.

**What the code does.** Refuses it `400`, and pins that `400` with a test.
`entry_type` is on the filterable schema (`UsageRecordQuery`) and deliberately
absent from `KEYSET_SAFE_RECORD_FIELDS`.

**Why the code is right.** `entry_type` is `UsageRecord::entry_type()`, a
function of the optional `invalidates` that partitions entries on that field's
*absence*. The SDK carries no such attribute and obliges no plugin to
materialize one, so it can promise no keyset over it — and the guarantee a
caller's `$orderby` rests on is the SDK's to give. A plugin *may* materialize a
generated column to make `$filter=entry_type eq 'record'` work; that still buys
no order key.

Note the rule the yaml states and the rule the code applies are different
rules, which is why the lists disagree: "a field this shape marks required" is
a presence test, and presence is necessary but not sufficient. `entry_type` is
present on every entry and still not keyset-safe.

**Load-bearing.** A caller reading the contract to build an order gets a `400`
the contract says cannot happen.

**Proposed wording:** add `entry_type` to both rejection lists, and state the
ground as *derived from an optional attribute* rather than as optionality, so
the criterion covers the next derived field too.

---

## 8. The 28-digit quantity guarantee has no enforcement and no test

**This is the one entry where the code is the wrong side.** Three documents
agree with each other and the gear implements none of it.

`gears/system/usage-collector/docs/usage-collector-v1.yaml:559`, the
`UsageQuantity` schema:

> Published range: at most 28 significant decimal digits (leading zeros
> excluded) and at most 28 digits after the decimal point. The magnitude is
> therefore below 10²⁸, and the smallest non-zero value is 1×10⁻²⁸.
> Every storage plugin MUST round-trip that full range — **including its
> negative half** — and the full precision without loss …

`gears/system/usage-collector/docs/DESIGN.md:570` — §3.1 "Quantity fidelity"
repeats the range verbatim. §3.3 lists "quantity range" among the gateway
responsibilities a plugin may therefore skip.

**What the code does.** Nothing checks any of it. `models.rs` and
`validation.rs` validate `ReasonCode` length, `IdempotencyKey` shape and the
metadata surface, and say nothing about `Decimal` magnitude or scale. No test
exercises a boundary value — not 10²⁸, not 1×10⁻²⁸, not the negative half. The
yaml's `UsageQuantity` `pattern` caps the *fraction* at 28 digits on the wire
and bounds the integer part not at all, so even the schema does not enforce the
prose.

The sign half is fine and deliberately so: a negative quantity is an ordinary
measurement recording a real decrease, never a correction, and that is stated
in `models.rs`, `reason.rs` and the ADR alike. It is the *range* that is
unguarded.

Found while fixing slice 4's sign-rule doc — the same grep that showed nothing
rejects a negative quantity showed nothing checks its magnitude either.

**Pre-existing**, not introduced by the correction-model slice.

**What to do:** slice 6's `quantity-round-trip` contract test is where the
plugin half lands. That leaves the gateway half — rejecting an out-of-range
submission at ingestion, which §3.3 promises a plugin it need not do — unowned.
Do not implement it here.

---

## 9. `uc_ingestion_records_total`'s label row does not match what the gear emits

`gears/system/usage-collector/docs/DESIGN.md:1779`, §3.11.5:

> `outcome` (`accepted`, `duplicate`, `rejected`), `entry_type` (`record`,
> `invalidation`), `origin` (`live`, `backfill`), `error_category` (`none`,
> `authz`, `unresolved_type`, `validation`, `idempotency_conflict`,
> `invalidation_rule`, `plugin_error`) … `invalidation_rule` covers the copy,
> reference and at-most-one rules alone.

Three problems, all on one table row.

**(a) `invalidation_rule` cannot cover the reference rule.** The copy and
at-most-one rules are classifiable — they raise
`ValidationReason::InvalidationFieldMismatch` /
`ValidationReason::InvalidationTargetNotRecord` and
`ConflictReason::AlreadyInvalidated`. The reference rule is not: an
unresolvable `invalidates` surfaces as `UsageCollectorError::NotFound`, and
that variant carries **no typed reason** — nothing but `detail` prose separates
it from an ordinary `usage_record_not_found`, and a plugin's own
`UsageRecordNotFound` reaches the same arm. `classify_record_error`
(`usage-collector/src/domain/service.rs`) declines to classify a bounded metric
label by substring match on a caller-facing string and leaves it on
`semantics_violation`, with the reasoning in the code.

**(b) Three of the seven listed label values are not the values emitted.**
`RecordErrorCategory::as_str` emits `none`, `authz`, `unknown_usage_type`,
`semantics_violation`, `invalidation_rule`, `metadata_size`,
`idempotency_conflict`, `plugin_error` — eight values. So `unresolved_type` and
`validation` can never fire, and `unknown_usage_type`, `semantics_violation`
and `metadata_size` are absent from the document. This is the same shape as
entry 4 on a different instrument, and it is **pre-existing**: those three
labels predate the correction-model slice, and `invalidation_rule`, the label
that slice added, is the one that matches.

**(c) `origin` is not emitted at all.** `record_ingestion_record` takes
`outcome`, `entry_type` and `error_category` and no fourth label.
`RecordOrigin` and the backfill path are slice 5's; the correction-model slice
added the `entry_type` label and deliberately did not half-build `origin`.

**Why the code is right on (a) and (b).** Classifying a bounded metric label by
substring match on a message string is worse than a slightly wrong label: the
label stops matching silently the day the message is reworded. Renaming
`semantics_violation` to `validation` to satisfy the document would break every
existing dashboard for no gain.

**Load-bearing.** An operator building an alert off this row alerts on two
labels that can never fire, misses three that do, and gets a correction-backlog
count that under-reports by one rule.

**Proposed wording:** replace the `error_category` enumeration with the eight
values `RecordErrorCategory` actually emits; narrow the `invalidation_rule`
sentence to the rules it can cover, or grow `NotFound` a typed reason so it can
cover the third. That second option is a design decision, and slice 6's
reason-vocabulary pass is where it lands. Leave `origin` in the row as the
forward statement it is, and note slice 5 as its owner.

---

## 10. The REST shapes and `usage-collector-v1.yaml` are incompatible, not merely divergent

Every other entry here is a document that describes working code imprecisely.
This one is a contract a generated client cannot use at all.

**Request side — a generated client cannot submit a single record.**
`CreateUsageRecordRequest` in the yaml (`:810`) lists `quantity` in `required`
and carries `additionalProperties: false`. The DTO
(`usage-collector/src/api/rest/dto.rs`) declares the field as **`value`** and
carries `#[serde(deny_unknown_fields)]`. So a client generated from the
published contract sends `quantity`, which the DTO refuses as an unknown field,
*and* omits `value`, which the DTO refuses as missing. Two independent
failures, both `400`, on every request.

**Response side — the emitted body is not an instance of the declared schema.**
`UsageRecord` in the yaml (`:719`) lists `accepted_at`, `acceptance_sequence`
and `origin` in `required`; none exists on `UsageRecordDto` or on the SDK's
`UsageRecord`. The same schema declares `additionalProperties: false` while the
gear emits `value`, which it does not declare. So the body is not a strict
subset of the contract; it is a non-instance in two independent directions.

**What the code does, and why it is where it is.** Both gaps are out of the
correction-model slice by explicit scope, and both are honest scope rather than
oversight:

- The `value` → `quantity` rename was out of slice 3's list and out of slice
  4's. It is a wire break on the ingestion path and deserves its own commit.
- `origin` is slice 5's, with `RecordOrigin` and the backfill route.
- `accepted_at` and `acceptance_sequence` are claimed by **no slice**. DESIGN
  §3.1 says the plugin assigns `acceptance_sequence` monotonically per
  `(tenant_id, gts_type_id)` and that the `LATEST` fold breaks ties on it, and
  `usage-collector-sdk/src/models.rs` documents that tie-break against a field
  the record does not carry. **This blocks a conformant storage plugin**: there
  is no field for it to assign and no field for the fold to read.

**Pinned in both directions.** `api/rest/dto_tests.rs` asserts the literal wire
key set the gear accepts (`create_usage_record_request_accepts_exactly_the_declared_wire_keys`)
and the literal key sets it emits, for an ordinary measurement and for a
withdrawal alike (`usage_record_dto_serialises_exactly_the_declared_wire_keys`),
and says in its own comment that this is the gear's shape and not the
contract's. **Those assertions fail the day either gap closes** — which is the
intent: the tests and this document say the same thing, and closing a gap
without updating both is not possible quietly.

**Load-bearing**, more decisively than any other entry here: no generated
client works at all.

**Proposed resolution:** this one is not a wording fix. Either the code renames
`value` and grows the three server-assigned fields, or the contract is
corrected to the shape the gear serves. Whichever way it goes it is a spec
decision plus a scheduled slice, not an editorial pass.

---

## 11. Two §3.1 invariant rows credit the Query Gateway with enforcement it does not do

`gears/system/usage-collector/docs/DESIGN.md:577` — §3.1, the "Withdrawal
exclusion" row, "Enforced by" column:

> Query Gateway + every plugin (§3.3 contract test)

`gears/system/usage-collector/docs/DESIGN.md:569` — the "Period-end selection"
row, identically:

> Query Gateway + every plugin (§3.3 contract test)

**What the code does.** The Query Gateway enforces no part of either.
`usage-collector/src/domain/service.rs` composes the PDP scope into the
caller's filter, dispatches, caps the returned bucket count and records
telemetry; it inspects no row, folds nothing, and applies no period predicate —
the range is a typed `TimeRange` parameter handed straight to the SPI.
`usage-collector-sdk/src/plugin_api.rs` says so outright on the fold obligation:
*"The gear enforces neither."*

**Why the code is right.** Both rules are properties of the selection and the
fold, which happen inside the store. The gateway cannot exclude a withdrawn
pair without reading and re-folding the plugin's answer, which is the thing the
server-side fold exists to avoid; and it cannot apply `from <= window_end < to`
without pushing a predicate the SPI deliberately keeps out of `$filter`.

**Load-bearing.** The "Enforced by" column is what a plugin author implements
against. A plugin author who reads "Query Gateway + every plugin" and infers
the gateway does its half ships a fold that includes withdrawn pairs — and the
result is a silently inflated charge, not an error. The `Period-end selection`
row has the same shape and is **pre-existing**, which is why both are recorded
in one entry: it is one edit to one column.

**Proposed wording:** "every plugin (§3.3 contract test)" for both rows,
matching the `Quantity fidelity` and `LATEST tie-break` rows, which already
attribute plugin-only rules to the plugin alone.

---

## Evidence

Two dated lines, not one. The slice-3 line is what makes entries 1-5
checkable; overwriting it would strand them.

**Slice 4 (append-only correction model), verified at `531227f45` on
`usage-collector/implementation-change`, 2026-09-08:** **648 passed, 6
skipped** across the three usage-collector packages; `cargo check --workspace
--all-targets`, `cargo clippy --workspace --all-targets --all-features` and
`cargo +nightly fmt --check` clean; `cargo doc --no-deps` clean on
`cf-gears-usage-collector-sdk` and 35 pre-existing warnings on
`cf-gears-usage-collector`, none of which names a symbol this slice created,
moved or deleted. The 6 skipped are the same `#[ignore]`d OpenAPI drift tests.
Entries 6-11 were verified against the branch at that commit; the sweep commit
that adds them changes doc comments, `DIVERGENCES.md` and the quarantined
Python e2e suite, and no behaviour.

Slice 4 touched no file under `gears/system/usage-collector/docs/` either, so
the quotations in entries 6-11 are the documents as slice 3 left them.

**Slice 3 (time model), verified at `74e3d4429` on
`usage-collector/implementation-change`:** **606 passed, 6 skipped** across the three usage-collector packages, 724
with `cf-gears-toolkit-odata`; `cargo check --workspace --all-targets`, `cargo
clippy --workspace --all-targets --all-features` and `cargo +nightly fmt
--check` clean. The 6 skipped are the `#[ignore]`d OpenAPI drift tests, which
are separate work.

Every quotation above is the document **as it stands on this branch**, and every
line number is the branch's. Slice 3 touched no file under
`gears/system/usage-collector/docs/` — the earlier slices on this branch are what
put those documents in their current state, and `main` has not caught up:
`usage-collector-v1.yaml` is 1241 lines here against 1049 on `main`, with
different blob hashes for it, `DESIGN.md` and `DECOMPOSITION.md` alike. So do
not expect these line numbers, or in places these sentences, to resolve against
`main`.

Checked with `git rev-parse HEAD:<path>` against `git rev-parse main:<path>`
rather than `git diff --quiet`, which reported the three files identical and was
wrong. Worth knowing if you reach for the same check.
