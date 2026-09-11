# Spec divergences in the usage-collector

**Twenty-five** places where `usage-collector-v1.yaml`, `DESIGN.md`,
`DECOMPOSITION.md`, a file under `docs/features/` or a plugin's own documents
describe behaviour the code does not have — or, in entries 17 and 25's case,
fail to describe behaviour the code does have. The direction varies; the
disagreement is the subject.

**Every count in this preamble is a `grep`, and a reader should re-run it rather
than trust the numeral** — each one here has gone stale at least once. Entries:
`grep -c '^## [0-9]'`. Load-bearing markers: `grep -c '^\*\*Load-bearing'`.

The *governing* documents were left unedited on purpose. Correcting one is the
spec owner's call, not the implementer's. Two documents that govern nothing were
corrected rather than registered — the plugin's `README.md` (entry 23 says why)
and one SDK rustdoc paragraph naming a fold the enum no longer has — and both
are named where they belong rather than left for a reader to notice.

**Entries 1-5** came out of slice 3, the time-model slice
(`usage-collector/implementation-change`, 23 commits ending `74e3d4429`).
**Entries 6-11** came out of slice 4, the append-only correction-model slice
(the ten commits ending `9e36bbf6b`; entries 6-11 were added by the sweep
commit `4c7d338a1`, the file itself was first committed in `46274f6ba`, and the
corrections its own review found landed after that).
**Entries 12-16** came out of slice 5, the record-origin and backfill slice
(the thirteen implementation tasks ending `a94d542cf`).
**Entries 17-19** came out of slice 6, the errors-and-contract-gate slice (the
eleven implementation tasks from `9f64cf22f` to `540dfbba0`, this sweep aside).
**Entries 20-25** came out of slice 7, the TimescaleDB plugin port (the
eighteen tasks ending at this sweep). Six of them were handed to the sweep by
the tasks that found them, which is why they arrive together rather than one per
task.

Slices 6 and 7 also **closed** things recorded here rather than only adding to
them, and each is struck in place rather than deleted. Slice 6 struck entry
9(a) — `UsageCollectorError::NotFound` now carries a typed `NotFoundReason`, so
the metric label it blocked is emitted — and section D, where the reason table
now pins every wire spelling to its own identifier. Slice 7 struck **entry 16**
(the plugin's filter allowlist, resolved in code as its own proposed resolution
described), **§A** (`api.json` regenerated and committed) and **§G**
(`group-by-absent-dimension`, resolved by owner decision and implemented as a
presence guard). Not all are *wholly* closed, and each says at its strike what is
left and where the remainder lives.

In **fourteen of the twenty-five the code is the correct side** and the document
is imprecise or stale. Eleven are not that shape, and saying so matters more
than a tidy summary:

- **Entry 8** — the three documents agree with each other and the code
  implements none of it. Slice 6 built the plugin half — the published 28-digit
  quantity range is now exercised at its corners by a contract check — and the
  gateway half is still enforced by nothing. The entry is split, not resolved.
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
- **Entry 16** — the TimescaleDB plugin's filter allowlist named three columns
  the model no longer had and was missing five the gear needed. The plugin was
  the deficient side. **Resolved by slice 7**, which is the port that entry
  asked for, and struck in place.
- **Entry 18** — the document is right and the code is deliberately one variant
  short. `UsageCollectorPluginError` ships five of DESIGN §3.3's six, and the
  sixth signals a replay refusal on a feed the SPI does not declare. Landing it
  with the feed was a spec-owner decision, not an oversight.
- **Entry 19** — two of DESIGN §3.3's seven contract checks cannot be written at
  all. Neither side is wrong about behaviour; the SPI and the record model are
  missing the method and the field a check would have to read. Slice 7 made it
  concrete rather than conditional: two conforming backends now answer a
  `LATEST` tie differently, on the rule the blocked check would have pinned.
- **Entry 20** — no document is wrong. The `LATEST` fold has a bound nobody
  stated, driven by an input the caller chooses. It belongs to the plugin's
  §3.10 deployment guide by kind and **not** by that section's enumeration,
  which lists six mandatory statements and no memory bound — and the plugin
  publishes no such guide anyway.
- **Entries 21 and 22** — the SPI is right and the store cannot fully hold it.
  A hypertable `UNIQUE` must contain the partition column, so at-most-one
  invalidation is conditional on the gateway; and a cross-call collision aborts
  a `create_batch` whole where the SPI asks for per-row outcomes.
- **Entry 25** — nothing is stale. A registration step that no document names
  and no artifact performs stands between a fresh deployment and its first
  successful ingest.

**Twenty** are load-bearing rather than cosmetic, and each is marked below. The
numeral in this sentence read "Fourteen" through two slices in which the true
count was fifteen — the entry-8 split added one and nobody re-measured — which
is why the preamble now names the `grep` beside every count.
The sharpest is still entry 10, and slice 6 widened it: a client generated from
the published contract cannot submit a single record and cannot make an
aggregate request either — two independent `400`s on each of those two
operations — while on a third the emitted body is not an instance of the schema
the contract declares for it.

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
`order_mismatch` is arguably its right home and needs no new label.

**But read that recommendation against this entry's own standard before acting
on it.** `QueryErrorCategory::OrderMismatch` is **constructed nowhere** in
non-test code — the only occurrences in `usage-collector/src` are the variant
declaration and its `as_str` arm — so "emitted-capable and absent from the
documented set" overstates it in exactly the way this entry objects to for
`undeclared_field` and `missing_time_range`. Entry 3 says why independently: the
in-process path now takes a continuation's order *from the token* rather than
from the caller, so the condition `order_mismatch` names is no longer reachable.
Documenting it as things stand would add a **third** dead label to a row this
entry exists to clear of two. Either give it the unsound-order refusal a
producer, and then document it, or leave it undocumented until something emits
it. Recorded rather than resolved: this is a pre-existing overstatement in the
entry, not a code defect. And the
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

**A second, narrower instance, added by slice 6.**
`gears/system/usage-collector/docs/DECOMPOSITION.md:573`:

> Request bodies named for their operation (`CreateUsageRecordsRequest`,
> `QueryAggregatedUsageRecordsRequest`) are the exception to the suffix
> convention.

There is no `QueryAggregatedUsageRecordsRequest`. Slice 6 renamed it to
**`AggregationRequest`** in `usage-collector/src/api/rest/dto.rs`, so that the
component the runtime publishes and the component `usage-collector-v1.yaml`
declares are the same string — which is what `openapi_contract_tests`'
`contract_name` rule requires, and the reason the rename happened at all.
`CreateUsageRecordsRequest` still resolves, so the sentence is half true, which
is the harder kind to notice.

Folded in here rather than opened as an entry of its own, because this entry
already owns `DECOMPOSITION.md` and the file is edit-forbidden either way. The
same rename left a sibling claim in `docs/features/usage-query.md`; that one is
in entry 14, which owns `docs/features/`.

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
differing field in `field`. **The spellings are now settled and can be
enumerated verbatim.** This entry used to defer them, because slice 6 owned the
final reason-vocabulary pass and might rename; that pass has run and renamed
none of the four. It also pinned each of them: as of §D below, every wire
constant in `reason.rs` is asserted to spell its own identifier, so these four
strings cannot drift out from under an enumeration written against them.

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

## 8. The 28-digit quantity guarantee is pinned for a plugin and enforced nowhere

**This is the one entry where the code is the wrong side.** Three documents
agree with each other and the gear implements none of it. Slice 6 built half of
what was missing and the entry is **split** below rather than marked resolved:
the half that landed is a check on a *plugin*, and the half that is unowned is
the one the gear itself would have to do.

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

**What the code does.** Nothing in the *gear* checks any of it. `models.rs` and
`validation.rs` validate `ReasonCode` length, `IdempotencyKey` shape and the
metadata surface, and say nothing about `Decimal` magnitude or scale. The
yaml's `UsageQuantity` `pattern` caps the *fraction* at 28 digits on the wire
and bounds the integer part not at all, so even the schema does not enforce the
prose. Until slice 6 no test anywhere exercised a boundary value — not 10²⁸,
not 1×10⁻²⁸, not the negative half.

The sign half is fine and deliberately so: a negative quantity is an ordinary
measurement recording a real decrease, never a correction, and that is stated
in `models.rs`, `reason.rs` and the ADR alike. It is the *range* that is
unguarded.

Found while fixing slice 4's sign-rule doc — the same grep that showed nothing
rejects a negative quantity showed nothing checks its magnitude either.

**Pre-existing**, not introduced by the correction-model slice.

**The plugin half is closed.** Slice 6 wrote the DESIGN §3.3
`quantity-round-trip` check
(`usage-collector-sdk/src/contract/checks/quantity_round_trip.rs`). It persists
five corners of the published range — `9999999999999999999999999999` and its
negation, `0.0000000000000000000000000001` and its negation, and `42.500` for
significant trailing zeros — through `create_usage_record`, reads each back
through `list_usage_records`, and compares the **rendered** decimals rather than
using `==`, because `Decimal`'s `PartialEq` calls `42.5` and `42.500` equal and
a backend that normalises scale on write is one of the things the check exists
to catch. `quantity_fixture` additionally re-checks per corner that
`rust_decimal::Decimal` parses and renders the literal unchanged, so a corner
the carrier itself rounds fails as a finding about the contract rather than
passing vacuously.

**The gateway half is unowned, and slice 6 deliberately left it that way** — on
this entry's own instruction. Nothing rejects an out-of-range *submission* at
ingestion. §3.3 lists "quantity range" (`DESIGN.md:1084`) among the gateway
responsibilities a plugin may therefore skip, so a plugin author is entitled to
assume the value already fits before it reaches the SPI.

**Load-bearing**, and unmarked until now only because nobody wrote the marker —
splitting the entry is the moment to settle it, in either direction, rather than
leave the file's self-declared most severe entry the one with no verdict. The
argument is the gap between two ceilings. The published range stops below 10²⁸;
the carrier every surface uses, `rust_decimal::Decimal`, holds up to
`79_228_162_514_264_337_593_543_950_335` (~7.9×10²⁸). The wire `pattern` bounds
the integer part not at all, so a quantity in that band deserializes, passes
every check the gear applies, and is dispatched to a plugin that §3.3 excused
from checking it. What happens next is the backend's column, not the contract's:
one store rejects it as an `Internal`, another rounds it, a third takes it. So
the same submission gets three answers across three conforming backends, and the
ledger is append-only — nothing corrects the admitted entry afterwards except an
invalidation naming a value the caller never sent. That is a silently wrong
charge, which is the consequence class entries 11 and 12 are marked for.

**A contract check proves a plugin round-trips the range. It proves nothing
about what the gear admits.** The check runs against a backend through the SPI
and never through the gear, so the two halves cannot substitute for each other
in either direction: a conforming plugin still stores whatever the gateway hands
it, and the gateway still hands it anything the wire `pattern` admits. Reading
the closed half as closing the entry is the specific mistake this split exists
to prevent.

The instruction stands for the next slice too, until a spec owner rules on where
the bound is enforced: a gateway check is a new rejection on the ingestion path
and needs a wire reason, an `error_category` (see entry 9(b), which records that
`RecordErrorCategory` has no `Validation` variant to route one to), and a
published range the yaml's own `pattern` does not currently contradict.

---

## 9. `uc_ingestion_records_total`'s label row does not match what the gear emits

`gears/system/usage-collector/docs/DESIGN.md:1779`, §3.11.5:

> `outcome` (`accepted`, `duplicate`, `rejected`), `entry_type` (`record`,
> `invalidation`), `origin` (`live`, `backfill`), `error_category` (`none`,
> `authz`, `unresolved_type`, `validation`, `idempotency_conflict`,
> `invalidation_rule`, `plugin_error`) … `invalidation_rule` covers the copy,
> reference and at-most-one rules alone.

**One problem is left on this table row. There were three.** `origin` was not
emitted at all, and slice 5 closed it: `record_ingestion_record` now takes the
fourth label and `RecordOrigin` supplies it on both ingestion paths. **(a)** is
closed by slice 6. Both are struck rather than rewritten, because nothing about
either survives.

**~~(a) `invalidation_rule` cannot cover the reference rule.~~ Closed in code.**
`UsageCollectorError::NotFound` now carries a typed `NotFoundReason`
(`usage-collector-sdk/src/reason.rs`), and `classify_record_error`
(`usage-collector/src/domain/service.rs`) matches on it rather than on prose:
`DeclarationNotFound` routes to `unknown_usage_type`,
`InvalidationTargetNotFound` to **`invalidation_rule`** — the ADR's
valid-reference rule, which is one of the three the document assigns this label
— and `UsageRecordNotFound` to `semantics_violation`. Nothing parses `name` as
a UUID any more, and nothing matches a substring of a caller-facing string. The
label now covers the copy, reference and at-most-one rules exactly as
`cpt-cf-usage-collector-adr-append-only-invalidation` and §3.11.5 describe it,
so the correction backlog no longer under-counts by a rule.

**The 404s are still indistinguishable to a client, and closing that is not
this gear's to do.** `NotFoundReason` is deliberately **not** a wire
vocabulary: it has no `SCREAMING_SNAKE` constants, no `from_wire` / `as_wire`,
and never reaches a `Problem` body, and `reason.rs` says so at the type. An
in-process consumer resolving `UsageCollectorClientV1` through `ClientHub`
reads the discriminator directly; a **REST client still separates the three
404s by `detail` prose and nothing else**, exactly as before slice 6.

The blocker is one level above this gear, and naming it precisely matters
because the obvious place to look is the wrong one.
`toolkit_canonical_errors::NotFoundV1`
(`libs/toolkit-canonical-errors/src/context.rs:186`) is `pub struct NotFoundV1
{}` — an **empty, platform-shared** context struct, which the canonical builder
constructs with no reason at all. Every gear's 404 is in that same position, so
a slot has to exist there before any gear can fill one. It is **not** this
gear's yaml: `Problem.context` at
`gears/system/usage-collector/docs/usage-collector-v1.yaml:1237` is already
`additionalProperties: true` and its description already names `reason` among
the common fields, so the schema forbids nothing. Reading the yaml alone
suggests the work is done. It is not, and it is a platform change, not an
editorial one — which is why this entry is struck at (a) and not closed.

**(b) Two of the seven listed label values are never emitted, and three emitted
values are absent from the list.** Open, and slice 6 declined to close it from
the code's side — see *"Slice 6 declined the rename"* below.
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

**Slice 6 declined the rename, and this entry's own argument is why.** Slice 6
owned the error-vocabulary pass and could have moved `semantics_violation` to
`validation` in an afternoon; it renamed nothing and added no variant — the
type's variants and `as_str` are byte-identical across the slice. (Its doc
comments are not: `7cce8db2e` rewrote the ones on `SemanticsViolation` and
`InvalidationRule`, the two whose meaning 9(a)'s closure changed. The label
vocabulary is what did not move.) `semantics_violation`
is an **emitted** series, so a rename does not correct a dashboard, it silently
empties one: every `sum by (error_category)` keyed on it collapses into an
absent-label bucket while the metric goes on reporting. That is the exact
failure §B below records for `record_kind` → `entry_type`, on this same
instrument, and it is worse for an operator than a deleted series because a
deleted series at least goes visibly flat. The document is the side to move.

That paragraph was once headed "Why the code is right **on (a) and (b)**",
while there were three sub-items and the qualifier existed to exclude (c) — the
one place the code was not right, because it had not been written yet. With (c)
struck the qualifier named every remaining sub-item, so it excluded nothing
while still reading as though something were excluded, and it was dropped
rather than re-scoped. It stays dropped now that (a) is struck too: (b) is the
only sub-item left, it is a document defect, and the residual 404 gap is a
platform gap argued at its own paragraph above rather than here.

**Load-bearing.** An operator building an alert off this row alerts on two
labels that can never fire, misses three that do, and — following the row's own
sentence — looks for period-bound rejections under a label that is never
emitted while they accumulate under `semantics_violation` beside unrelated
failures. The correction-backlog under-count is no longer among the
consequences: as of slice 6 `invalidation_rule` covers all three rules the
document assigns it.

**Proposed wording:** replace the `error_category` enumeration with the eight
values `RecordErrorCategory` actually emits. The `invalidation_rule` sentence
now needs **no** change — the second option this entry used to offer, growing
`NotFound` a typed reason, is the one slice 6 took, and the sentence is true as
written. Rewrite the period-bound sentence to name the category the bounds
actually reach, or give them one; the `origin` column needs no change either.
The rename in the other direction — code to document — is argued against above
and should not be read out of "replace the enumeration": the enumeration is the
document's, and it is the document that moves.

---

## 10. The REST shapes and `usage-collector-v1.yaml` are incompatible, not merely divergent

Every other entry here is a document that describes working code imprecisely.
This one is a contract a generated client cannot use at all — on three
operations now, not two. Slice 6 added the third.

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

**Aggregate request — a third case, on a third operation, found by slice 6.**
`gears/system/usage-collector/docs/usage-collector-v1.yaml:1045` declares
`AggregationRequest` with `required: [gts_type_id, time_range]` and
`additionalProperties: false`, carrying `gts_type_id`, `time_range`, `filter`,
`metadata_filter` and `group_by` as body properties. `AggregationRequest` in
`usage-collector/src/api/rest/dto.rs` carries `#[serde(deny_unknown_fields)]`,
exactly two fields — `time_range` and `group_by` — and **no `gts_type_id`**:
the gear takes the meter as a mandatory query parameter
(`api/rest/routes/usage_records.rs`, on `POST
/usage-collector/v1/records/aggregate`). So a client generated from the
contract puts `gts_type_id` in the body, where `deny_unknown_fields` refuses it
as unknown, *and* omits the query parameter the gear requires. **Two
independent `400`s on every aggregate request**, the same shape as the request
side above on a different operation. `filter` and `metadata_filter` disagree
the same way and are excused for the same reason.

The component *name* matches on both sides — slice 6's rename
(`b353c27fc`, `8b30a288d`) is what made it match, so `AggregationRequest` is
one string in the yaml and in the registry. That is what the drift gate
compares, and it is why the gate is green over an incompatibility.

**Nothing in the drift gate catches this, by design, and re-enabling the gate
did not change that.** `body_schemas_match`
(`usage-collector/src/api/rest/routes/openapi_contract_tests.rs`) compares
media type, `required`, and the referenced schema *name*; the module header
lists "field-level schema contents" among what it deliberately does **not**
enforce. Two schemas can therefore agree on name and required-ness and disagree
on every property. Slice 6 turned the gate from six `#[ignore]`d checks to zero
skipped, and this entry is the standing reminder that a green gate is not a
compatible contract — do not write the re-enablement up as though it closed
anything here. What the suite does instead is *record* the disagreement:
`BODY_VS_QUERY_DRIFT` names all three inputs with their two spellings, and
`body_vs_query_drift_is_really_drift` expires a row from either side, so the
list cannot become a standing exemption.

**Two gaps in the gate itself, recorded here rather than only in code
comments.**

- **The cross-check that keeps a placement disagreement out of
  `UNDOCUMENTED_PARAMETERS` matches on the property name alone.**
  `undocumented_parameters_are_really_undocumented` asserts that a row's
  parameter is not also declared as a body property of the same operation —
  which is what stops a client-breaking placement disagreement being filed as
  the weaker "documentation gap" and losing the `body_property` column that
  expires it. It compares names, so the `$filter` row is the case it **provably
  cannot police**: the query spelling is `$filter` and the body spelling is
  `filter`, so re-filing that row from `BODY_VS_QUERY_DRIFT` into
  `UNDOCUMENTED_PARAMETERS` leaves the suite green. It is correctly filed
  today, and the assertion message says outright that a differently-named body
  property is invisible to it — which is the most a name-matching check can do,
  and the reason this is written down rather than fixed with a stricter
  assertion that would not be stricter.
- **This file carried an unpinned evidence line.** "Re-verified at the branch
  head … **648 passed, 6 skipped**" named no commit, unlike the three
  commit-pinned lines around it. It was true when written and false by the time
  slice 6 opened; slice 6 alone moved that count several times and took the
  skips to zero. It is pinned to `9e36bbf6b` in **Evidence** below rather than
  renumbered, because an unpinned count re-breaks on the next commit — which is
  presumably why the others are pinned.

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

`acceptance_sequence` now exists, and not where this entry needs it: the
TimescaleDB plugin assigns it in its own `usage_records` table, strictly
monotonic per `(tenant_id, gts_type_id)`, which discharges DESIGN §3.7's
*storage* obligation and nothing else — the SDK's `UsageRecord` still carries
no such field and the published response shape still cannot emit one, so this
entry is open in exactly the terms it was written in. `accepted_at` has not
moved at all.

**Load-bearing**, more decisively than any other entry here: no generated
client works at all, on either write path — ingestion or aggregate.

**Proposed resolution:** this one is not a wording fix. Either the code renames
`value`, grows the two remaining server-assigned fields and moves the three
aggregate inputs into the body, or the contract is corrected to the shape the
gear serves. On the aggregate case the contract looks like the side that is
wrong: `$filter` reaches the handler through `toolkit_odata`'s query-string
extractor, so honouring the body placement means abandoning that extractor.
Whichever way it goes it is a spec decision plus a scheduled slice, not an
editorial pass.

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

**A second file under `docs/features/`, added by slice 6.** This entry is
titled for `usage-emission.md` because that is where it started; it is the entry
that owns `docs/features/` and the instance below belongs to it rather than to a
new number.

`gears/system/usage-collector/docs/features/usage-query.md:127`, inside §1.6
"Implementation Status" — the subsection whose whole job is to say what is
*actually* built:

> The implemented body is `QueryAggregatedUsageRecordsRequest`, carrying only
> `op` and `group_by` … The schema names that once appeared in these sections —
> `AggregationRequest`, `AggregationSpec`, `MetadataFilter` — no longer exist in
> `usage-collector-v1.yaml`.

Both halves are now false, and in opposite directions. The implemented body is
**`AggregationRequest`** — slice 6 renamed it so the registered component name
and the contract's would be one string — and it carries `time_range` and
`group_by`, not `op` and `group_by`, because the fold is resolved from the
queried type's declaration and no request names an operator at all. Meanwhile
`AggregationRequest` and `MetadataFilter` **do** exist in
`usage-collector-v1.yaml`, at `:1045` and `:1009`, so the sentence retiring
those names is retiring one the code has just adopted. Only `AggregationSpec`
is genuinely absent from the yaml.
`:139` restates the same claim inside the aggregated-read bullet, alongside the
retired `created_at` window model entry 5 covers, so it is two sites in one
file.

**Two sites is what this entry said, and the file is stale wholesale.** The
register is the index a reader trusts for what is stale, so naming `:127` and
`:139` and stopping implied the rest of the file was sound. Measured on the
branch (`gears/system/usage-collector/docs/features/usage-query.md`, **951
lines**):

- The retired `(created_at, id)` keyset order appears on **14** lines — `:147
  :215 :232 :331 :333 :347 :349 :356 :453 :763 :786 :877 :932 :939`. The
  canonical order is `(window_end, id)`.
- `created_at` appears on **18** lines / **35** occurrences in this file, and on
  **62** lines / **93** occurrences across
  `gears/system/usage-collector/docs/`.
- The whole read-path description is pre-port. The mandatory time window is
  described as a `$filter` conjunct — `timestamp ge X and timestamp lt Y` on
  **16** lines, a third spelling matching neither the model's
  `window_start` / `window_end` nor the wire — and the gear now takes it as the
  mandatory typed `from` / `to` query parameters instead, so the rejection the
  file documents on **18** lines (`MISSING_TIME_WINDOW`) is not the one a caller
  gets. Beside it: `last_keyset` (**15** lines), `page_after` (**14**),
  `validate_cursor_against` (**17**), and a `status` filter field (**12**) that
  the append-only model deleted.

**Do not fix the file, and do not fix it line by line** — one current paragraph
inside a wholesale-stale document is harder to notice than a uniformly stale
one, which is the same rule entry 23 applies to the plugin's `docs/DESIGN.md`.
This paragraph exists so the register stops implying the file has two stale
paragraphs.

**Load-bearing for the same reason the rest of this entry is**, and slightly
worse: §1.6 exists precisely so a reader who distrusts the surrounding sections
has one place to trust, and it is the section that is wrong. Its sibling
instance in `DECOMPOSITION.md:573` is in entry 5.

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

## 16. ~~The TimescaleDB plugin's filter allowlist cannot serve the published `$filter` field set~~

**Resolved in code by the TimescaleDB port, and kept rather than deleted.**
`record_column` is the model that exists: the three dead columns are gone,
`window_start`, `window_end`, `invalidates` and `origin` are mapped, and
`entry_type` is the stored generated column over `invalidates` — the proposed
resolution at the foot of this entry, implemented as written. The plugin is a
workspace member again, `RECORD_COLUMNS` and `UsageRecordRow` name the current
columns, and the DESIGN §3.3 contract suite runs against a live container.
`$filter=origin eq 'backfill'` is served rather than answered with a `500`.

**What the resolution did not close is entry 19, which this entry sends a porter
to read.** A green suite covers six checks and two of DESIGN's seven remain
blocked; `latest-tie-break` is the one that matters to a backend author, and see
entry 19 for how this backend and the reference one now differ on it. The
original entry follows unchanged as the record of what was wrong.


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

**Read three other places before starting, because this entry is where a porter
arrives and none of them is on the way here.** Compiling again and passing the
allowlist work above is necessary and not sufficient. **Entry 19** says the
DESIGN §3.3 suite covers five of seven and names what the other two need, so a
green run is not a conformance certificate. **§F** says two of those five checks
are not independent — a backend selecting on `window_start` fails
`quantity-round-trip` for a reason that has nothing to do with decimals — and
that nothing in the suite exercises the SPI's keyset obligations, which is where
a port is most likely to be quietly wrong. **§G** names a grouping case where
this gear's reference answer and a natural SQL projection disagree, and which
answer is right is not yet decided.

---

## 17. `$top` and `metadata.<key>` are served on `GET /records` and documented nowhere

`gears/system/usage-collector/usage-collector/src/api/rest/routes/usage_records.rs`
registers both on `GET /usage-collector/v1/records` — `metadata.<key>` at
`:149` and `$top` at `:159` — each with its reasoning written at the
registration site rather than inferred here.

`$top` is declared by hand (`:104`-`:119`): DE0802 requires every `$`-prefixed
`OData` parameter to come through `OperationBuilderODataExt`, that trait offers
`with_odata_filter`, `with_odata_orderby` and `with_odata_select` and **no
`$top` method**, and #4422 bound `$top` on the wire —
`ODataParams.limit` gained `#[serde(alias = "$top")]` — without adding one. The
extractor therefore folds `$top` and `limit` onto one slot (`:153`-`:157`), and
publishing only `limit` would under-report the accepted surface. `metadata.<key>`
is declared because the raw read path really does accept repeated metadata
filters.

`gears/system/usage-collector/docs/usage-collector-v1.yaml` documents **neither**.
`limit` **is** documented — the `Limit` component parameter (`:469`-`:470`),
referenced from `GET /usage-collector/v1/records` at `:178` — which is what
makes the omission easy to miss: the page-size *concept* is in the contract
under the alias spelling, and the canonical OData spelling the gear equally
honours is not.

**Load-bearing.** A client generated from the contract cannot set a page size by
the canonical OData spelling — it gets `limit` or nothing — and does not know
the metadata filter exists at all, which on a meter with declared metadata
properties is the difference between a narrow read and pulling the whole range
and filtering client-side. Neither failure is loud: `$top` is simply absent from
the generated surface, and an undeclared `metadata.<key>` is a feature nobody
discovers.

**Pinned, and the pin expires itself.** `UNDOCUMENTED_PARAMETERS` in
`usage-collector/src/api/rest/routes/openapi_contract_tests.rs:164` names
exactly these two rows, and
`undocumented_parameters_are_really_undocumented` fails on any row whose gap has
closed — including the day the yaml documents the name in *any* parameter
location, deliberately stricter than the row needs. So this entry expires
automatically when the contract catches up; nobody has to remember to delete it.
The list structurally excuses **registered-but-undocumented query parameters
only**. The reverse direction — documented but unregistered — is the dangerous
one and is never excused there.

**Proposed wording:** document both on `GET /usage-collector/v1/records` —
`$top` as an integer page size beside the existing `Limit`, noting that the two
are aliases folded onto one slot and that sending both in one request is
rejected, and `metadata.<key>` as a repeated query parameter with the OR-within-
a-key / AND-across-keys semantics the registration already publishes.

---

## 18. `UsageCollectorPluginError` ships five of DESIGN §3.3's six variants

`gears/system/usage-collector/docs/DESIGN.md:1217`-`:1224` tabulates the SPI
taxonomy and the envelope each variant lifts to, then states the count outright
at `:1226`:

> Six variants, deliberately.

**What the code does.** `UsageCollectorPluginError`
(`usage-collector-sdk/src/error.rs:865`) declares five: `Transient`,
`IdempotencyConflict`, `UsageRecordNotFound`, `AlreadyInvalidated` and
`Internal`. The absent one is
`CursorBeyondRetention { oldest_available }` → `InvalidArgument(CursorBeyondRetention)`
(`DESIGN.md:1224`).

**This was a spec-owner decision, not an omission.** The missing variant is the
**feed's replay-refusal signal**: a consumer resuming from a cursor older than
the retention floor. The feed is not on this gear's SPI — the same absence entry
19 records as blocking `feed-snapshot-and-replay` — so nothing constructs the
variant, nothing lifts it, and no test can exercise it. Its `error_category` has
the same shape: `cursor_beyond_retention` is a label on
`uc_feed_requests_total` (`DESIGN.md:1781`, and the alert built on it at
`:1837`), an instrument this gear does not emit at all. Slice 6 owned the plugin
error taxonomy and chose to land the variant **with the feed** rather than ship
a public enum arm that nothing raises and no assertion covers.

**Not load-bearing today**, and the reason is worth stating rather than leaving
to inference: nothing can raise it, so nothing mis-reports. A plugin author
cannot be misled into thinking they must construct it either — `#[non_exhaustive]`
on the enum means adding it later is not a breaking change for a matcher, which
is exactly what makes deferring it safe.

**It becomes load-bearing the moment the feed lands without it.** A feed that
cannot say "your cursor is past the retention floor" in the taxonomy says it as
`Internal(detail)`, which lifts to a `500`: the consumer retries, gets the same
`500`, and an operator reads infrastructure failure where the truth is a
consumer falling behind. Whoever builds the feed owns this row.

**Proposed resolution:** none for the document — DESIGN is right and says
"deliberately". The record here is that the code is knowingly one variant
behind, so that the next reader of `DESIGN.md:1226` does not count five in
`error.rs` and file it as drift, and so that the feed slice inherits the
obligation in writing.

---

## 19. Two DESIGN §3.3 contract checks cannot be written against the SPI this gear declares

`gears/system/usage-collector/docs/DESIGN.md:1105`-`:1116` tabulates seven
plugin contract tests and says every conforming plugin MUST pass the suite in
`usage-collector-sdk`. Slice 6 built that suite. **Five of the seven are
written**; two cannot be written at all.

**What the code says, and where.** `BLOCKED_CHECKS` in
`usage-collector-sdk/src/contract.rs:227` is the in-code record, carrying each
blocked name with what unblocks it:

- **`feed-snapshot-and-replay`** — the gear's SPI declares no feed method.
  DESIGN §3.3 gives `UsageCollectorPluginV1` a `read_feed_page`; this gear
  implements five methods and none reads a feed. Unblocked by the usage feed.
  This is entry 18's blocker seen from the other side.
- **`latest-tie-break`** — the check would assert *greatest `window_end`, then
  greatest `acceptance_sequence`* (`DESIGN.md:1116`), and `UsageRecord` carries
  no `acceptance_sequence` field. Until it exists there is nothing for a plugin
  to assign or a fold to read.

The split is not prose that can drift: `IMPLEMENTED_CHECKS`, `UNWRITTEN_CHECKS`
(empty — everything the current SPI can express is written) and `BLOCKED_CHECKS`
are asserted to partition DESIGN's seven exactly, and `ADDITIONAL_CHECKS` — one
check DESIGN states as an obligation without tabulating — is asserted disjoint
from all three, so a name DESIGN never wrote cannot be smuggled into the
partition and a check that half-lands fails it.

**This is the second slice to stand next to the same hole, and saying so is the
point.** `accepted_at` and `acceptance_sequence` are already recorded in
**entry 10** as blocking a conformant storage plugin — DESIGN §3.1 has the gear
stamp one and the plugin assign the other monotonically per
`(tenant_id, gts_type_id)`, and `usage-collector-sdk/src/models.rs` documents
the `LATEST` tie-break against a field the record does not carry. Slice 5
flagged it and did not fill it; slice 6 flagged it again from the contract-suite
side and did not fill it either. **Two flags, one problem.** A third reader must
not open a third entry.

**The same root cause reaches the suite's own behaviour, not just its
coverage.** The reference backend's `LATEST` fold breaks a `window_end` tie on
the **greatest `id`**, because the declared tie-break field does not exist. It
is documented at the fold
(`usage-collector-sdk/src/contract/reference.rs`, `fold_value`) and in that
module's "Stated limits", and it says what it is: a total order is needed there
or the answer would depend on ledger insertion order, so `id` stands in — it is
deterministic, it is **not** the declared rule, and no check asserts either way.
So the suite silently ships one substituted semantic, on the exact rule the
blocked check would have pinned.

**A second backend has landed since, and it sharpens the entry rather than
resolving it.** The TimescaleDB plugin assigns `acceptance_sequence` in its own
table, so its `LATEST` fold orders on `(window_end DESC, acceptance_sequence
DESC)` — **DESIGN's declared tie-break exactly**. The reference backend cannot:
the field is absent from `UsageRecord`, so it substitutes the greatest `id`. Two
conforming backends now give **different answers** on a `window_end` tie over
the same ledger, and the check that would have caught it is the blocked one. The
substitution was disclosed before there was a second backend to disagree with;
now there is.

**Load-bearing**, in the same conditional way entries 11 and 16 are: the
consequence lands on whoever ports a backend, not on a running system. "Run this
suite" is the acceptance criterion for a port, so a green run that covers five
of seven must not read as a conformance certificate — which is why the suite
exports all four constants and why its docs say a caller reporting coverage must
report them together rather than `IMPLEMENTED_CHECKS` alone. A porter who reads
the green and ships a `LATEST` fold with its own arbitrary tie-break produces
answers that differ from another conforming backend's on the same ledger, with
nothing failing anywhere.

**Proposed resolution:** none is available as a wording fix, and none should be
attempted as one. `latest-tie-break` unblocks when `acceptance_sequence` exists,
which is entry 10's contract decision; `feed-snapshot-and-replay` unblocks when
the feed lands, which is entry 18's slice. Until then the honest statement is
the one the code already makes: seven tabulated, five written, two blocked, with
the blocker named per check.

---

## 20. `LATEST` has an unbounded server-side allocation driven by caller input

The TimescaleDB plugin's `Latest` fold is
`(ARRAY_AGG(r.value ORDER BY r.window_end DESC, r.acceptance_sequence DESC))[1]`
(`plugins/timescaledb-usage-collector-plugin/src/infra/storage/query/aggregate.rs`,
`LATEST_SELECT_EXPR`), which materializes a group's values before picking one.
`aggregate_limit_clause` in the same module gives **zero** protection against
it: that clause bounds the number of *groups* (`LIMIT MAX_AGGREGATION_BUCKETS +
1`) and never the rows within one. The only bound on rows in a group is the
gateway-enforced `from` / `to` covered-period window — **a request parameter**.
`MIN` / `MAX` / `SUM` / `COUNT` carry no such cost.

**This is a limit to publish, not a question to weigh.** Gear DESIGN §3.10
requires each plugin crate's deployment guide to publish that plugin's actual
profile — "**Every guide MUST state:**", followed by six items. A memory bound
is **not** one of the six; they are consistency, freshness, retention and
throughput statements. So this belongs to that guide by kind and not by the
enumeration, which matters only in that nobody can be held to it today: the
plugin publishes no such guide, and the document that claims the role is the one
entry 23 registers as stale wholesale.

**Measured, on `timescale/timescaledb:2.29.2-pg18` (`PostgreSQL` 18.6).** At the
image's *tuned* settings, not `PostgreSQL`'s compiled defaults: the image runs
`001_timescaledb_tune.sh` at initdb, so a fresh container reported
`work_mem = 7837kB` and `shared_buffers = 1959MB` on the measuring host, and no
`SET` was issued. **Read the deltas below, not the absolutes.** Every RSS figure
is from one host and one fixture table, and peak RSS counts the shared buffers a
backend has touched — so against that 1 959 MB `shared_buffers` the absolutes
run an order of magnitude above what an independent replication saw (76.8 /
84.5 / 84.2 / 110.3 MB for the same four queries). **The differences reproduced
exactly, and the differences are the claim.**

1. **The planner never chooses a `HashAggregate` here.** An aggregate carrying
   its own `ORDER BY` takes the grouped node off the hash path entirely. With
   `enable_sort` *and* `enable_incremental_sort` off the plan is still
   `Sort → GroupAggregate` with the `Sort` reported `Disabled: true` — and a
   disabled node is chosen only when no alternative path exists, while
   `HashAggregate` was never disabled. The same statement with the inner
   `ORDER BY` dropped plans as a `HashAggregate` immediately; adding one ordered
   aggregate beside a plain `MAX` takes that query off the hash path too;
   `COUNT(DISTINCT …)` behaves identically; and with an index supplying the
   order and every scan method disabled the node is *still* `GroupAggregate`. It
   is a property of ordered and distinct aggregation generally, not of this
   expression, this data or this row count.
2. **So the peak is O(largest group), and it is real.** Exactly one array is
   live at a time. On the worst case for it — 1 000 000 rows in one group,
   parallelism off — peak backend RSS ran **+34 MB over `MAX(r.value)`** on the
   same rows (1 022.7 MB against 988.6 MB here; +33.5 MB in the independent
   replication), i.e. **≈34 bytes per row in the largest group**, reproducible
   to ±0.2 MB across runs. The array does not spill.
3. **The `Sort` beneath does scan-sized work, and is not this fold's cost.** It
   materializes the whole selection but is `work_mem`-bounded and spills rather
   than growing: `external merge`, ~10 MB in each of four workers under the
   image's default parallelism at 1 000 000 rows, and 41 MB as a single sort
   with `max_parallel_workers_per_gather = 0` (33 MB in the independent
   replication — fixture-dependent absolute, same shape). Every candidate
   formulation needs the same sort.

**Both alternatives were measured and both stay out.** On that single-group
worst case `DISTINCT ON` and `ROW_NUMBER() OVER (PARTITION BY …) = 1` both
peaked **~25 MB below** the shipped form (997.6 MB and 997.4 MB here; 25.8 MB
and 26.1 MB below in the independent replication), O(1) per group, with
execution times inside the run-to-run noise of the parallel plan (86-111 ms at
100 000 rows, 257-293 ms at 1 000 000, all three formulations). Neither is a
`SELECT`-list expression that composes beside `SUM`, and — the decisive fact —
**neither can express the ungrouped fold**: `DISTINCT ON ()` is a syntax error,
and `PARTITION BY` nothing, like the `ORDER BY … LIMIT 1` rewrite, answers
**zero** rows over an empty selection where the SPI owes exactly one
empty-keyed bucket. So `aggregate.rs` is unchanged and this entry publishes a
limit rather than a fix.

**Load-bearing.** An operator sizing a deployment from DESIGN §3.10 gets no
bound for this fold from any document, and the input that drives it — the
covered-period window — is chosen by the caller, not the operator. The same
correction is on `RecordStore::aggregate`'s rustdoc and on `LATEST_SELECT_EXPR`'s
and both ship; what did not exist until this entry is the register a reviewer
reads.

**A superseded claim, recorded because it was nearly published.** The
pre-measurement form of this entry said the peak was "O(rows scanned), not
O(largest group): under a `HashAggregate` plan every group's array is live at
once, and only a sorted `GroupAggregate` gives the weaker bound, the planner
chooses." Fact 1 above falsifies it. It is written down here so a reader who
met the earlier phrasing elsewhere can see it was retracted on a measurement
rather than quietly reworded.

**Proposed resolution:** none in the code. State the bound wherever the plugin's
§3.10 deployment guide eventually lives — today the only candidate is the
plugin's `docs/DESIGN.md`, whose traceability row at `:89` claims the
consistency-profile role for its §4, and entry 23 is why that file cannot carry
anything a reader would trust.

---

## 21. The at-most-one-invalidation guarantee is conditional on the Ingestion Gateway

The SPI says **the store** MUST reject a second withdrawal of one target, and
MUST make that check atomic with the entry it admits — in as many words, and
with the reason a gateway-side pre-read cannot substitute.

**What the store can enforce.** The mechanism is the partial unique index
`usage_records_one_invalidation_uniq` over `(invalidates, window_end)`
(`plugins/timescaledb-usage-collector-plugin/migrations/0001_init.sql`), claimed
and inserted inside the one transaction that also claims `acceptance_sequence`,
so the atomicity half is met.

**Why it cannot key on `invalidates` alone.** A hypertable's `PRIMARY KEY` and
every `UNIQUE` must contain the partition column. `window_end` is the partition
column, so **no hypertable-compatible index can key on `invalidates` alone** —
this is a property of the storage engine, not a choice in this schema.

**What that costs, measured on a live container.** Two withdrawals of one target
sharing the target's `window_end` are rejected. Two carrying **different**
`window_end` are **both accepted**. Conformance therefore rests on every
withdrawal being a faithful copy of its target's covered period — which the
Ingestion Gateway enforces upstream, and which the migration's own comment
states as though it were unconditional ("an invalidation is a faithful copy of
the entry it withdraws, so it shares that entry's covered period"). **A caller
reaching this SPI directly is not bound by it.** The residual guarantee lives at
the gateway; the entry exists so that nobody reads the index as the whole of it.

**A green contract run is not evidence the general case is covered.** Task 14's
`at-most-one-invalidation` check passes either way, because its fixture builds
every withdrawal from its target's own `window_start` / `window_end`
(`usage-collector-sdk/src/contract/checks/at_most_one_invalidation.rs`,
`at_most_one_fixtures`) — the shape the gateway admits, and the shape the index
catches. The check is right to use it; it just does not reach the case this
entry records.

**Load-bearing.** The SPI's obligation is on the store, and a second backend
author reading the SPI would implement it as written and conclude this one does
too. A direct SPI caller — another gear, a migration tool, a test harness — can
land two accepted withdrawals of one entry today, and the fold excludes a
withdrawn entry once regardless, so the ledger carries a contradiction nothing
reports.

**Proposed resolution:** a spec decision, not a code fix. Either the SPI's
obligation narrows to "the store MUST reject a second withdrawal *carrying the
target's covered period*", which is what a hypertable-backed store can hold and
what the gateway already guarantees, or the obligation stands and the SPI
declares that a conforming store may require a non-partitioned uniqueness
domain. Recorded at the call site in
`plugins/timescaledb-usage-collector-plugin/src/infra/storage/record_store.rs`
(`create_inner`'s rustdoc) as well as here.

---

## 22. A cross-call invalidation collision fails a `create_batch` whole

The SPI asks `create_batch` for **per-record outcomes aligned to input order**.
Two withdrawals of one target get that treatment only when they arrive
together.

- **Same call** — `plan_batch`
  (`plugins/timescaledb-usage-collector-plugin/src/infra/storage/record_store.rs`)
  pre-rejects all but the first, per row, and the other rows still commit. This
  is the SPI's shape.
- **A later call** — the collision is caught by
  `usage_records_one_invalidation_uniq`, which aborts the whole multi-row
  `INSERT`. The batch returns an outer
  `UsageCollectorPluginError::AlreadyInvalidated` instead of a per-row result
  vector, so every well-formed record travelling beside the offending one is
  refused with it.

**It was scoped out, not missed.** Fixing it needs a per-row `SAVEPOINT` pass —
each row's insert wrapped so its rollback does not take the statement with it —
which changes the batch's transaction shape and its cost, and belongs in a slice
that can measure the result. Recorded at `create_batch_inner`'s rustdoc as well
as here.

**Load-bearing.** A caller batching a day's emissions gets the whole batch
refused because one record withdraws something already withdrawn, and the outer
error names the one entry rather than the batch, so a naive retry of the whole
batch fails identically. The SPI's per-record contract is what a client author
builds retry logic against.

**Proposed resolution:** a plugin slice for the `SAVEPOINT` pass. Until then the
honest statement is the one the code makes at the call site: in-batch is per-row,
cross-call is whole-batch.

---

## 23. The plugin's own `docs/DESIGN.md` is stale wholesale, and no entry owned it

`gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/docs/DESIGN.md`
is **710 lines** describing the superseded model throughout: `gts_id` on 37 of
them, `catalog` on 30, `created_at` on 29, `usage_type` on 25, `corrects_id` on
10, `deactivate` on 7. Its §4 Observability table still lists
`uc_timescaledb_deactivate_duration_seconds` (`:630`),
`uc_timescaledb_usage_type_referenced_total` (`:652`) and
`uc_timescaledb_usage_type_catalog_size` (`:672`) — three instruments this crate
deleted — and describes `uc_timescaledb_compensations_total` as `corrects_id`-driven
(`:676`).

**The point of this entry is the ownership gap, not the staleness.** Entry 5
owns `gears/system/usage-collector/docs/DECOMPOSITION.md`; entry 14 declares
itself the entry that owns `docs/features/`. **Neither reaches the plugin's
directory**, and nothing else here did either, so this file was stale *and*
unregistered — the worse of the two states, because an unregistered document
has no reader who knows to distrust it.

**It is the nominal owner of three things it can no longer describe.** The gear
delegates, and this file is what claims the delegation — nothing else in the
tree does:

- Gear DESIGN §3.7: "Concrete table shapes are plugin-internal … each plugin's
  own DESIGN document owns them."
- Gear DESIGN §3.11.5: "Plugins may expose backend-internal metrics under their
  own prefix. Those series are owned by the plugin's deployment guide." This
  file's traceability row at `:90` claims exactly that role —
  `cpt-cf-uc-plugin-nfr-operational-visibility` → "OTel `uc_timescaledb_*`
  metric inventory (§4 Observability)".
- Gear DESIGN §3.10: "Each plugin crate's deployment guide MUST publish that
  plugin's actual consistency profile." The row at `:89` claims that one too —
  `cpt-cf-uc-plugin-nfr-consistency-profile` → "Single-node read-after-write
  ceiling; per-topology profile (§4, ADR-0011)".

Both claimed roles point at the same §4, and §4 is part of what is stale. The
register has to say that the file cannot currently be read as the owner of any
of the three. Entry 20's bound is a fourth thing with nowhere to go for the same
reason.

**Registered rather than fixed, on entry 14's own rule.** One current paragraph
inside a wholesale-stale document is harder to notice than a uniformly stale
one, so the §4 table was deliberately left alone rather than patched in place.

**The contrast that makes the rule legible.** The same slice *corrected* the
plugin's `README.md` — 52 lines with three wrong ones, including a **Note** that
declared an "intentional divergence from the SPI's 3-tuple contract" where the
shipped `usage_records_dedup_uniq` is the gear's DESIGN §3.7 5-tuple verbatim,
the opposite of a divergence. A mostly-right document is where a wrong line does
its damage, and is worth the edit; a uniformly stale one is worth a marker.

**Load-bearing.** An implementer or reviewer working the plugin's §4 table
builds three instruments that no longer have anything to measure and mis-keys a
fourth, and the file presents itself — via `:90` and via gear DESIGN §3.11.5 —
as the authority for exactly that.

**Proposed resolution:** a documentation slice rewriting the file against the
shipped model, not an editorial pass. Until it lands, the file needs a banner at
its head saying it describes the pre-port model; that banner is the smallest
change that does not create the mixed-staleness problem, and it is a spec
owner's to write. See entry 24 for the traceability ignore that is coupled to
this decision.

---

## 24. The `.cf-studio` ignore block's trigger has fired, and its stated reason is now half false

`.cf-studio/config/artifacts.toml:84-91` ignores the plugin's `docs/*`, `src/*`
and `tests/*` from traceability validation. Its reason:

> Ignore the TimescaleDB usage-collector storage plugin — its specs **and code**
> still describe the superseded model … pending the plugin's own update to
> aggregation folds and invalidation once the rewrite tracked in PRD section 13
> lands. Docs and code are ignored together: ignoring the specs alone would
> orphan the plugin's code traceability markers.

**That rewrite is this port.** The code half of the reason is therefore false:
`src/` and `tests/` were rewritten to the aggregation-fold and invalidation
model the block was waiting for. The ignore now suppresses validation of markers
that **are** current — `@cpt-flow:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1`
at `src/gear.rs:34`, for one.

**The decision, taken jointly with entry 23: the ignore stands as written.** The
block's final clause is its own answer. Narrowing it to `docs/*` would validate
the code's markers against specs that are still stale — which is entry 23,
unchanged, because that file was registered rather than rewritten. The ignore is
internally coherent for exactly as long as `docs/DESIGN.md` is stale, and not one
commit longer: **when entry 23 is resolved, narrow this block to `docs/*` in the
same change, or drop it.**

**Not load-bearing**, and marked so deliberately. Nothing is wrong today; a true
half and a false half currently reach the same correct outcome. What fails is
the *next* reader, who finds a reason that no longer describes the tree and
cannot tell whether the block is still wanted.

**This entry is routed here because it is otherwise ownerless.** Before it,
`grep -c 'artifacts.toml\|cf-studio'` over this port's plan returned **0** —
nothing anywhere would have brought a reader back to the file.

**Proposed resolution:** on its own, correct the reason text to say that the code
is current and the ignore now stands on the specs alone — the patterns unchanged.
The file is the studio tooling's, not this branch's, so the wording is proposed
and not applied:

> Ignore the TimescaleDB usage-collector storage plugin — its `docs/` still
> describe the superseded model (`DIVERGENCES.md` entry 23), while `src/` and
> `tests/` were brought to the current model by the PRD section 13 rewrite.
> Code is ignored with the docs rather than on its own merits: validating the
> plugin's code traceability markers against stale specs would fail them.
> Narrow to `docs/*` when entry 23 is resolved.

---

## 25. Nothing seeds `gts.cf.core.uc.usage_record.v1~`, so a fresh deployment meters nothing

A meter is a **derived GTS type** of `gts.cf.core.uc.usage_record.v1~`, and the
gear resolves it through `types-registry`. The abstract base itself is registered
by nothing:

- usage-collector declares no `#[gts_type_schema]` for it. Its only link-time
  type schema is the storage-plugin spec
  (`usage-collector-sdk/src/gts.rs`).
- No shipped config carries it in `gears.types-registry.config.entities` —
  checked by grepping every `*.yaml` / `*.yml` / `*.json` in the tree for the id,
  which finds it only in the gear's own `docs/schemas/` and
  `docs/usage-collector-v1.yaml`.

`types-registry` refuses a child whose parent is unknown, so until the base is
registered **every ingest is a 404 "GTS type … is not declared"** and no meter
can be declared either. The E2E suite works around it by posting
`docs/schemas/usage_record.v1.schema.json` itself before every meter
(`testing/e2e/suites/usage_collector/conftest.py`), which is a test fixture
standing in for a deployment step.

**The comparison that makes this a decision rather than a bug report.**
`config/quickstart.yaml:366-376` seeds the AM platform-root tenant type
(`gts.cf.core.am.tenant_type.v1~cf.core.am.platform.v1~`) exactly this way, under
`gears.types-registry.config.entities`. So the platform has a shipped idiom for
seeding an abstract root, and usage-collector does not use it.

**Load-bearing.** Out of the box the gear's whole ingest surface returns 404.
Anyone deploying it has to discover the obligation from a 404, and the document
that would have told them does not exist.

**What is recorded, since this one needs a decision and not only a description.**
Three options, and the register's job is to say the choice has not been made:

1. **Link time** — a `#[gts_type_schema]` on the gear for the base. Makes the
   base present wherever the gear is linked, with no operator step; also makes
   the gear the owner of a type its own `docs/schemas/` already publishes, which
   is where the schema would have to come from.
2. **`config/quickstart.yaml`** — the AM platform-root idiom above. Smallest
   change, and consistent with a shipped precedent; seeds only deployments that
   start from that config, so it fixes the demo path and not the general one.
3. **An operator obligation** — documented in the gear's deployment guidance and
   left to the deployer. Honest if the base is expected to be versioned
   independently of the gear binary; today it is documented nowhere, which is the
   state this entry is about.

**Proposed resolution:** option 1 or 2 is a spec-owner call; option 3 is only
tenable once it is written down. What must not stand is the current fourth
state — no seeding, no documentation, and a test fixture quietly covering for
both.

---

## Not divergences — seven things this branch owes someone else

None is a spec-owner decision, so none is numbered above. **A** and **B** were
found by slice 4's final review, after eight per-task review rounds had missed
them; **C** and **D** by slice 5's; **F** and **G** by slice 6's. All reach
someone outside this branch. **Three are struck** — **D** by slice 6, and **A**
and **G** by slice 7 — and each stands struck rather than deleted, for the same
reason the struck sub-items above do. **B**, **C**, **E** and **F** are open.

### ~~A. `docs/api/api.json` is stale, and it will fail CI~~

**Discharged by the TimescaleDB port's last task: `make openapi` ran and the
result is committed.** The three claims below were re-verified immediately
before the regeneration and all three held — `git diff --stat main --
docs/api/api.json` empty, `QueryAggregatedUsageRecordsRequest` present **twice**,
`AggregationRequest` **zero** times.

**The regeneration was larger than this section's inventory, and that is worth
recording here rather than leaving the next reader to rediscover it.** §A
predicted the removals and the rename, which is what a staleness note is for; it
did not predict the *additions* the same commits require, because it was written
from the side of what the document still says. Measured over the whole document,
the delta is confined to the usage-collector gear — no other gear's path or
schema moved — and is exactly:

- **Paths:** `+ /records/backfill`; `- /records/{id}/deactivate`,
  `- /usage-types`, `- /usage-types/{gts_id}`.
- **Schemas removed:** `QueryAggregatedUsageRecordsRequest` (renamed),
  `CreateUsageTypeRequest`, `UsageTypeDto`, `Page_UsageTypeDto` (the two deleted
  routes' bodies), and **`AggregationOpDto`** — the one §A gave no reason to
  expect, and it goes because the fold is resolved from the queried type's
  declaration, so no request names an operator.
- **Schemas added:** `AggregationRequest` (the rename's target) and
  **`TimeRangeDto`**, the mandatory covered-period range that request carries.
- **DTO fields removed:** exactly the four §A names — `status`, `corrects_id`,
  `created_at`, `gts_id` — and nothing else.
- **DTO fields added:** `gts_type_id`, `window_start`, `window_end`,
  `invalidates`, `reason_code` on both record shapes, plus `entry_type` and
  `origin` on `UsageRecordDto`.
- **Query parameters:** `gts_id` → `gts_type_id` on `GET /records` and
  `POST /records/aggregate`; `from` and `to` added to `GET /records` as
  mandatory, which is the time window leaving `$filter` (see entry 14).

No `$ref` is left dangling. The breaking-change label is still owed — see the
foot of this section — and now rests on fourteen `!` commits from slices 6-7
(`9f64cf22f..HEAD`) rather than the eleven the port's plan predicted. **The PR
goes against `main`, which is a wider set: thirty.**

```
git log --oneline $(git merge-base main HEAD)..HEAD | grep '!'
```

`git merge-base main HEAD` is `42e285e9a`; the plan's `9f64cf22f` base covers
slices 6-7 only. **The commits that earned the label are mostly in the other
sixteen** — the four that removed the very routes and fields this regeneration
deletes are `d027e2089` (remove the usage-type catalog), `547915dc1` (delete the
deactivation surface), `b3a3811fe` (carry the read-path time range as a typed
parameter) and `c8a51ea73` (carry the covered period and derive identity over
it), and not one of them is among the fourteen. Run the command rather than
reading a numeral out of this paragraph.

The original text follows unchanged.


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

Slice 6 made it stale by a **renamed component**, which is a third direction
again. `docs/api/api.json` carries `QueryAggregatedUsageRecordsRequest` — twice
— and the gear now publishes that request body as `AggregationRequest`
(`b353c27fc`, then `8b30a288d` for the reason the intermediate spelling was
wrong: `toolkit_macros::api_dto` registers a schema under the literal Rust
identifier, so a `Dto` suffix on the type is a `Dto` suffix on the served
component). So the regeneration renames a component as well as adding a route
and removing two.

The fix is `make openapi` plus a commit, and it was left undone deliberately: it
needs a build of the example server and a decision about the breaking-change
label, both of which belong to whoever opens the PR. It is still
**byte-identical to `main`** at `540dfbba0`, checked with `git rev-parse` on
both sides rather than `git diff`. The drift tests slice 6 re-enabled do **not**
catch any of it — they compare the registry against `usage-collector-v1.yaml`,
a different document — so turning that gate on changed nothing here, and this
section is still the only record.

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
`usage-collector/src/domain/service.rs` — the SPI-dispatch block,
`:1067`-`:1087` — `inst-state-usage-record-validated` opens **before**
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

### ~~D. The reason round-trip table cannot see a changed wire spelling~~

**Closed by slice 6, at the front of its reason-vocabulary pass, exactly where
this item asked for it.** The defect was real: `reason_tests.rs`'s round-trip
tables read the same constant on both sides of every assertion, so they proved
`from_wire` and `as_wire` are inverses and nothing else — changing
`FUTURE_WINDOW`'s **value** to `"FUTURE_WINDOWX"` left every test green while
every client matching the published spelling broke.

`every_wire_constant_spells_its_own_identifier` is the companion that closes it.
A `stringify!` macro builds a `(identifier, value)` table and asserts the two
are equal for every constant, so the value is now pinned to something other than
itself.

**The count moved, and the fix is why the count no longer has to be maintained.**
This item said "all 18 constants". There are **16**: slice 6 deleted
`INVALID_CURSOR` and `FILTER_MISMATCH` from `ValidationReason` in `7691b8222`,
because this gear originates neither — both are `toolkit_odata`'s, and the gear
now carries the upstream error and reads the wire `field` and `reason` off it
rather than re-spelling them (`reason.rs` says so where they used to be, and
`the_upstream_cursor_reasons_no_longer_model_themselves` pins that they are *not*
modelled here). Any 18 written down in this file was going to be wrong by the
end of the slice.

So the count is not written down. Coverage is derived instead: an
`include_str!("reason.rs")` scan counts the module's own `pub const`
declarations at compile time and asserts it equals the number of rows in the
table. **The pair is inseparable in both directions** — a constant added to
`reason.rs` without a row fails the count instead of going unpinned and
invisible, and a row added without a constant fails it too. A hardcoded length
could not do the first, since `pinned` is a fixed-size array and its length
always equals the rows written above it: the assertion would have been a
tautology. The scan is textual and line-anchored, so it over-counts a
`pub const`-shaped line at column zero inside a comment; that trade is
deliberate and documented at the assertion, because every miscount it admits
fails loudly and points at `reason.rs`.

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

### F. Two properties of the contract suite a porter has to be told

Neither is a defect and neither is a document divergence, so neither is numbered
above. Both are things an operator or a porter will get wrong if the suite is
handed over without them. Entry 19 records what the suite does **not cover**;
this records two ways the coverage it does have can be misread.

**Two checks are not independent, and the coupling points the wrong way.** A
backend that selects on `window_start` where the SPI says `window_end` fails
**both** `window-end-selection` and `quantity-round-trip`.
`quantity_round_trip`'s read-back range is `[window_end, window_end + 1s)`,
while each fixture's `window_start` sits an **hour** earlier
(`quantity_fixture`, `usage-collector-sdk/src/contract/checks/quantity_round_trip.rs`).
Such a backend therefore returns none of the five corners at all, and the
quantities cannot be compared *at all*. The check says so — its violation reads
*"record … was accepted by `create_usage_record` but a `list_usage_records`
range containing its `window_end` did not return it, so its quantity could not
be compared at all"* — but a `quantity-round-trip` failure is still the wrong
place to start looking, and at acceptance time an operator must not read it as a
decimal-fidelity problem. Both red means diagnose the period rule and re-run.
Decoupling would mean changing a check file and was out of slice 6's scope; the
coupling is a property of the fixtures, not a bug in either check.

**`the_reference_backend_conforms` passing means the backend satisfies *this
suite*, not that it is a conforming plugin.** Nothing in the suite exercises the
SPI's keyset obligations: `InMemoryReferencePlugin` serves the canonical
`(window_end, id)` ascending order, ignores `query.order`, and **mints no
`next_cursor`** — all three are in its own "Stated limits", and a real plugin
owes all three.

That gap is the strongest candidate for the next check, and the reason is
written in the gear rather than inferred here. `require_cursor_fingerprint`
(`usage-collector/src/domain/query.rs`) says carrying `query.filter_hash` into
`next_cursor.f` is *"the one requirement in this gear's Plugin SPI that gives an
implementor no compiler error — a plugin written before it recompiles clean and
paginates exactly once"*. The gear already spends a wire decode in the domain to
diagnose it one request early (`report_unbound_next_cursor`,
`usage-collector/src/domain/service.rs`) precisely because the compiler will not.
**An obligation with neither a compiler backstop nor a contract check is where a
suite is worth the most.** Recorded, not built: it is a new check plus reference
support for minting cursors, which is a slice, not a sweep.

### ~~G. `group-by-absent-dimension` is a spec question before it is a check~~

**Resolved by owner decision: drop the row.** It went to the owner in the form
the port's aggregation task reframed it, and the answer matches both
`InMemoryReferencePlugin` and the published wire shape, where
`AggregationBucket.key` types every item as a non-nullable string with no null
spelling available. `dimension_presence_guard`
(`plugins/timescaledb-usage-collector-plugin/src/infra/storage/query/aggregate.rs`)
implements it: a grouped dimension that can yield `NULL` gets an `IS NOT NULL`
built from the very expression the `GROUP BY` ordinal points at.

Three things about the section below, now that it is settled.

**The live case was metadata, not subject.** §G is written about
`GROUP BY subject_id`, and the two backends agreed on subject all along — the
plugin's aggregate already pushed `subject_id IS NOT NULL` and
`subject_type IS NOT NULL`. It pushed nothing for a metadata dimension, and
`InMemoryReferencePlugin`'s `bucket_key` reads a grouped metadata key as
`row.metadata.get(key).cloned()` (`contract/reference.rs`), so an absent key
yields `None` and the row joins no bucket. **The metadata dimension is the one
place the two actually differed**, and the presence guard closes it.

**"DESIGN says nothing about the case" understates what was already written.**
DESIGN is silent; the SDK was not. `AggregationDimension::SubjectId` and
`SubjectType` document the drop answer in as many words — "rows without a
subject are excluded from the grouping" (`usage-collector-sdk/src/models.rs`).
Of the six dimensions, three can be absent at all — `SubjectId`, `SubjectType`
and `Metadata`; the other three read `NOT NULL` columns and the case cannot
arise — so the SDK had already answered two of the three, and the unanswered one
was the one that mattered.

**The consequence is already stated below and is not re-reported as a gap.**
"Grouped buckets need not sum to the ungrouped total" stands where it is. What
is new is that it becomes **uniform rather than accidental**: it held for
subject because the caller happened to guard those two dimensions, not because
anyone had decided it should, and now it holds for every dimension because
someone did.

The original text follows unchanged.


A second candidate check, and this one cannot be written until someone decides
what the right answer is.

`InMemoryReferencePlugin` **drops** a row with no `subject_ref` from a
`GROUP BY subject_id` entirely — `bucket_key` returns `None` and the row joins
no bucket — where naive SQL would collect those rows into a NULL group. The
consequence is concrete: grouped buckets need not sum to the ungrouped total, so
an exemplar backend and a SQL projection give **different sums** for the same
ledger and the same query, with nothing failing.

It is in the reference backend's stated limits, so it is disclosed rather than
hidden, and no check pins it in either direction.

**DESIGN says nothing about the case.** And the wire shape may already have
decided it: `usage-collector-v1.yaml:1092`-`:1101` types every
`AggregationBucket.key` item as a non-nullable `string`, with no null spelling
available, so dropping may be the only answer the published response can carry.
That is an argument, not a ruling — the alternative is a sentinel or a documented
omission of the bucket — and it is a spec owner's to make.

**Do not write the check first.** A check pins whichever answer its author
picked, and here that would be a decision made by a test rather than by the
contract. The order is: DESIGN states the rule, `usage-collector-v1.yaml` gains
whatever the rule needs on `AggregationBucket.key`, then a check pins it and the
reference backend either already conforms or is corrected.

## Evidence

Four dated lines, not one. The slice-3 line is what makes entries 1-5
checkable, the slice-4 line entries 6-11, and the slice-5 line entries 12-16;
overwriting any of them would strand those entries. Every line names the commit
it was taken at, and a line that names none is not evidence — see the last
paragraph of this section.

**Slice 7 (the TimescaleDB plugin port), verified at `f884b9c64` on
`usage-collector/implementation-change`, 2026-09-11:** **937 passed, 0 skipped**
across the **four** usage-collector packages — the plugin is a workspace member
again, which is what moves the figure from slice 6's three-package 716; plus
**166 passed, 0 skipped** for `cargo nextest run -p cf-gears-usage-collector-sdk
--features contract`. `cargo check --workspace --all-targets` and `cargo clippy
--workspace --all-targets --all-features` clean, `cargo +nightly fmt` a no-op;
`cargo doc --no-deps` clean on `cf-gears-usage-collector-sdk` and the same **35**
pre-existing warnings on `cf-gears-usage-collector`, neither count grown.
Separately, the DESIGN §3.3 contract suite runs green against a live
`timescale/timescaledb` container via
`cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features
postgres`, which is the port's acceptance criterion and is not in the figures
above.

**The plugin is no longer identical to `main`, and every earlier line here says
it is.** `git diff --stat main --
gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/` is 40
files, +10 736 / -4 288. Entries 1-19 predate that, so where one of them quotes
the plugin it is quoting the pre-port file; entry 16's strike is the marker for
the one that did so substantively.

Entries 20-25, the strikes on 16 / §A / §G, and the additions to 10, 14 and 19
were verified against the branch at that commit. The sweep commits that add them
change `DIVERGENCES.md`, the plugin's `README.md`, one rustdoc paragraph in
`usage-collector-sdk/src/models.rs`, and the generated `docs/api/api.json` — no
behaviour and no test. Slice 7 touched no file under
`gears/system/usage-collector/docs/`, so the quotations in 1-19 stand as earlier
slices left them.

**Slice 6 (errors and the contract gate), verified at `540dfbba0` on
`usage-collector/implementation-change`, 2026-09-09:** **716 passed, 0
skipped** across the three usage-collector packages, from the 701-passed /
6-skipped baseline this slice inherited; plus **166 passed, 0 skipped** for
`cargo nextest run -p cf-gears-usage-collector-sdk --features contract`, which
is the DESIGN §3.3 plugin contract suite and its discrimination proofs. **The 6
skips are gone**: they were the `#[ignore]`d OpenAPI drift tests every earlier
line on this branch reports, and turning that gate on is what removes them —
there is no `#[ignore]` left in the gear. `cargo check --workspace
--all-targets` and `cargo clippy --workspace --all-targets --all-features`
clean, `cargo +nightly fmt` a no-op; `cargo doc --no-deps` clean on
`cf-gears-usage-collector-sdk` and the same 35 pre-existing warnings on
`cf-gears-usage-collector`, neither count grown. `git diff --stat 9f64cf22f --
gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/` is
empty, so entry 16 still quotes the plugin exactly as `main` has it.

Entries 17-19 and the additions to 5, 8, 9, 10 and 14 were verified against the
branch at that commit. The sweep commit that adds them changes `DIVERGENCES.md`
and one sentence in the governing spec under `docs/superpowers/specs/`, and no
behaviour and no test. Slice 6 touched no file under
`gears/system/usage-collector/docs/` either, so the quotations in 17-19 are the
documents as slices 1-2 left them.

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

**Re-verified at `9e36bbf6b`** — slice 4's head, three commits after
`531227f45`, none of which changes behaviour — after that sweep's own review
found four accuracy defects in this file and one gap in the SPI: **648 passed, 6
skipped**, `cargo check --workspace --all-targets` and `cargo clippy
--workspace --all-targets --all-features` clean, `git diff --stat main --
gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/` empty.

That line read "Re-verified at the branch head" and named no commit until slice
6 pinned it. It was true when written and false soon after — slice 5 moved the
count to 701 and slice 6 to 716 with the skips gone — and the number was not
simply refreshed, because an unpinned count re-breaks on the very next commit,
which is presumably why every other line here carries one. Recorded as entry
10's second gate gap. **Do not add an evidence line that names no commit.**

Checked with `git rev-parse HEAD:<path>` against `git rev-parse main:<path>`
rather than `git diff --quiet`, which reported the three files identical and was
wrong. Worth knowing if you reach for the same check.
