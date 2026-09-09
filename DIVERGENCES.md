# Spec divergences in the usage-collector

Sixteen places where `usage-collector-v1.yaml`, `DESIGN.md`,
`DECOMPOSITION.md` or a file under `docs/features/` describes behaviour the
code does not have.

They were left unedited on purpose. Correcting a governing document is the
spec owner's call, not the implementer's.

**Entries 1-5** came out of slice 3, the time-model slice
(`usage-collector/implementation-change`, 23 commits ending `74e3d4429`).
**Entries 6-11** came out of slice 4, the append-only correction-model slice
(the ten commits ending `9e36bbf6b`; entries 6-11 were added by the sweep
commit `4c7d338a1`, the file itself was first committed in `46274f6ba`, and the
corrections its own review found landed after that).
**Entries 12-16** came out of slice 5, the record-origin and backfill slice
(the thirteen implementation tasks ending `a94d542cf`).

In **eleven of the sixteen the code is the correct side** and the document is
imprecise or stale. Five are not that shape, and saying so matters more than a
tidy summary:

- **Entry 8** — the three documents agree with each other and the code
  implements none of it. The published 28-digit quantity range is enforced by
  nothing and exercised by no test.
- **Entry 10** — neither side is simply right. The gear cannot serve the
  contract's shape and the contract does not describe the gear's; closing it is
  a spec decision plus a scheduled slice, and the entry says so rather than
  picking a winner.
- **Entry 12** — the document is right and the gear owes it a guarantee it
  does not provide. The backfill route publishes workload isolation the code
  falsifies, so the gear dropped the claim from what it registers rather than
  ship it.
- **Entry 15** — DESIGN gives `$filter` and `group_by` one field set and the
  SDK gives `group_by` five of the eight. The narrower surface is unbuilt
  scope, not a document defect.
- **Entry 16** — the TimescaleDB plugin's filter allowlist names three columns
  the model no longer has and is missing five the gear needs. The plugin is the
  deficient side, and it is outside both this slice's edit surface and the
  workspace: it has not compiled since slice 4.

**Eleven** are load-bearing rather than cosmetic, and each is marked below. The
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

**And the same row carries entry 9's defect, which this entry originally
missed.** `uc_query_requests_total` lists `unresolved_type` while
`QueryErrorCategory::as_str` emits **`unknown_usage_type`** — the identical
document-versus-code mismatch entry 9 records for the sibling instrument, on the
row directly above it. `order_mismatch` is also emitted-capable and absent from
the documented set. A spec owner acting on this entry as first written would fix
the ingestion row and leave two dead labels on the query one.

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

**Three of the four ride a different wire slot from the fourth**, and a spec
owner enumerating them must not put them in one place. `ALREADY_INVALIDATED` is
a `409 Aborted` and rides `context.reason`, alongside `IDEMPOTENCY_CONFLICT`.
`INVALIDATION_REFERENCE_INCOMPLETE`, `INVALIDATION_TARGET_NOT_RECORD` and
`INVALIDATION_FIELD_MISMATCH` are `400`s and ride `field_violations[0].reason`,
alongside `VALIDATION` — see `usage-collector/src/infra/sdk_error_mapping.rs`.

**Load-bearing.** A client writing a typed matcher has
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

Two problems, both on one table row. There was a third — `origin` was not
emitted at all — and slice 5 closed it: `record_ingestion_record` now takes
the fourth label and `RecordOrigin` supplies it on both ingestion paths. The
sub-item is struck rather than rewritten, because nothing about it survives.

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

**(b) Two of the seven listed label values are never emitted, and three emitted values are absent from the list.**
`RecordErrorCategory::as_str` emits `none`, `authz`, `unknown_usage_type`,
`semantics_violation`, `invalidation_rule`, `metadata_size`,
`idempotency_conflict`, `plugin_error` — eight values. So `unresolved_type` and
`validation` can never fire, and `unknown_usage_type`, `semantics_violation`
and `metadata_size` are absent from the document. This is the same shape as
entry 4 on a different instrument, and it is **pre-existing**: those three
labels predate the correction-model slice, and `invalidation_rule`, the label
that slice added, is the one that matches.

The row's own prose names the case that makes `validation`'s absence concrete.
"A period-bound rejection is `validation` for either `entry_type`, since the
bound belongs to the path" — but a period-bound rejection is
`UsageCollectorError::InvalidArgument` carrying `ValidationReason::FutureWindow`
or `ValidationReason::PastWindow`
(`usage-collector-sdk/src/error.rs`, `covered_period_beyond_future_tolerance`
and `covered_period_before_past_tolerance`), and neither reason is named in
`classify_record_error`'s `InvalidArgument` arm, and no arm could name it:
`RecordErrorCategory` has no `Validation` variant to route them to. Both fall
to the catch-all and land on `semantics_violation`. So the one rejection DESIGN
spells out by name is the one that proves the label it is assigned to can never
fire. This is slice 5's contribution to (b) rather than an entry of its own:
slice 5 built the bounds the sentence describes and did not build the label the
sentence assigns them to, because there is no such label to build against.

**Why the code is right.** Classifying a bounded metric label by
substring match on a message string is worse than a slightly wrong label: the
label stops matching silently the day the message is reworded. Renaming
`semantics_violation` to `validation` to satisfy the document would break every
existing dashboard for no gain.

This paragraph was headed "Why the code is right **on (a) and (b)**" while
there were three sub-items, and the qualifier existed to exclude (c) — the one
place the code was not right, because it had not been written yet. With (c)
struck the qualifier named every remaining sub-item, so it excluded nothing
while still reading as though something were excluded. Dropped rather than
re-scoped: the entry is now wholly a document defect and says so in one
place.

**Load-bearing.** An operator building an alert off this row alerts on two
labels that can never fire, misses three that do, gets a correction-backlog
count that under-reports by one rule, and — following the row's own sentence —
looks for period-bound rejections under a label that is never emitted while
they accumulate under `semantics_violation` beside unrelated failures.

**Proposed wording:** replace the `error_category` enumeration with the eight
values `RecordErrorCategory` actually emits; narrow the `invalidation_rule`
sentence to the rules it can cover, or grow `NotFound` a typed reason so it can
cover the third. That second option is a design decision, and slice 6's
reason-vocabulary pass is where it lands. Rewrite the period-bound sentence to
name the category the bounds actually reach, or give them one; the `origin`
column needs no change — it is emitted.

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
`UsageRecord` in the yaml (`:719`) lists `accepted_at` and `acceptance_sequence`
in `required`; neither exists on `UsageRecordDto` or on the SDK's
`UsageRecord`. The same schema declares `additionalProperties: false` while the
gear emits `value`, which it does not declare. So the body is not a strict
subset of the contract; it is a non-instance in two independent directions.
`origin` was a third missing `required` field until slice 5; it is on both
shapes now and is no longer part of this entry.

**What the code does, and why it is where it is.** Both gaps are out of the
correction-model slice by explicit scope, and both are honest scope rather than
oversight:

- The `value` → `quantity` rename was out of slice 3's list and out of slice
  4's. It is a wire break on the ingestion path and deserves its own commit.
- `accepted_at` and `acceptance_sequence` are claimed by **no slice**, and
  slice 5 is where that stopped being a scheduling detail. DESIGN §3.1 has the
  gear stamp `accepted_at` and the plugin assign `acceptance_sequence`
  monotonically per `(tenant_id, gts_type_id)`, and says the `LATEST` fold
  breaks ties on it; `usage-collector-sdk/src/models.rs` documents that
  tie-break against a field the record does not carry. **This blocks a
  conformant storage plugin**: there is no field for it to assign and no field
  for the fold to read. `origin` was their sibling in the server-assigned
  group and shipped in slice 5; these two did not, and nothing on the roadmap
  picks them up. Re-flagged here rather than filled: inventing either field is
  a contract decision, not a sweep's.

**Pinned in both directions.** `api/rest/dto_tests.rs` asserts the literal wire
key set the gear accepts (`create_usage_record_request_accepts_exactly_the_declared_wire_keys`)
and the literal key sets it emits, for an ordinary measurement and for a
withdrawal alike (`usage_record_dto_serialises_exactly_the_declared_wire_keys`),
and says in its own comment that this is the gear's shape and not the
contract's. **Those assertions fail the day a gap closes** — which is the
intent: the tests and this document say the same thing, and closing a gap
without updating both is not possible quietly.

That has now happened once, and it is the evidence the pinning works.
`origin` closed in `d35cbdd05` (slice 5, "return the admitting path on every
read"), the response-side assertion went red on both its key sets, and the
same commit added `origin` to each and rewrote the test's comment to say which
gaps remain. The request-side assertion
(`create_usage_record_request_accepts_exactly_the_declared_wire_keys`) is
untouched: it still spells `value`, and it will go red on the `quantity`
rename. `accepted_at` and `acceptance_sequence` are still absent from both
emitted key sets.

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

## 12. The published backfill route promises a workload isolation the gear does not implement

`gears/system/usage-collector/docs/usage-collector-v1.yaml:246`, the summary of
`POST /usage-collector/v1/records/backfill`:

> Bulk historical import, isolated from live ingestion

and its description, immediately below it:

> Identical validation and request shape to `POST /records`, differing only in
> four respects: the workload is isolated from live ingestion so it cannot
> breach live-path SLOs, …

`gears/system/usage-collector/docs/DESIGN.md:101` says the same thing in the
NFR table, on the `cpt-cf-usage-collector-nfr-workload-isolation` row: "The
backfill path is isolated from live ingestion at the gear."

**What the code does.** `Service::backfill_usage_records`
(`usage-collector/src/domain/service.rs`) is
`create_usage_records_for_origin` under a different `origin`. Same runtime,
same connection pool, the same `PDP_CONCURRENCY` and
`TYPE_RESOLUTION_FANOUT_CONCURRENCY` fan-out budgets, and no separate bound of
any kind. A bulk import can degrade live ingestion p95. The method's rendered
doc says so, and a `TODO` naming the NFR sits in its body.

**What the gear publishes instead.** The registered route
(`usage-collector/src/api/rest/routes/usage_records.rs:70`) drops the claim
from **both** fields: the description enumerates "three respects" rather than
the contract's four, and the summary is "Bulk historical import of periods the
live path rejects" rather than the yaml's "isolated from live ingestion".
Dropping it from the description alone would have shipped the same guarantee
through the summary. `backfill_route_publishes_three_differences_and_claims_no_workload_isolation`
in `api/rest/routes/usage_records_tests.rs` pins both halves — negatively, that
neither field contains `isolat`, and positively, that the description still
names all three differences the gear does implement, since an empty
description satisfies the negative half and is a worse contract than a false
one.

**So the yaml diverges from the registry in two fields, not one.** The gear is
the deficient side on the substance — the published document states the
obligation and the code does not meet it — and the narrower registered text is
the consequence, deliberately chosen so the untrue half is not served to a
client while the obligation stands open.

**Load-bearing.** An operator reading the contract concludes a bulk import
cannot breach live-path SLOs, and schedules one against a live tenant on that
basis. Nothing in the gear-level suite goes red over the gap: the ADR's
confirmation case is a concurrent load test against
`cpt-cf-usage-collector-nfr-throughput-profile`, which this repository does not
run.

**Proposed resolution:** not a wording fix. Either the gear grows a separate
admission bound for the backfill path — its own fan-out budget at minimum —
and the registered text is widened to four respects, or the yaml and the DESIGN
NFR row drop the gear-level isolation claim and leave isolated backend pools as
the plugin-deployment obligation they already are. Until one of those happens,
`handle_backfill_usage_records` carries no flow or DoD marker for the same
reason — see §C below.

---

## 13. `uc_pdp_duration_seconds` cites a nine-value label set that no list in the document has

`gears/system/usage-collector/docs/DESIGN.md:1796`, §3.11.5 histograms:

> `uc_pdp_duration_seconds` | seconds | `operation` (same nine-value set)

There is no nine-value set to be the same as. The `operation` vocabulary is
enumerated once, on `uc_pdp_failures_total` (`:1784`), and it lists **seven**:
`ingest`, `backfill`, `query_raw`, `query_aggregated`, `get_record`,
`read_feed`, `reconciliation`. `uc_authz_decisions_total` refers to it
correctly, as "(same set)", with no number.

**What the code emits.** `PdpOp` (`usage-collector/src/domain/ports/metrics.rs`)
has **five** variants — `Ingest`, `Backfill`, `QueryRaw`, `QueryAggregated`,
`GetRecord`. `read_feed` and `reconciliation` belong to surfaces this gear does
not have, so seven is the document's forward-looking set and five is today's.
Nine matches neither.

**Why the code is right.** `PdpOp` is a closed Rust enum whose `as_str` is the
label; it cannot emit a value it has no variant for, and the two absent values
name components that do not exist. Slice 5 moved this number's neighbourhood —
`PdpOp` went from four variants to five when `Backfill` landed — which is what
surfaced the disagreement, but the "nine" predates it and was wrong at four as
well.

**Not marked load-bearing**, and the reason is worth stating rather than
leaving to inference: the sentence is a cross-reference, and a reader who
follows it lands on the seven-value enumeration and gets a usable answer. The
count is decoration on a working pointer. It is recorded because it is the same
class as entries 4 and 9 — a §3.11.5 label vocabulary stated twice and not
reconciled — and because the next person to widen `PdpOp` will read "nine" and
believe they have room.

**Proposed wording:** "(same set)", matching `uc_authz_decisions_total`. One
enumeration, one place.

---

## 14. `docs/features/usage-emission.md` still calls `uc_ingestion_duration_seconds` label-free

Three sites, one claim.

`gears/system/usage-collector/docs/features/usage-emission.md:176`
(`inst-emit-record-completion-metrics`), `:226`
(`inst-emit-batch-request-completion-metrics`) and `:1233` (the DoD that
restates them) all describe the histogram as "the label-free
`uc_ingestion_duration_seconds`".

**What the code does.** `Metrics::observe_ingestion_duration`
(`usage-collector/src/infra/metrics.rs:269`) records with
`KeyValue::new(key::ORIGIN, origin.as_str())`, and the instrument's own
description is "Ingestion request wall-clock by origin". One label, two bounded
values.

**Why the code is right.** DESIGN §3.11.5's histogram table (`:1792`) gives the
instrument `origin` (`live`, `backfill`), and separating a bulk import's
latency from live emission's is the only way the workload-isolation alert at
`:1833` can be read at all. The feature file contradicted DESIGN **before this
slice** — DESIGN carried the label as a forward statement while the code
emitted none — and slice 5 resolved the contradiction on the code's side and
not the feature file's, because `docs/features/` is outside this slice's edit
surface.

Two of the three carry a second staleness of their own — `:176` and `:1233`,
and `:225` beside the third — spelling the per-record counter's second label
`record_kind="compensation"` / `"usage"`, which slice 4 renamed to
`entry_type="invalidation"` / `"record"`. That is the §B rename below reaching
a third document.

**Load-bearing.** These are instruction-level steps, which is what an
implementer works from. Someone building or reviewing the emit point against
`inst-emit-batch-request-completion-metrics` would drop the label to match, and
`uc_ingestion_duration_seconds` would go back to being unable to answer the one
question the backfill route exists to make askable.

**Proposed wording:** replace "label-free" with the `origin` label at all
three, and take the `record_kind` spellings to `entry_type` in the same pass.

---

## 15. `AggregationDimension` carries five of the eight fixed dimensions DESIGN gives `group_by`

`gears/system/usage-collector/docs/DESIGN.md:520`, the `UsageRecordFilterField`
row — one sentence covering both surfaces:

> The admissible `$filter` **and `group_by`** field set: `tenant_id`,
> `resource_id`, `resource_type`, `subject_id`, `subject_type`, `entry_type`,
> `origin`, and `invalidates`, plus the queried type's declared metadata keys.

**What the code does.** `UsageRecordQuery`
(`usage-collector-sdk/src/models.rs`) carries all eight on the `$filter` side.
`AggregationDimension`, the `group_by` type, carries `TenantId`, `ResourceId`,
`ResourceType`, `SubjectId`, `SubjectType` and `Metadata(MetadataKey)` — five
fixed dimensions and the metadata escape hatch. `entry_type`, `origin` and
`invalidates` have no variant, so no caller can group by any of them.

**This is unbuilt scope, and slice 5 widened the gap rather than opening it.**
`entry_type` and `invalidates` were already missing when slice 4 put them on
the filter surface in `e6ede155a`; slice 5 added `origin` to `UsageRecordQuery`
and to
`KEYSET_SAFE_RECORD_FIELDS` and did not add it to `AggregationDimension`, so
the shortfall went from two dimensions to three.

**Load-bearing.** `origin` is the case that makes it concrete, and the ADR
states the consumer need itself: a consumer that has already raised a charge
for a period needs to separate imported history from current consumption. On
the raw path they can. On the aggregate path — the one a billing run actually
uses, because it folds server-side — they cannot group by it, and the fold
silently mixes backfilled history into the live bucket. Filtering `origin eq
'live'` and re-running for `'backfill'` is the workaround, and it doubles the
query count while nothing in the contract says it is necessary.

**Proposed resolution:** either grow `AggregationDimension` the three variants
— `origin` first, since it is a stored non-null column on every entry and needs
no plugin materialization, unlike `entry_type` — or split DESIGN's row so
`$filter` and `group_by` state their sets separately and the aggregate surface
stops advertising three dimensions it does not have.

---

## 16. The TimescaleDB plugin's filter allowlist cannot serve the published `$filter` field set

`gears/system/usage-collector/docs/usage-collector-v1.yaml:440`, the `$filter`
parameter, names eight fixed fields: `tenant_id`, `resource_id`,
`resource_type`, `subject_id`, `subject_type`, `entry_type`, `origin`,
`invalidates`.

**Read this as a note for the port, not as a live fault.** The plugin is not a
workspace member — it is absent from the root `Cargo.toml` `members` list, and
`cargo check -p cf-gears-timescaledb-usage-collector-plugin` answers "did not
match any packages". It has not compiled since slice 4 removed
`UsageRecordStatus` and `corrects_id` from the SDK: `infra/storage/mapper.rs`
alone still names them ten times. Nothing is serving `$filter=origin` and
failing, because nothing is serving anything. What follows is one item on the
list that port has to work through, recorded while the reason for it is fresh.

**What the code says.** `record_column`
(`gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/query/translate.rs:55`)
is the closed allowlist every `$filter` conjunct has to pass, and it maps `id`,
`created_at`, `tenant_id`, `resource_id`, `resource_type`, `subject_id`,
`subject_type`, `corrects_id` and `status`. Its own doc comment calls that
"these nine identifiers".

Read against the published surfaces it is wrong in both directions.

**Three of the nine no longer exist on the model.** `created_at`,
`corrects_id` and `status` were removed by slices 3 and 4 — the time model
replaced `created_at` with the `window_start` / `window_end` pair, and the
append-only correction model replaced `corrects_id` / `status` with
`invalidates`. Any `$filter` naming one now passes the plugin's allowlist and
fails against the table.

**Five identifiers the gear needs are missing.** Three are published `$filter`
fields — `entry_type`, `origin` and `invalidates`, leaving the allowlist
covering five of the contract's eight. The other two are `window_start` and
`window_end`, which are reserved on `$filter` but sit in
`KEYSET_SAFE_RECORD_FIELDS` and have to resolve to a column for the canonical
`(window_end, id)` keyset to render at all. Of the seven admissible `$orderby`
names the allowlist maps four.

**The plugin is the deficient side, and it fell behind before this slice.**
Slice 4 removed `corrects_id` and `status` from the model without following
them into the plugin — the same change that stopped it compiling; slice 5 added
`origin` to the filter surface, to `KEYSET_SAFE_RECORD_FIELDS` and to the
published parameter, and likewise did not. This slice touched no file under
`plugins/timescaledb-usage-collector-plugin/` by instruction, so the gap is
recorded rather than fixed.

**Load-bearing**, in the same conditional way entry 11 is: the consequence
lands on whoever does the port, not on a running system. A port that brings
this allowlist across unchanged — the natural thing to do, since it compiles
against nothing that changed here — serves an `Internal` for
`$filter=origin eq 'backfill'`, a predicate the published contract names and
the gear's own `reject_reserved_filter_fields` guard admits: a `500` for a
valid request. Ordering by `origin` fails the same way, and `origin` is the
one keyset-safe field slice 5 added. Compiling again is necessary and not
sufficient, which is the whole reason to write it down.

**Proposed resolution:** a plugin slice. Bring `record_column` to the model
that exists — drop the three dead columns, add `window_start`, `window_end`,
`invalidates` and `origin`, and materialize `entry_type` as the stored
generated column over `invalidates` that `usage-collector-sdk/src/models.rs`
already describes. The plugin's own `RECORD_COLUMNS` const, and the
`UsageRecordRow` it decodes positionally into, still name `value`,
`created_at`, `corrects_id` and `status` and belong in the same pass.

---

## Not divergences — five things this branch owes someone else

None is a spec-owner decision, so none is numbered above. **A** and **B** were
found by slice 4's final review, after eight per-task review rounds had missed
them; **C** and **D** by slice 5's. All four reach someone outside this branch.

### A. `docs/api/api.json` is stale, and it will fail CI

The generated aggregate contract still advertises
`POST /usage-collector/v1/records/{id}/deactivate`, both `/usage-types` routes,
and `status` / `corrects_id` / `created_at` / `gts_id` on the DTOs. It is
**byte-identical to `main`**, so this is pre-existing debt from slices 1-3 plus
this slice — but it was recorded nowhere until now.

Slice 5 made it stale by one more operation in the other direction:
`POST /usage-collector/v1/records/backfill` is registered and served and is
absent from `api.json` entirely. So the regeneration now adds a route as well as
removing two, and `oasdiff` will report both.

Two consequences. `.github/workflows/api_contracts.yml` triggers on `**/*.rs`,
runs `make openapi`, and fails Phase 1 on any diff; this branch changes hundreds
of `.rs` files, so **it will go red**. Phase 2's `oasdiff` will then flag the
route and field removals as breaking and require the `breaking-api-acknowledged`
label — correctly, since routes and fields really are gone.

It is also **the last artifact anywhere on this branch that tells a consumer a
persisted entry can be mutated.** In code the append-only invariant is total
(verified against the SDK trait, the SPI, the five registered routes, every
service dispatch, and the absence of any `&mut UsageRecord`); the published
contract is the one place still saying otherwise.

The fix is `make openapi` plus a commit, and it was left undone deliberately: it
needs a build of the example server and a decision about the breaking-change
label, both of which belong to whoever opens the PR. Slice 6's re-enabled drift
tests will **not** catch it — they compare the registry against
`usage-collector-v1.yaml`, a different document.

### B. `uc_ingestion_records_total` renamed a label, and no trailer says so

Commit `6e72ef233` changed the label key `record_kind` → `entry_type` and the
value `compensation` → `invalidation`. It carries no `!` and no
`BREAKING CHANGE:` trailer, under a subject that reads as a feature addition.

This is worse for an operator than a deleted series. A deleted series goes
visibly flat; a renamed label leaves every existing `sum by (record_kind)`
silently collapsing into an absent-label bucket while the metric keeps
reporting. The sibling commit `547915dc1` trailered exactly that *lesser* case
for the two deleted deactivation instruments.

Whoever writes release notes and whoever owns the ingestion dashboards both need
this, and `git log --format=%s` plus trailers will not give it to them. It was
recorded here rather than fixed because amending the message means rewriting four
commits.

### C. Two traceability facts about the ingestion path, before anyone blames slice 5

**A pre-existing marker crossing.** In
`usage-collector/src/domain/service.rs` — the SPI-dispatch block, around
`:1053`-`:1073` — `inst-state-usage-record-validated` opens **before**
`inst-emit-record-accepted` and closes **before** it as well, so the two spans
cross rather than nest. Every marker in the block is paired, which is what a
balance check looks at; the traceability spec asks for well-formed nesting,
which this is not. It is present at `d0315e328`, the commit before slice 5
began, at `:999` / `:1004` opening and `:1023` / `:1029` closing — the same
crossing in a block slice 5 never touched. Recorded here because so much of
`service.rs` moved this slice that the next reader will assume it is fresh
damage. Fixing it means
reordering two `@cpt-end` lines and belongs to whoever owns the traceability
sweep.

**The backfill handler carries no flow or DoD marker, on purpose.**
`handle_backfill_usage_records` (`api/rest/handlers/usage_records.rs`) carries
exactly one `@cpt` pair — `inst-algo-attrib-receive-ctx`, on the
`Extension<SecurityContext>` extractor, which every route taking one is marked
with — and no `@cpt-flow` or `@cpt-dod` line at all. The batch body it delegates
to is already marked at the service, and the only obligation the route owns
beyond that body is the workload isolation of entry 12 — which is
unimplemented, so no marker may claim it. No feature file owns `fr-backfill` or
`adr-backfill-isolation` at all, so there is no DoD to mark against even if
there were something to mark. If a traceability tool expects every registered
handler to carry one, this will read as an omission; it is a deliberate absence
and the missing feature coverage is the thing to fix.

Related and *not* a divergence: the
`@cpt-dod:…-nfr-workload-isolation:p1` marker on `create_usage_records_inner` is
sound. The DoD it traces to (`docs/features/usage-emission.md:794`-`796`) is
about isolating the write path from the read-side and operator-side gateways,
which the code does satisfy. The backfill-vs-live gap is a different obligation,
from the ADR, and it is entry 12.

### D. The reason round-trip table cannot see a changed wire spelling

`usage-collector-sdk/src/reason_tests.rs` walks a `(constant, variant)` table
through `from_wire` / `as_wire`. Both sides of every assertion read the same
constant, so the table proves the two functions are inverses and nothing else:
change `FUTURE_WINDOW`'s **value** to `"FUTURE_WINDOWX"` and every test still
passes, while every client matching on the published spelling breaks. The
spellings are pinned externally by `usage-collector-v1.yaml` and by nothing in
this crate.

This is true of all 18 constants in `reason.rs`, not only the two slice 5 added,
and it predates the slice. All 18 have `value == identifier`, so a single
`stringify!` table pins every wire spelling at once in about six lines. It was
declined in slice 5 on scope grounds: pinning 2 of 18 makes the table
inconsistent about what it guarantees, and slice 6's reason-vocabulary pass owns
this vocabulary wholesale. It belongs at the front of that pass, before the
vocabulary moves.

### E. Wrong counts in the slice-5 handoff itself

Recorded for whoever writes the next handoff, since both were believed and acted
on before being checked:

- **"Four config keys, joining `UsageCollectorConfig`."** Slice 5 added
  **three**: `live_future_tolerance_secs`, `live_past_tolerance_secs` and
  `backfill_window_secs`, taking the struct from five keys to eight. The
  covered-period bounds are three, not four, and every place in the code that
  counts them says three. The governing table is §3.10 Configuration of
  `docs/superpowers/specs/2026-09-06-usage-collector-gateway-registry-owned-typing-design.md:263`,
  and it has six rows (`:269`-`:274`): the three above plus
  `metadata_size_cap`, `type_cache_ttl` and `type_cache_capacity`, which
  `UsageCollectorConfig` already carried. Six minus three already present is
  three. The four is neither number, and reading the table's row count as new
  work is the nearest thing to an explanation.
- **"The DESIGN rework dropped the `flow` / `algo` / `dod` / `state` /
  `component` marker categories, and a resolve-gate would fail crate-wide."**
  The first four resolve. Their ids live in `docs/features/*.md` and
  `DECOMPOSITION.md`, which is where this convention keeps them — not in
  `DESIGN.md`, which is where the handoff looked. `component` ids do still exist
  in `DESIGN.md` and are not a marker category in this code at all. Only the two
  compensation ids were genuinely stale, and slice 5's first task removed them.

## Evidence

Three dated lines, not one. The slice-3 line is what makes entries 1-5
checkable and the slice-4 line entries 6-11; overwriting either would strand
them.

**Slice 5 (record origin and backfill), verified at `a94d542cf` on
`usage-collector/implementation-change`, 2026-09-09:** **701 passed, 6
skipped** across the three usage-collector packages, from a 648-passed
baseline; `cargo check --workspace --all-targets` and `cargo clippy
--workspace --all-targets --all-features` clean, `cargo +nightly fmt` a no-op;
`cargo doc --no-deps` clean on `cf-gears-usage-collector-sdk` and the same 35
pre-existing warnings on `cf-gears-usage-collector`, none naming a symbol this
slice created, moved or deleted. The 6 skipped are the same `#[ignore]`d
OpenAPI drift tests. `git diff --stat main --
gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/` is
empty, so entry 16 quotes the plugin exactly as `main` has it.

Entries 12-16 were verified against the branch at that commit. The sweep
commits that add them change `DIVERGENCES.md`, seven doc comments across six
source files and one comment inside a test table, and no behaviour. Slice 5 touched no file under
`gears/system/usage-collector/docs/` either, so the quotations in 12-16 are the
documents as slices 1-2 left them.

Read the rustdoc warning count off its own "generated N warnings" summary line
rather than by counting lines that start with `warning:` — that summary line
starts with `warning:` and so counts itself, which is how three separate people
on this branch arrived at 36.

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

**Re-verified at the branch head** after the sweep's own review found four
accuracy defects in this file and one gap in the SPI: **648 passed, 6 skipped**,
`cargo check --workspace --all-targets` and `cargo clippy --workspace
--all-targets --all-features` clean, `git diff --stat main --
gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/` empty.
The three commits after `531227f45` change no behaviour.

Checked with `git rev-parse HEAD:<path>` against `git rev-parse main:<path>`
rather than `git diff --quiet`, which reported the three files identical and was
wrong. Worth knowing if you reach for the same check.
