# Usage collector: where the implementation disagrees with the spec

This is a work list for bringing the usage-collector code in line with its
specification.

**Source of truth.** The spec lives in `gears/system/usage-collector/docs/`:

- `DESIGN.md`
- `PRD.md`
- `usage-collector-v1.yaml`
- `ADR/*`
- `schemas/*`
- `DECOMPOSITION.md`

The `docs/features/*.md` files predate the latest spec rework, so they are
secondary.

Prefer changing code. Section 2 lists the places where the spec itself looks
wrong, inconsistent or impossible. Those need a decision before the code can
follow.

**How to read an item.**

- **Spec** is what the spec requires.
- **Code** is what the implementation does.
- **Fix** is the suggested change.

Each item also gives a severity (**high**, **medium** or **low**). The IDs in
brackets (for example `ING-9`) are the IDs from the six review reports this
file was built from, kept so the same issue can be traced across areas.

**Where references point.**

- Paths are relative to `gears/system/usage-collector/` unless they start at the
  repo root.
- Line numbers are at commit `2f09940e3` and will drift. Search for the quoted
  names if a line has moved.

**How this was checked.** Every item was found by reading code against the spec.
The high-severity items were re-checked by hand.

**Out of scope.** The usage feed and reconciliation are not built yet, by plan,
so their absence is not listed here. Where an item touches them, it says so.

## Suggested order of work

Items depend on each other, so this order avoids doing things twice.

1. **Record shape:** `quantity`, `accepted_at` and the idempotency key rules
   (1.1–1.3). Almost everything else touches these.
2. **Invalidation and dedup model** (2.x). It changes the plugin API errors, the
   plugin schema and the contract suite together.
3. **Backfill hard bound and permission cleanup** (3.1).
4. **Plugin API shape.** Make errors match the spec (8.1, 8.2), move cursor
   minting to the gateway (8.6), remove the obsolete `acceptance_sequence` (6.1),
   then rewrite the contract suite (8.3).
5. **Query rules** (7.x), **authorization** (5.x), **types and registry** (9.x).
6. **Errors, metrics and docs** (10.x, 11).

## Decisions (settled 2026-09-15)

These were open spec questions. They are now decided, and they override the
"Decide" text of the matching item in section 12.

- The **Spec edit** column lists changes the spec documents still need.
- The **Code** column points to the items whose fix follows from the decision.

| Item | Decision | Spec edit | Code |
| --- | --- | --- | --- |
| **12.0** | Revert the implementation's edit to the DESIGN `uc_type_resolution_total` row | Done: `DESIGN.md` restored to the design-change text | — |
| **S-Q0** | Quotas are keyed on `subject_id`: per subject and per (subject, tenant) | PRD fr-rate-limiting, DESIGN `:91`, `:667`. Fix the `SecurityContext` claim at DESIGN `:244-247`. Quotas are deployment config, so drop "quota configuration" as a REST-only operation (DESIGN `:921-922`, PRD `:939`) | 4.2 |
| **S-A1** | The invalidation target lookup uses the scope compiled from the write (`create`) permit's constraints | Name it in DESIGN §3.2 / §3.9.6 | 2.4 |
| **S-C1** | Raw read: the plugin takes a keyset and a limit and returns rows plus the last keyset. The Query Gateway mints and decodes cursors | DESIGN §3.3 `list_usage_records` signature (`:1067-1073`) | 8.6 |
| **S-I1** | Backfill isolation means its own configurable concurrency/admission budget, plus an alert on live ingestion latency under backfill load | ADR-0012 and DESIGN `:104`, `:681`: name the mechanism and its config keys. Add the alert to §3.11.6 | 3.2 |
| **S-P3** | Keep flat tenant expansion by the PDP: no `InTenantSubtree`, no closure table, as in credstore. The subject property is `subject_id`. Add `gts_type_id` | DESIGN §3.9.6 row: `owner_tenant_id` takes a flat tenant set, no subtree predicate | 5.1 |
| **S-Q2** | `group_by`: the YAML's five fixed fields plus metadata keys, at most 2 dimensions | DESIGN `:522` splits the `$filter` and `group_by` sets. PRD `:266-268`, AC `:1459` get the cap | 7.4 |
| **S-R2** | The retention floor is an operator readiness-review duty. The gear warns with a metric when a resolved type's retention is below the configured floor. The idempotency-horizon guarantee is stated as conditional on it | YAML `:280-282`, PRD `:727`, AC `:1466`, DESIGN `:570`. Add a replay-horizon config key | 9.6 |
| **S-E2** | Publish the reasons the code already emits, and add new ones for the missing PRD cases (for example `RESERVED_KEY_PREFIX`, `KEY_ON_INVALIDATION`, `QUANTITY_OUT_OF_RANGE`), all in `field_violations[].reason` | Enumerate them in the YAML | 1.3, 1.4, 10.2 |
| **S-T1** | Defer the declaration mirror until types-registry runs its DB-backed store (today it wires an in-memory repository, `gears/system/types-registry/types-registry/src/gear.rs:127`) | Add a DESIGN §3.12.2 debt row. Mark ADR-0015 as not delivered. `restored` stays reserved | 9.1 (deferred) |
| **S-T2** | The gear registers its own base type through the link-time GTS inventory, from `docs/schemas/usage_record.v1.schema.json` | PRD `:1026` and DESIGN `:1327`: allow a gear to publish its own reserved base | 9.2 |
| **S-T3** | Cache model: TTL refresh (default 300 s), and the last good copy is served during a registry outage. Meter ids are major-only and can change in place under types-registry ADR-0004 | Amend ADR-0008 statement 3 to cite types-registry ADR-0004. DESIGN §3.2 documents the TTL and capacity; §3.11.5 documents `served_stale` | 9.3 |

Defaults, also accepted:

- **S-B6:** the gateway and the plugin both resolve same-identity entries within one batch.
- **S-B7:** the faithful copy and dedup compare the quantity as text, digit for digit.
- **S-Q1:** `and`/`or`/`not` are allowed in `$filter`.
- **S-Q3:** an oversized aggregate result is reported in `field_violations`, as DESIGN says.
- **S-Q7:** `COUNT` and `SUM` over nothing return `"0"`. The spec states this per fold.
- **1.7:** schema errors in a batch are rejected per entry.
- **4.3:** PDP runs before validation.
- **S-C2:** no convergence bound is needed yet (TimescaleDB is linearizable).
- **S-C3:** the noop plugin is exempt from the contract suite.

Still open, and not blocking: S-T4 to S-T8, S-P1, S-P2, S-E1, S-O1, S-O2,
S-R1, S-R3, S-Q4 to S-Q6, S-Q8, S-S1 to S-S5, and the PRD editorial fixes (12.8).

---

## 1. Record and request shape

### 1.1 Wire field is `value`, spec says `quantity` — high
[ING-9, API-8, QRY-16]

- **Spec:** `quantity` is required in YAML `CreateUsageRecordRequest` (`usage-collector-v1.yaml:868-874`) and `UsageRecord` (`:779`, `:837`). Also DESIGN §3.1 field ownership (`DESIGN.md:537`) and `schemas/usage_record.v1.schema.json`.
- **Code:** the field is named `value`:
  - REST DTOs: `usage-collector/src/api/rest/dto.rs:104` (request, `deny_unknown_fields`) and `:177` (response).
  - SDK model and its wire shadows: `usage-collector-sdk/src/models.rs:894`, `:999`, `:1259`, `:1326`, `:1400`, `:1456`.
  - The e2e suite pins `"value"`: `testing/e2e/suites/usage_collector/conftest.py:391`.

  A client generated from the YAML is refused with 400.
- **Fix:** rename to `quantity` in the DTOs, the SDK serde names (ideally the Rust fields too), the tests and the e2e suite.

### 1.2 `accepted_at` is never stamped, stored or returned — high
[ING-11, API-9, QRY-4, SPI-9]

- **Spec:**
  - The server assigns `accepted_at` at the Ingestion Gateway (`DESIGN.md:512`, `:538`, `:670` "Stamps `accepted_at` and `origin`").
  - It is required on `UsageRecord` (`usage-collector-v1.yaml:783`, `:845-852`).
  - PRD fr-billing-fields-on-read (`PRD.md:541`, `:660`).
  - It is also the second key of the `LATEST` tie-break (`DESIGN.md:586`).
- **Code:** it is absent from `UsageRecord` (`usage-collector-sdk/src/models.rs:850-943`) and from `UsageRecordDto` (`usage-collector/src/api/rest/dto.rs:165-210`).
  - The only non-test mention is a doc comment (`models.rs:757`).
  - The TimescaleDB plugin stores `ingested_at DEFAULT now()`, a database clock value that is never read back (`plugins/timescaledb-usage-collector-plugin/migrations/0001_init.sql:68`).
- **Fix:**
  - Stamp once per request in the gateway.
  - Carry it through the SDK model and the plugin API.
  - Persist it in the plugin, replacing or next to `ingested_at`.
  - Return the stored value on reads and on absorbed retries.

### 1.3 Idempotency key rules for invalidations and the `inv:` prefix — high
[ING-1, ING-2, API-10]

- **Spec:**
  - An invalidation carries no caller key. The gateway derives `inv:` + target id (lowercase, hyphenated) and rejects a supplied key.
  - A record key starting with `inv:` is rejected.
  - Sources: `DESIGN.md:566`; also `:519`, `:537-538`, `:649-650`, the invalidate sequence `:1440`, PRD `:231`, `:240`, ADR-0004 `:88-92`, ADR-0007 `:122-127`.
  - YAML: the invalidation branch has `idempotency_key: false` (`:883-889`), and `IdempotencyKey` has `not: pattern '^inv:'` (`:659-660`).
- **Code:** the key is required on every entry and never derived.
  - `CreateUsageRecord.idempotency_key` in the SDK (`models.rs:1001`).
  - `pub idempotency_key: String` in the DTO (`dto.rs:112`).
  - `handlers/usage_records.rs:931`.
  - `domain/invalidation.rs:108-110` says an invalidation "carries its own key".
  - `IdempotencyKey::new` (`models.rs:344-369`) has no prefix check, and the string `inv:` appears nowhere in the code.
- **Fix:**
  - Make the key optional on the SDK and the DTO.
  - Reject it on an invalidation.
  - Derive `inv:<target>` before deriving the id.
  - Reject a record key starting with `inv:`, and a record with no key.
  - Add golden id vectors (ADR-0007 `:233-236`).

### 1.4 Quantity range and precision are not enforced; parsing silently rounds — high
[ING-10]

- **Spec:** the quantity has a published range and precision.
  - Pattern `^-?(?:0|[1-9][0-9]{0,27})(?:\.[0-9]{1,28})?$`, at most 28 significant digits, "the gear enforces at ingestion" (`usage-collector-v1.yaml:623-641`).
  - "No conversion, scaling, rounding, or truncation on any path" (`DESIGN.md:574`, `:658`).
  - PRD `:619`, ADR-0013.
- **Code:** no range or scale check exists anywhere.
  - `#[serde(with = "rust_decimal::serde::str")]` (`dto.rs:103`, `models.rs:1399`) parses through rust_decimal's `from_str`, which rounds past 28 fractional digits.
  - It also falls back to scientific notation.
  - So `1e3`, leading `+`, `_` separators and leading zeros are all accepted, and over-precise values are rounded instead of rejected.
- **Fix:** deserialize the quantity as a string, check it against the pattern and the 28-digit bound, then parse exactly. Do this for REST, SDK serde, and an in-process check on `CreateUsageRecord`. Reject with a validation reason on `quantity`.

### 1.5 Aggregate request puts `gts_type_id`, `filter`, `metadata_filter` in the query string — high
[API-11, QRY-15]

- **Spec:** YAML `AggregationRequest` (`usage-collector-v1.yaml:1099-1141`) is a body with `required: [gts_type_id, time_range]`, `additionalProperties: false`, and `filter`, `metadata_filter` and `group_by` in the body. `POST /records/aggregate` has no query parameters (`:224-253`).
- **Code:**
  - `dto.rs:390-397` has only `time_range` and `group_by`.
  - `api/rest/routes/usage_records.rs:204-213`, `:230` declare the other three as query parameters, and `handlers/usage_records.rs:452-508` parses them.
  - The mismatch is excused in `openapi_contract_tests.rs:196-212` (`BODY_VS_QUERY_DRIFT`).
- **Fix:**
  - Read all three from the body, parsing `filter` with the `toolkit_odata` string parser.
  - Remove the query parameters.
  - Empty `BODY_VS_QUERY_DRIFT`.

### 1.6 Default page size is 1000, spec says 100 — medium
[API-12, QRY-8]

- **Spec:** YAML `Limit` default is 100, maximum 1000 (`usage-collector-v1.yaml:494-503`).
- **Code:** REST sets `None => Some(MAX_PAGE_SIZE)` (1000) at `handlers/usage_records.rs:635`. The SDK path falls through to the plugin's `DEFAULT_PAGE_SIZE = 100` (`plugins/timescaledb-usage-collector-plugin/src/infra/storage/record_store.rs:77`), so REST and SDK disagree.
- **Fix:** apply a default of 100 in the service, for both surfaces.

### 1.7 One malformed entry fails the whole batch; the cap is checked after parsing — medium
[ING-17]

- **Spec:**
  - Per-entry outcomes; a rejected entry carries the error a single submission would get (`usage-collector-v1.yaml:95-96`, `:980-1000`; `DESIGN.md:1411-1412`).
  - An over-cap submission is rejected "before any entry is validated" (`DESIGN.md:645-646`).
- **Code:** `Json<CreateUsageRecordsRequest>` (`handlers/usage_records.rs:66`) strictly deserializes every entry. A missing or extra field, a bad timestamp or a bad quantity in one entry fails the whole request with 400. An over-cap body containing a bad entry never reaches the cap check (`:149-154`).
- **Fix (decided, 1.7):** deserialize `records` as `Vec<serde_json::Value>`, check the cap first, then decode each entry into a per-index rejection.

### 1.8 Attribution string limits differ from the schema — low
[ING-18]

- **Spec:** `resource_id`, `resource_type`, `subject_id` and `subject_type` have `maxLength: 256`. `IdempotencyKey` is ≤ 256 and `ReasonCode` is ≤ 128, counted in characters (`usage-collector-v1.yaml:744-768`).
- **Code:**
  - `ResourceRef::new` and `SubjectRef::new` have no length cap (`models.rs:145-175`, `:236-272`).
  - `IdempotencyKey` and `ReasonCode` cap bytes, not characters (`:351`, `:468`).
  - `ReasonCode` also rejects control characters, which the spec does not mention (`:475`).
- **Fix:** add the 256 caps and count characters.

---

## 2. Invalidation and dedup model

### 2.1 "At most one invalidation" is a store-side rule; the spec makes it a dedup outcome — high
[ING-3, SPI-4, SPI-5, SPI-7, SPI-12, API-19]

- **Spec:**
  - At most one invalidation follows from the derived invalidation key. A second one with the same reason code is absorbed; a different reason code is `AlreadyInvalidated`. "No store-side rule exists beyond the dedup identity" (`DESIGN.md:578`; also `:1122-1123`).
  - The plugin reports `IdempotencyConflict { idempotency_key, existing }`. The host lifts it to `Conflict(AlreadyInvalidated)` when the dispatched entry is an invalidation (`DESIGN.md:1263`).
  - ADR-0010 `:155-170` and `:198-204` reject a store-side atomic check.
- **Code:**
  - The plugin API rustdoc requires an atomic store check: `usage-collector-sdk/src/plugin_api.rs:30-46`, `:56-60`, `:154-161`; SDK `error.rs:690-695`, `:761-768`.
  - The plugin API error enum has an extra `AlreadyInvalidated { id, invalidated_by }` (`usage-collector-sdk/src/error.rs:903-915`).
  - TimescaleDB enforces it with a partial unique index, `usage_records_one_invalidation_uniq` (`migrations/0001_init.sql:114-130`):
    - its error class (`src/infra/storage/error.rs:15-19`, `:112-118`);
    - `map_insert_error` and `find_existing_invalidation` (`record_store.rs:281-331`, `:1152-1177`);
    - an in-batch pre-reject (`:1613-1671`);
    - two metrics (`src/metrics.rs:373-380`).
  - A cross-call collision on that index aborts the whole multi-row insert, so `create_batch` fails as a whole instead of per entry (`record_store.rs:790-797`).
  - The host lifts `IdempotencyConflict` to `IDEMPOTENCY_CONFLICT` whatever the entry kind (`usage-collector/src/domain/error.rs:285-313`, `:351-357`).
  - The reference backend enforces the old rule in `admit` (`usage-collector-sdk/src/contract/reference.rs:326-338`).
  - Visible result: a second withdrawal with the same reason code but a different caller key gets `ALREADY_INVALIDATED`, where the spec says it is absorbed.
- **Fix (after 1.3):**
  - Remove `AlreadyInvalidated` from the plugin API errors and the store obligation from the rustdoc.
  - Drop the partial index, its error class, the in-batch pre-reject and the two metrics.
  - A second invalidation then collides on `usage_records_dedup_uniq`.
  - In the host, choose `Conflict(AlreadyInvalidated)` or `Conflict(IdempotencyConflict)` by the kind of the dispatched entry.
  - Update the header comment of `0002_usage_rollup.sql` and README lines 48 and 72, which justify rollup exactness by the removed index.

### 2.2 `ALREADY_INVALIDATED` context lacks `invalidated_by` and `reason_code`; the plugin conflict lacks `existing` — medium
[ING-4, API-14, SPI-3]

- **Spec:**
  - The `context` names the target, `invalidated_by` and the stored `reason_code` (`usage-collector-v1.yaml:987-990`; `DESIGN.md:1263`).
  - The plugin variant is `IdempotencyConflict { idempotency_key, existing }`, where `existing` is the stored entry (`DESIGN.md:1259-1268`).
- **Code:**
  - `IdempotencyConflict { idempotency_key, existing_id: Uuid }` (`usage-collector-sdk/src/error.rs:886-893`).
  - `UsageCollectorError::already_invalidated` puts `invalidated_by` only in the prose `detail` (`error.rs:769-776`).
  - The mapping sets only resource name and reason (`usage-collector/src/infra/sdk_error_mapping.rs:241-256`).
- **Fix:** change the variant to `existing: UsageRecord`, and put `invalidated_by` and `reason_code` into the problem `context`.

### 2.3 No converged-only target lookup; no `TargetNotConverged` — high
[ING-5, SPI-2, QRY-22]

- **Spec:**
  - The plugin API has `get_usage_record(id, scope, converged_only: bool)` (`DESIGN.md:1041-1051`). The obligation is to answer `UsageRecordNotConverged` only until it can decide (`:1124-1128`).
  - The target is looked up converged-only (`:662-666`, sequence `:1429-1434`).
  - The host lifts `UsageRecordNotConverged` to `Conflict(TargetNotConverged)` with `retryable = true` (`:1251-1254`, `:1265`).
  - YAML `:990`, PRD `:437`, ADR-0010 `:135-140`, `:179-192`.
- **Code:**
  - `plugin_api.rs:93-97` takes `(id, scope)` only; the same applies in the TimescaleDB adapter, `ports.rs:26-30`, `record_store.rs:2193-2197`, noop `plugin.rs:51-57` and `reference.rs:178`.
  - There is no `UsageRecordNotConverged` variant and no `TARGET_NOT_CONVERGED` reason (`usage-collector-sdk/src/reason.rs:197-215`).
  - `is_retryable()` is true only for `ServiceUnavailable` (`error.rs:840-842`).
- **Fix:**
  - Add the parameter, the variant, the reason (retryable) and the lift.
  - The gateway passes `converged_only = true` at both target lookups (`usage-collector/src/domain/service.rs:620`, `:1032-1035`) and `false` on the caller-facing read.
  - TimescaleDB, a single primary, can state it is always converged.

### 2.4 Invalidation target lookup ignores the caller's scope (existence oracle) — high, security
[ING-6, API-5]

- **Spec:** a converged-only lookup "applies `scope` first", and an out-of-scope target answers exactly like an absent one (`DESIGN.md:1124-1125`; contract check `converged-target-lookup` `:1141`; ADR-0010 `:194-196`; PRD `:437`; threat model `:1634`).
- **Code:** `target_pinned_read_filter` builds `id eq <target>` (`service.rs:512-520`) and is used at `:620` and `:1034`. So the answer depends on whether the target id exists:
  - an id that does not exist gets 404;
  - a row from another tenant gets 400 `INVALIDATION_FIELD_MISMATCH`, naming the first differing field (`domain/invalidation.rs:187-211`).
  
  This leaks the existence and attributes of other tenants' rows.
- **Fix (decided, S-A1):** compile the constraints of the write (`create`) permit into an `ast::Expr` and pass it as the lookup scope.

### 2.5 Dedup comparison includes the server-assigned `origin` — medium
[SPI-10, ING-14]

- **Spec:** a collision is resolved "by exact equality of the caller-supplied fields" (`DESIGN.md:568`). `origin` is server-assigned (`:537-538`). ADR-0004 `:117-122` lists the compared fields.
- **Code:**
  - TimescaleDB `canonical_equal` compares `row.origin == incoming.origin` (`record_store.rs:1865`).
  - The reference backend compares the full `UsageRecord` (`reference.rs:318`).
  - An identical retry sent once via live and once via backfill gets `IDEMPOTENCY_CONFLICT`.
- **Fix:** compare only caller-supplied fields in both backends, ideally with a shared SDK helper. Compare the quantity as text, digit for digit (decided, S-B7), here and in the faithful-copy check (`domain/invalidation.rs:170-180`).

### 2.6 Duplicates inside one batch are resolved by the plugin, not the gateway — low
[ING-15]

- **Spec:** "Two same-identity entries in one request resolve the same way at the gateway, the later against the earlier" (`DESIGN.md:568`; ADR-0004 `:186-188`).
- **Code:** the gateway does not group by derived id (`service.rs:1408-1751`); TimescaleDB resolves them (`record_store.rs:720-760`).
- **Fix:** add a gateway pass before dispatch. Plugins keep handling it too (decided, S-B6).

### 2.7 No backend declares a dedup level or convergence bound — medium
[SPI-15]

- **Spec:** the plugin must "Declare a dedup level and meet it", published per §3.10 (`DESIGN.md:1121`, `:569`, §3.10 item 9 `:1766-1770`). A late divergent write is "discarded, counted when divergent".
- **Code:**
  - TimescaleDB is effectively `linearizable` (single primary, `ON CONFLICT DO NOTHING`), but nothing declares it.
  - No late-convergence counter exists (`plugins/timescaledb-usage-collector-plugin/src/metrics.rs:332-490`).
  - The contract suite takes no declared level as input.
- **Fix:**
  - Declare the level and bound in the plugin README.
  - Add a (zero-valued) late-convergence counter.
  - Give `contract::run_all` a declared-level input.

---

## 3. Backfill

### 3.1 Backfill window is not a hard bound; a withdrawn "elevated backfill" override still exists — high
[ING-7, API-1, API-2, TYP-4, API-16]

- **Spec:**
  - The window is a hard bound; no surface reaches past it, and the override is withdrawn (ADR-0012 `:138-148`; `DESIGN.md:681-683`, `:1556`, `:1981`; PRD fr-backfill `:725`, `:729`, `:211`).
  - YAML `:266-271`: rejected with `PAST_WINDOW` naming the window; a period ending exactly at the window is admitted.
  - The idempotency horizon and the retention floor depend on this (`DESIGN.md:570`).
- **Code:** the window only picks the PDP action.
  - `enforce_covered_period_bounds` never reads the window (`usage-collector/src/domain/covered_period.rs:119-140`). `ingestion_action` returns `BACKFILL` beyond it (`:63-70`, `:168-186`), used at `service.rs:998` and `:1507`.
  - `actions::BACKFILL` (`domain/authz.rs:239-249`), and the `action` member of `AttributionTupleKey`.
  - The permission `cf.core.uc.usage_record_backfill.v1` is registered (`usage-collector/src/gts/permissions.rs:54-62`, test `:95-116`).
  - Docs describe elevated authorization in `config.rs:95-111`, `:279-285`, `:305-315`, the route description (`api/rest/routes/usage_records.rs:73-81`), the SDK trait (`usage-collector-sdk/src/api.rs:70-74`) and `service.rs:1215-1220`.
- **Effects:**
  - A backfill entry of any age is admitted when the permission is granted.
  - An entry older than its type's retention is acknowledged and then purged at the next sweep, and its dedup identity is already gone.
- **Fix:**
  - On the backfill path, reject `now - window_end > backfill_window` with `PAST_WINDOW`, naming the bound; this applies to invalidations too.
  - Delete `ingestion_action`, `actions::BACKFILL`, the permission instance and the tuple `action`, so all ingestion uses `create`.
  - Fix the config, route, SDK and service docs, and the tests that expect elevated authorization.

### 3.2 Backfill workload isolation is not implemented — high
[ING-13, API-25]

- **Spec:** "The backfill path is isolated from live ingestion at the gear" (`DESIGN.md:104`, `:681`, `:1559`; ADR-0012 `:167-169`; YAML `:246`, `:261-262`; PRD `:725`).
- **Code:** `backfill_usage_records` shares the live path and its concurrency budgets (`service.rs:1193-1252`, TODO at `:1222-1231`). The route text deliberately omits the claim (`routes/usage_records.rs:60-69`), and so does `usage-collector-sdk/src/api.rs:76-80`.
- **Fix (decided, S-I1):** give backfill its own configurable admission and concurrency budget, add an alert on live ingestion latency under backfill load, then restore the route summary to match the YAML.

---

## 4. Ingestion gateway: other rules

### 4.1 Batch cap is a constant, not operator configuration — medium
[ING-8, API-20]

- **Spec:**
  - "The cap is operator configuration, 100 by default" (`DESIGN.md:643-646`; YAML `:80-82`, `:939-941`).
  - `uc_ingestion_batch_size` buckets extend to the configured cap above 100 (`DESIGN.md:1858`).
- **Code:**
  - `pub const MAX_BATCH_RECORDS: usize = 100` (`service.rs:66`), used at `:1275` and `handlers/usage_records.rs:150`.
  - There is no config key (`config.rs:17-112`).
  - Buckets are fixed at ≤ 100 (`infra/metrics.rs:48-50`).
  - The doc at `service.rs:63-65` wrongly claims the YAML has `maxItems`.
- **Fix:**
  - Add `max_batch_records` (default 100, at least 1) and pass it into the service and handler.
  - Build the buckets to include the cap.
  - Fix the doc.

### 4.2 No ingestion quotas and no `ResourceExhausted` — high
[ING-12, API-21, API-17]

- **Spec:**
  - Per-caller and per-(caller, tenant) quotas, with a throttle error carrying retry guidance (PRD fr-rate-limiting `:739`; `DESIGN.md:667-669`, sequence `:1390`, `:1635`).
  - `ResourceExhausted` → 429 (`:1245`).
  - Metric category `quota` (`:1838`).
- **Code:** nothing: no config, no check, no error variant.
- **Fix:**
  - Add quota config keyed on `subject_id` and (`subject_id`, tenant) (decided, S-Q0), and a check before PDP on both ingestion paths.
  - Add `ResourceExhausted { retry_after_seconds }` → 429.
  - Add the `quota` metric category.

### 4.3 Validation runs before the PDP call — low
[ING-16, API-6]

- **Spec:** PDP first, then scope, type resolution and dispatch (`DESIGN.md:889-893`; sequences `:1390-1403`, `:1427-1440`, `:1554-1557`).
- **Code:**
  - Projection, id derivation and period bounds run before PDP (`service.rs:986-1001`, batch `:1461-1485`).
  - The code justifies this by "§3.8's pipeline order", but §3.8 is now Deployment Topology.
  - So an unauthorized caller sees `PAST_WINDOW`/`FUTURE_WINDOW` instead of 403.
- **Fix (decided, 4.3):** authorize on the attribution tuple first, then validate, and remove the stale §3.8 references.

---

## 5. Authorization

### 5.1 PDP request misses `gts_type_id`; property names and subtree support differ from §3.9.6 — high
[ING-23, API-3]

- **Spec:**
  - §3.9.6 (`DESIGN.md:1664-1676`) advertises `owner_tenant_id` (with `InTenantSubtree`), `resource_id`, `resource_type`, `subject_id`, `subject_type` and `gts_type_id`.
  - A write needs a scope that admits the full tuple: tenant, resource, GTS type and subject (`:243-247`, `:1316`; PRD `:357-358`).
- **Code:**
  - `AttributionTupleKey` has no meter (`usage-collector/src/domain/authz.rs:160-192`), the write request sends no `gts_type_id` (`:332-344`), and the post-permit gate never checks it (`:565-586`).
  - Advertised properties omit `gts_type_id`, and the subject is advertised as `OWNER_ID`, not `subject_id` (`:222-231`).
  - `InTenantSubtree` is rejected fail-closed (`:516-527`, `:901-918`).
  - Read authorizations send no `gts_type_id` either.
  - Batch entries that differ only by meter share one decision.
- **Fix:**
  - Add `gts_type_id` to the key, the request properties and the gate, on writes and reads.
  - Advertise `subject_id`.
  - Keep rejecting `InTenantSubtree`. The PDP expands tenant subtrees into a flat list (decided, S-P3; the spec row changes).

---

## 6. Obsolete feed-order mechanism

### 6.1 `acceptance_sequence` still drives the TimescaleDB schema and ingest — medium
[SPI-8]

- **Spec:** feed order belongs to the plugin and its position is opaque (`DESIGN.md:524`, `:587`). The spec no longer has a sequence obligation (§3.7, `:1590-1592`). The `LATEST` tie-break no longer uses it (see 7.1).
- **Code:** the obsolete sequence is still wired through the schema and the store:
  - `acceptance_sequence` column, `usage_acceptance_sequence` table and two indexes (`plugins/timescaledb-usage-collector-plugin/migrations/0001_init.sql:53-66`, `:132-144`, `:158-169`).
  - `claim_acceptance_sequence` and `claim_batch_sequences` (`record_store.rs:433-448`, `:994-1112`, rustdoc citing an old DESIGN §1.2).
  - `entity.rs:90-93`, `mapper.rs:189`, `pool.rs:70`.
  
  Every ingest takes a row lock per (tenant, type), which serializes ingestion per scope.
- **Fix:** remove the column, table and claim path (the feed will design its own position when it is built). Re-check whether SQLSTATE `55P03` still belongs in the transient set (`src/infra/storage/error.rs:31-44`).

---

## 7. Query rules

### 7.1 `LATEST` tie-break is wrong in both backends — high
[QRY-5, SPI-9]

- **Spec:** the greatest `window_end`, then the greatest `accepted_at`, then the greatest `id` in byte order. This ordering also applies across tenants (`DESIGN.md:586`; ADR-0009 `:115`, `:136-149`; check `latest-tie-break` `:1148`).
- **Code:**
  - TimescaleDB uses `ORDER BY r.window_end DESC, r.acceptance_sequence DESC` and documents that no cross-tenant order exists (`plugins/timescaledb-usage-collector-plugin/src/infra/storage/query/aggregate.rs:26-61`).
  - The reference backend uses `(window_end, id)` (`usage-collector-sdk/src/contract/reference.rs:812-836`).
  - The SDK doc mentions `acceptance_sequence` (`models.rs:1529-1531`).
  - The index is built for the sequence (`0001_init.sql:147-152`).
- **Fix (after 1.2):** order by `window_end DESC, accepted_at DESC, id DESC` in both backends, and fix the doc and the index.

### 7.2 `$orderby` refuses `entry_type` and `accepted_at` — medium
[QRY-7]

- **Spec:** YAML `RecordOrderKey` (`:587-611`) includes both, and DESIGN `:592` names it the single definition.
- **Code:**
  - `KEYSET_SAFE_RECORD_FIELDS` excludes them (`usage-collector-sdk/src/models.rs:1816-1848`).
  - Rejection happens at `usage-collector/src/domain/query.rs:252-262`.
  - The plugin has no arms for them (`record_store.rs:1204-1215`).
- **Fix:** admit both (`accepted_at` after 1.2).

### 7.3 `$filter` operators other than `eq`/`in` are accepted — medium
[QRY-9]

- **Spec:** "Fixed-field identifiers accept `eq` and `in` only" (`DESIGN.md:591`; YAML `:458-463`).
- **Code:** the gateway checks only reserved fields (`domain/query.rs:186-207`). The plugin renders `ne`/`gt`/`ge`/`lt`/`le` and `not` (`query/translate.rs:140-150`, `:307`).
- **Fix:** add a gateway operator check on both read paths → 400 on `$filter`. `and`/`or`/`not` stay allowed (decided, S-Q1).

### 7.4 `group_by` limits (max 2, unique) are not enforced — medium
[QRY-11]

- **Spec:** `group_by` has `maxItems: 2`, `uniqueItems: true` (`usage-collector-v1.yaml:1131-1141`).
- **Code:** `dto.rs:393-397` has no bound, and `service.rs:2183-2192` checks only declared keys.
- **Fix:** enforce in the service → 400 on `group_by`. The YAML's five fields and cap of 2 are the rule (decided, S-Q2).

### 7.5 Metadata filter caps are enforced on REST only — low
[QRY-12]

- **Spec:** 16 filters × 32 values, on both read paths (YAML `:1062`, `:1125`).
- **Code:** the caps are only in `handlers/usage_records.rs:551-557`, `:817-843`.
- **Fix:** move the caps into the service.

### 7.6 Cursor handling edge cases — low
[QRY-25]

- **Code:**
  - A backward cursor is refused only inside the plugin, as `Internal` / 500 (`record_store.rs:1351-1355`).
  - `ORDER_MISMATCH` can never fire, because the order is taken from the token (`domain/query.rs:405-425`).
- **Fix:** reject a non-forward cursor at the gateway with 400 `INVALID_CURSOR`. See S-Q4 for the DESIGN text.

### 7.7 Noop plugin returns an invalid aggregate and page — low
[QRY-28]

- **Spec:** an ungrouped aggregate yields one bucket with an empty key (YAML `:1176-1177`). `PageInfo.limit` is at least 1 (`:1292-1296`).
- **Code:** it always returns empty buckets and `ODataPage::empty(0)` (`plugins/noop-usage-collector-plugin/src/plugin.rs:72-96`).
- **Fix:** return spec-valid shapes.

### 7.8 Query-side error fields and reasons — low
[QRY-14]

- **Code:**
  - An undeclared metadata key on the read path reuses the ingestion error (field `metadata`, `UNKNOWN_METADATA_KEY`; `service.rs:1971-1975`, `error.rs:485-493`).
  - An undeclared `group_by` key has the same reason (`error.rs:651-661`).
  - The spec says 400 `VALIDATION` on the read parameter (YAML `:1055-1056`).
- **Fix:** use read-path field names (`metadata.<key>`, `metadata_filter`, `group_by`). Keep `UNKNOWN_METADATA_KEY` and publish it (decided, S-E2). Oversized results keep using `field_violations` (decided, S-Q3).

---

## 8. Plugin API and contract suite

### 8.1 Plugin API error enum does not match the six spec variants — high
[SPI-3]

- **Spec:** `Transient`, `Internal`, `IdempotencyConflict { idempotency_key, existing }`, `UsageRecordNotFound { id }`, `UsageRecordNotConverged { id }`, `CursorBeyondRetention` (`DESIGN.md:1259-1268`).
- **Code:** `usage-collector-sdk/src/error.rs:865-922` has:
  - `existing_id` instead of `existing`;
  - an extra `AlreadyInvalidated`;
  - no `UsageRecordNotConverged`.

  `CursorBeyondRetention` belongs to the feed and comes with it.
- **Fix:** covered by 2.1, 2.2 and 2.3. Also update `is_plugin_error_exhaustive_today` and every constructor and test that names the removed variant.

### 8.2 Plugin API rustdoc omits the current obligations — medium
[SPI-6]

- **Spec:** the obligation list in `DESIGN.md:1116-1128`:
  - declare and meet a dedup level;
  - converge within a bound;
  - acknowledge only what is durable;
  - converged-only lookups;
  - do not re-validate (→ `Internal`).
- **Code:** the `plugin_api.rs` rustdoc covers period-end selection, withdrawal exclusion, recomputation and cursors only.
- **Fix:** restate the §3.3 obligation list on the trait (and remove the old rule, see 2.1).

### 8.3 Contract suite still counts DESIGN's old seven checks; DESIGN now lists eleven — high
[SPI-11, SPI-12, SPI-13, QRY-6]

- **Spec:** `DESIGN.md:1136-1148` lists 11 checks:
  - `window-end-selection`
  - `invalidation-excluded-from-fold`
  - `at-most-one-invalidation`
  - `converged-target-lookup`
  - `dedup-identity-over-window`
  - `dedup-floor`
  - `dedup-concurrent`
  - `quantity-round-trip`
  - `feed-snapshot-and-replay`
  - `feed-completeness`
  - `latest-tie-break`
  
  ADR-0009 `:185-197` also asks for per-fold coverage and a COUNT hand-fold check.
- **Code:**
  - `usage-collector-sdk/src/contract.rs:45-81`, `:179-244` partitions against "DESIGN's seven". `contract_tests.rs:219-246` hard-codes `[&str; 7]`.
  - Not accounted for: `converged-target-lookup`, `dedup-floor`, `dedup-concurrent`.
  - The `latest-tie-break` blocker text cites `acceptance_sequence`, which is obsolete.
  - `at_most_one_invalidation.rs:129-133`, `:244-248` expects the removed `AlreadyInvalidated`, with fixtures using distinct caller keys (`fixtures.rs:185-208`).
  - `dedup_identity_over_window.rs:58-80` never submits a divergent same-identity entry.
  - `invalidation_excluded_from_fold.rs` has no COUNT, LATEST or cross-tenant coverage.
  - The README (`:86-88`) and `tests/contract_conformance_pg.rs` repeat "five of seven".
- **Fix:**
  - Partition against the 11.
  - Rewrite `at-most-one-invalidation` on a derived `inv:` key: the same reason code is absorbed, a different one is `IdempotencyConflict`.
  - Write `converged-target-lookup` (after 2.3), `dedup-floor`, `latest-tie-break` (after 7.1) and `dedup-concurrent` (needs a concurrent harness and a declared-level input).
  - Keep `feed-snapshot-and-replay` and `feed-completeness` listed as blocked until the feed is built.
  - Add per-fold checks.
  - Update the README and the test messages.
  - The noop plugin is exempt from the suite (decided, S-C3).

### 8.4 TimescaleDB README is missing most §3.10 deployment-guide items — medium
[SPI-16]

- **Spec:** `DESIGN.md:1738-1770` requires 9 items. It is a release-readiness gate (ADR-0006 `:177-179`, ADR-0011 `:252`).
- **Code:** `plugins/timescaledb-usage-collector-plugin/README.md` covers aggregate visibility (item 4, cited as "item 3") and retention (item 6, cited as "item 5"). It lacks:
  - item 1 (pools and query-path lag);
  - item 5 (monotonic reads);
  - item 8 (ingestion batching);
  - item 9 (dedup level and convergence).

  Items 2, 3 and 7 are about the feed and wait for it.
  
  It also documents the removed index (`:48`) and "five of DESIGN's seven" (`:86-88`).
- **Fix:** add the missing items, renumber the citations, drop the index and seven-check text, and re-cite the dedup tuple to §3.1.

### 8.5 A retention sweep can free a dedup identity early when `type_key_slice_width > 1` — low–medium
[SPI-17]

- **Spec:** "a purge never frees a dedup identity earlier than its horizon" (`DESIGN.md:595`, `:570`).
- **Code:** the README (`:59`) documents a race where a newly keyed type writes into a chunk the running sweep drops. It gives only advice; `config.rs` allows the width. (Race path read from the README; `retention_sweep.rs` was not traced end to end.)
- **Fix:** re-check slice membership under the retention lock before dropping, or reject a width above 1.

### 8.6 Raw read: move cursor minting from the plugin to the Query Gateway — medium
[SPEC-1, SPI-14; decided, S-C1]

- **Spec:**
  - Plugins never mint or read a wire cursor. They take a keyset and return rows plus the position to continue from (`DESIGN.md:336-341`, raw sequence `:1503-1507`, ADR-0011 `:106-107`).
  - The DESIGN signature (`:1067-1073`) is being amended to match.
- **Code:**
  - `list_usage_records(..., query: &ODataQuery, ...) -> ODataPage<UsageRecord>` (`usage-collector-sdk/src/plugin_api.rs:193-259`). The plugin must mint `next_cursor` and carry `filter_hash`.
  - TimescaleDB encodes and decodes `CursorV1` (`plugins/timescaledb-usage-collector-plugin/src/infra/storage/query/keyset.rs:49`, `:295-311`).
- **Fix:**
  - Change the plugin method to take the filter, order, an optional start-after keyset and a limit, and return rows plus the last keyset.
  - Mint, bind and decode `CursorV1` in the Query Gateway.
  - Strip the cursor obligations from the plugin rustdoc, TimescaleDB, the reference backend and the noop plugin.

---

## 9. Types and registry

### 9.1 Declaration mirror table and restore path do not exist — deferred
[TYP-1]

- **Spec:**
  - The gear mirrors each resolved declaration and restores one the registry lost (ADR-0015, `status: accepted`, statements 1–5 and 7 `:101-131`).
  - DESIGN §3.2 `:723-734`, §3.7 `:1578-1585`, `:1602`, `:1327`.
  - PRD fr-usage-type-resolution `:498` "MUST recover one the registry has lost", AC `:1422`.
- **Code:**
  - The resolver only fetches, parses and caches (`usage-collector/src/domain/type_resolver/mod.rs:178-234`), and a `NotFound` fails closed (`:209`).
  - The port is read-only (`domain/ports/declarations.rs:20-33`).
  - The gear has no database (`usage-collector/src/module.rs:32-37`).
  - There are no mirror metrics, and no ADR-0015 confirmation tests.
- **Decided (S-T1): deferred, no code now.**
  - types-registry still wires an in-memory repository (`gears/system/types-registry/types-registry/src/gear.rs:127`), and its DB-backed store would make the mirror unnecessary.
  - The spec records this as debt: a §3.12.2 row, ADR-0015 marked not delivered, and `restored` stays reserved.
  - Revisit when types-registry switches to its DB store. The design, if still needed then:
    - a DB capability and a mirror table;
    - upsert on cache miss, where a failed write counts `uc_declaration_mirror_write_failures_total` and does not reject;
    - on a definite `NotFound`, re-register the stored document and count `restored`;
    - never serve the mirror on a registry error.

### 9.2 Nothing registers the reserved base type `gts.cf.core.uc.usage_record.v1~` — high
[TYP-3]

- **Spec:** the base is reserved and abstract and carries the traits schema (ADR-0008 statement 7 `:139-148`). It is published at `docs/schemas/usage_record.v1.schema.json` (`DESIGN.md:486-487`; YAML `:704-710`).
- **Code:**
  - `usage-collector-sdk/src/gts.rs` defines only the id constant and the plugin spec type (`cf.toolkit.plugins.plugin.v1~cf.core.uc.plugin.v1~`).
  - No config seeds the base (`config/quickstart.yaml`, `config/e2e-local.yaml`, `testing/e2e/suites/usage_collector/config.yaml:82-83`).
  - The e2e fixture posts it by hand (`conftest.py:312-318`, `:360`).
  - On a fresh deployment no meter can be registered and every ingest fails.
- **Fix (decided, S-T2):** ship the base through the link-time GTS inventory, using `include_str!` of the published schema. The PRD and DESIGN get amended to allow it.

### 9.3 Type cache TTL and eviction are undocumented and weaken the outage guarantee — medium
[TYP-7]

- **Spec:** declarations are immutable, so "indefinite cache validity" is safe, and cached declarations "MUST remain usable while the registry is unreachable" (ADR-0008 statement 3 `:111-114`; PRD `:1027`).
- **Code:**
  - The TTL is justified by a "mutable major-only id" premise that contradicts ADR-0008 (`type_resolver/mod.rs:9-14`, `:38-52`).
  - Defaults are TTL 300 s and capacity 10 000 (`config.rs:119-120`), the same as the PRD's launch type count (`PRD.md:1495`).
  - Eviction removes the stale fallback (`mod.rs:281-289`), so an evicted meter during a registry outage is rejected.
  - After the TTL, a `NotFound` fails closed even though a cached copy exists (`:209-213`).
- **Fix (decided, S-T3: TTL plus stale fallback):**
  - Keep the TTL.
  - Never evict the only outage fallback of an in-use meter.
  - A `NotFound` after the TTL stays fail-closed, because the declaration was withdrawn.
  - Fix the resolver doc to cite types-registry ADR-0004 (mutable major-only ids).
  - The spec gets amended: ADR-0008 statement 3, DESIGN §3.2 (TTL, capacity) and §3.11.5 (`served_stale`).

### 9.4 Retention trait: weeks rejected, unparseable values keep data forever — medium
[TYP-6]

- **Code:**
  - The plugin rejects `P2W` (`libs/toolkit-utils/src/iso8601_duration.rs:199-201`) along with years and months.
  - An invalid value keeps chunks forever (`plugins/timescaledb-usage-collector-plugin/src/infra/registry_retention.rs:65-82`, `src/domain/retention.rs:93`). The meter still ingests normally.
- **Fix:** accept `PnW` (weeks are fixed-length). See S-T4 for adding a pattern to the schema.

### 9.5 A missing or unserved fold rejects ingestion — low
[TYP-8]

- **Spec:** ingestion does not depend on the fold (`PRD.md:494`, AC `:1383`; `DESIGN.md:676`).
- **Code:** `type_resolver/declaration.rs:100-109` fails the whole resolution, which affects ingest too (`service.rs:1017`, `:1583`). This is reachable only if the gear serves fewer folds than the schema enum.
- **Fix:** parse the fold lazily and reject only on the aggregate path, or reword the PRD (S-T5).

### 9.6 Warn when a type's retention is below the floor — low
[TYP-5, SPEC-24; decided, S-R2]

- **Spec (after the S-R2 amendment):**
  - The retention floor (backfill window plus one replay horizon) is an operator readiness-review duty.
  - The gear signals a resolved type whose declared retention is below it.
  - Today the code has no replay-horizon config and no check (`usage-collector/src/config.rs`).
- **Fix:**
  - Add a `replay_horizon` config key (PRD default 35 days).
  - When the Type Resolver resolves a declaration, parse its `retention` and increment a warning counter if it is below `backfill_window + replay_horizon`, without rejecting.
  - Add the counter to the spec's metric inventory.

---

## 10. Errors and observability

### 10.1 `UsageCollectorError` variants differ from the DESIGN table — medium
[API-17]

- **Spec:** `PermissionDenied`, `InvalidArgument`, `NotFound`, `Conflict`, `ResourceExhausted`, `ServiceUnavailable`, `Internal` (`DESIGN.md:1239-1247`).
- **Code:** `usage-collector-sdk/src/error.rs:66-201` has:
  - no `ResourceExhausted` (see 4.2);
  - an extra `AlreadyExists`, never constructed;
  - an extra `CursorRejected`.
- **Fix:** add `ResourceExhausted` and remove `AlreadyExists`. See S-E1 for `CursorRejected`.

### 10.2 Reason vocabulary differs — medium
[API-18]

- **Code:** `usage-collector-sdk/src/reason.rs`:
  - Missing: `TARGET_NOT_CONVERGED`.
  - Stale catalog-era reasons (`:39-55`): `SEMANTICS_VIOLATION` and `INVALID_METADATA_FIELDS_*`.
  - Emitted but not in the YAML: `METADATA_VALIDATION`, `UNKNOWN_METADATA_KEY`, `INVALID_BASE_GTS_ID` and the three `INVALIDATION_*` reasons.
- **Fix (decided, S-E2):** add the missing reasons (`TARGET_NOT_CONVERGED`, plus new ones for the `inv:` key rules and the quantity range, for example `RESERVED_KEY_PREFIX`, `KEY_ON_INVALIDATION`, `QUANTITY_OUT_OF_RANGE`) and remove the catalog-era ones. The YAML publishes every emitted name.

### 10.3 Metrics in the §3.11.5 inventory that are never built — high
[API-27]

- **Spec:** `DESIGN.md:1838-1871` lists:
  - `uc_declaration_mirror_write_failures_total`
  - `uc_type_resolution_duration_seconds{result}`
  - `uc_resolved_types`
  - `uc_declaration_cache_age_seconds`
  
  The "Declaration-cache staleness" alert (`:1896`) depends on them.
- **Code:** none are built (`usage-collector/src/infra/metrics.rs:65-92`, `domain/ports/metrics.rs:544-649`).
- **Fix:** add the cache-age, resolution-duration and resolved-types instruments now, from `type_resolver`. The mirror metric waits for 9.1 (deferred).

### 10.4 `uc_ingestion_records_total` / `uc_ingestion_requests_total` labels and coverage — medium
[API-28, API-29]

- **Spec:** `DESIGN.md:1838-1839`.
- **Code (records counter):**
  - It emits `unknown_usage_type` (should be `unresolved_type`), `semantics_violation` (should be `validation`) and an extra `metadata_size` (`domain/ports/metrics.rs:373-387`).
  - It maps `AlreadyInvalidated` to `invalidation_rule` (spec: `idempotency_conflict`; `service.rs:355`).
  - Per-entry rejections made in the REST handler before the service are never counted (`handlers/usage_records.rs:160-171`).
- **Code (requests counter):**
  - It has only `none`, `missing_security_context` (never emitted) and `plugin_error` (`metrics.rs:266-285`).
  - Empty and over-cap batches are not counted (`service.rs:1275-1283`), and single-entry SDK calls record nothing (`:1112-1121`).
- **Fix:** rename the values, fix the mappings, and count handler-level and single-entry rejections. See S-O1 for the unreachable label values.

### 10.5 Query metrics: point lookups not instrumented, label drift — medium
[API-30, QRY-27]

- **Spec:** `query_kind` is `aggregated`, `raw` or `point`, with `error_category` including `unresolved_type`, `undeclared_field` and `cursor_decode` (`DESIGN.md:1840`, `:1853`).
- **Code:**
  - There is no `Point` kind, and `get_usage_record` records nothing (`metrics.rs:393-409`, `service.rs:1782-1818`).
  - It emits `unknown_usage_type`, plus extra `filter_mismatch` and `order_mismatch`.
  - An undeclared key lands in `query_budget` (`service.rs:424-436`).
  - REST-edge rejections are not counted.
- **Fix:** instrument point lookups, rename the values, classify undeclared keys correctly, and count edge rejections.

### 10.6 No plugin-host span, no per-operation log with `correlation_id` — medium
[API-32]

- **Spec:** "The Plugin Host opens that span around each dispatch … Every accepted and rejected operation emits a structured log entry carrying the propagated `correlation_id`" (`DESIGN.md:1822-1825`; PRD `:878`).
- **Code:** `instrument_spi` opens no span (`service.rs:194-223`), and `correlation_id` appears nowhere.
- **Fix:** add a span in `instrument_spi` and an outcome log per operation.

### 10.7 Small observability differences — low
[API-33, API-34]

- **Meter scope name:** `"usage-collector"`, where the spec says `"usage_collector"` (`infra/metrics.rs:368` vs `DESIGN.md:379`).
- **`uc_query_inflight`:** an up/down counter, where the spec table says gauge (`metrics.rs:87`). This is fine in practice.

---

## 11. Stale code docs — low
[ING-22 and others]

Update these with the related fix:

- `UsageRecord.value` doc says the sign depends on counter/gauge semantics (`models.rs:883-885`). The spec says the sign is never constrained (`DESIGN.md:233-235`).
- `service.rs:63-65` claims the YAML has `maxItems`.
- Backfill "elevated authorization" docs (see 3.1).
- `plugin_api.rs` and SDK `error.rs` describe the atomic invalidation rule (see 2.1).
- Plugin README index and seven-check text (see 8.4).

---

## 12. Spec problems found along the way

These need a decision, not just a code change. The spec is still the source of
truth by default.

Many are now settled: see **Decisions** near the top, which wins over the
"Decide" text below.

- Items marked **blocks code** must be settled before the related code item
  can be done.
- The rest can be fixed in the spec alone.

Paths in this section are relative to `gears/system/usage-collector/docs/`.

### 12.0 The implementation commit edited the spec in three places

The implementation commit (`2f09940e3`) touched three spec files. One of those edits
breaks the rule that code is not a second source (`DESIGN.md:1930`).

- **`DESIGN.md:1842` (`uc_type_resolution_total` row).** The edit added `served_stale`, "a cached declaration served past its TTL", and marked `restored` as "not yet emitted, since that path is not yet implemented".
  - DESIGN defines no TTL (ADR-0008 says indefinite cache validity).
  - The mirror restore is normative (ADR-0015, accepted).
  - Implementation status belongs in §3.12.2, not in the instrument table.
  - **Decided: reverted.** `DESIGN.md` is back to the design-change text. `served_stale` returns to the spec with the S-T3 amendment. [TYP-2, SPEC-13]
- **`features/usage-emission.md`, get-record flow.** Better than before (it now authorizes before reading), but still off:
  - It puts the point lookup on the Ingestion Gateway, but DESIGN `:776` puts it on the Query Gateway.
  - It uses the old two-argument `get_usage_record(id, scope)`.
  - It cites "Method 10" / `sdk-trait.md`, neither of which exists.
  - It reuses one instance marker for three steps.
  - It has no telemetry step.
  
  It should move into the rewrite of the feature docs (12.9).
- **`schemas/example.stored_volume.v1.schema.json`:** the `x-gts-traits` move to the top level is fine. DESIGN and ADR-0008 do not prescribe the placement, and it is what types-registry reads.

### 12.1 Plugin API shape (blocks code)

- **S-C1 — Raw read makes the plugin mint cursors, against the stated principle (high). [SPEC-1, SPI-14]**
  - `DESIGN.md:336-341` says plugins never mint or read a wire cursor. The raw sequence (`:1503-1507`) and ADR-0011 `:106-107` agree.
  - But `list_usage_records(..., query: &ODataQuery, ...) -> ODataPage<UsageRecord>` (`:1067-1073`) carries a `CursorV1` in and `next_cursor` out.
  - The code follows the signature: TimescaleDB encodes and decodes `CursorV1` (`plugins/timescaledb-usage-collector-plugin/src/infra/storage/query/keyset.rs:49`, `:295-311`).
  - **Decide:** change the signature to keyset in / rows plus last keyset out (then move cursor minting into the Query Gateway), or amend the principle.
- **S-C2 — Converged-only lookup needs a finite bound some valid plugins don't have (medium). [SPEC-23]**
  - "Definite answer within the convergence bound plus query-path lag bound" (PRD `:437`; `DESIGN.md:1124-1128`).
  - But "eventually consistent with no upper bound" is a valid profile (`:1698-1700`; PRD `:828`).
  - **Decide:** require a finite bound for the converged lookup.
- **S-C3 — Noop plugin is not carved out of "every conforming plugin MUST pass" (low). [SPI-18]** `DESIGN.md:1133`. Exempt dev-only null backends, or say the noop plugin is non-conforming.

### 12.2 Invalidation, dedup, backfill

- **S-A1 — Which scope is used for the invalidation target lookup (high, blocks 2.4). [ING-6]** Ingestion authorization is a permit on the write tuple and yields no read scope. Name the PDP action whose constraints form the lookup scope (DESIGN §3.2 / §3.9.6).
- **S-B6 — Who resolves same-identity entries within one batch (low). [ING-15, SPEC-33]**
  - DESIGN `:568` and ADR-0004 `:186-188` say the gateway resolves them.
  - The `dedup-floor` check (`:1143`) asserts it at the plugin.
  - Also unstated:
    - an invalidation whose target is earlier in the same batch (likely `TargetNotConverged`);
    - the later of a same-identity pair whose earlier entry was rejected ("against the earlier *accepted*").
  - **Decide:** gateway only, or both (defence in depth), and state both outcomes.
- **S-B7 — Faithful copy and dedup: numeric or textual quantity equality (low). [ING-19]** `42.500` versus `42.5` is equal numerically but not "digit for digit" (`DESIGN.md:574-575`). State which.
- **S-R1 — Retention floor applies to every type, or only charging-consumer types (medium). [SPEC-4]**
  - PRD `:711` and ADR-0012 `:119`, `:325-334` say every type.
  - DESIGN `:595` and `:1757-1759` say charging-consumer types.
  - The idempotency horizon (`:570`) needs every type.
  - **Suggested:** fix DESIGN.
- **S-R2 — Retention floor cannot be enforced as written (medium). [SPEC-24, TYP-5]**
  - YAML `:280-282` and PRD `:727`, AC `:1466` say a deployment "must not admit" a window that leaves any type below the floor.
  - Elsewhere: "not gear-level enforcement" (PRD `:713`, AC `:1465`; `DESIGN.md:89`, `:1976`). Types are also added at run time with no gear action.
  - A later type with `retention: P30D` silently breaks the horizon guarantee.
  - **Decide:** check retention against the floor at resolution time or registration (with a signal), or downgrade the guarantee to a readiness-review obligation.
- **S-R3 — Retention is measured from "the covered period", not its end (low). [SPEC-20]** DESIGN `:570` and the schema. PRD `:707` and ADR-0012 `:318` say "end". Use "end" everywhere.
- **S-I1 — Backfill isolation has no mechanism or test (medium). [SPEC-29]** The obligation is gear-level (ADR-0012 `:167`; `DESIGN.md:104`, `:681`), but no budget, config or alert is defined. Specify it (for example separate concurrency budgets, plus an alert on live latency under backfill load), or drop the claim.
- **S-Q0 — Quotas per "calling gear" use an identity the platform does not carry (high, blocks 4.2). [SPEC-22]**
  - PRD fr-rate-limiting `:739` and `DESIGN.md:91`, `:667` key quotas on the calling gear. `DESIGN.md:244-247` claims `SecurityContext` "carries the calling-gear identity".
  - But `DESIGN.md:93` and PRD `:761` say the tenant plane carries none, and `libs/toolkit-security/src/context.rs:23-35` has only subject, tenant, token scopes and bearer token.
  - Also, "quota configuration" is listed as a REST-only operator operation (`DESIGN.md:921-922`; PRD `:939`), but no such endpoint exists [SPEC-15].
  - **Decide:**
    - key quotas on `subject_id` (or defer per-gear quotas);
    - fix the `SecurityContext` claim;
    - make quotas deployment config.

### 12.3 Query surface

- **S-Q1 — Which `$filter` logical operators are allowed (low). [QRY-9]** Fixed fields accept `eq`/`in` (`DESIGN.md:591`). Nothing says whether `and`/`or`/`not` are allowed.
- **S-Q2 — `group_by` set differs across documents (medium). [SPEC-6, QRY-26]**
  - DESIGN `:522` lists eight fixed fields.
  - YAML `AggregationDimension` (`:565`) lists five, and `maxItems: 2` (`:1133`).
  - PRD `:266-268` and AC `:1459` say "any combination and any order", limited only by the result cap.
  - **Decide:** one set (the code follows the YAML's five), and whether the cap of 2 stays.
- **S-Q3 — Aggregation result cap and query error slots (medium). [SPEC-7, QRY-14]**
  - YAML `:1185-1188` puts `AGGREGATION_RESULT_TOO_LARGE` in `context.reason`, but DESIGN `:1242` says an `InvalidArgument` reason rides `field_violations[0].reason`.
  - PRD `:268` calls the cap configurable; YAML fixes it at 100 000; DESIGN never mentions it.
  - Undeclared metadata or `group_by` keys: the YAML says `VALIDATION` (`:1055-1056`), while the code uses `UNKNOWN_METADATA_KEY`.
  - **Decide:** the cap's owner, whether it is configurable, the error slot, the reason code and a metric category.
- **S-Q4 — Cursor binding text in DESIGN is older than the YAML (medium). [SPEC-5, QRY-25]**
  - DESIGN `:1208-1213` says the cursor binds "the order and a hash of the filter" and lists `ORDER_MISMATCH`.
  - YAML `:509-516` binds the full query scope and has no `ORDER_MISMATCH`.
  - **Suggested:** the YAML wins. Decide whether `ORDER_MISMATCH` exists.
- **S-Q5 — `$orderby` "appends the pair" is wrong when the caller already names `id` or `window_end` (low). [SPEC-19, QRY-24]** YAML `:489-491`, DESIGN `:592`. Reword to "appends whichever of the pair is missing".
- **S-Q6 — Undocumented read parameters (low). [API-13, QRY-10, QRY-29]**
  - `$top` is served on `GET /records` but not documented.
  - `id` is accepted in `$filter` but not in the fixed field set (`DESIGN.md:522`).
  - `metadata.<key>` is described in prose only.
  - `from == to` is rejected, but the spec is silent.
  - **Decide:** document or refuse each.
- **S-Q7 — Empty aggregate results (low). [SPEC-34, QRY-13]** Specify per fold whether `SUM`/`COUNT` over nothing is `null` or `"0"`, and whether an empty ungrouped range returns one bucket. The code returns `"0"` for `COUNT`.
- **S-Q8 — "Query Gateway + every plugin" credits the gateway with rules only plugins enforce (low). [SPEC-18]** `DESIGN.md:573`, `:581`, `:585`. Change the rows to "every plugin (§3.3 contract test)".

### 12.4 Types and registry

- **S-T1 — Build the declaration mirror, or retire ADR-0015 (high, blocks 9.1). [TYP-1, SPEC-28, SPEC-31, SPEC-14]**
  - types-registry now has a DB-backed store (`gears/system/types-registry/types-registry/src/infra/storage/migrations/m20260817_000001_initial.rs`). ADR-0015's retirement condition (statement 7) may be close.
  - If the mirror stays, the spec must also settle three points:
    - Multi-replica: "mirror shared across replicas" (`DESIGN.md:1602-1604`) versus an in-process registry per binary (ADR-0015 `:143-146`).
    - Divergence: what the resolver does when the registry holds a different document under the same id than the mirror or cache.
    - Withdrawal: ADR-0008 `:159-161` says no removal operation exists, ADR-0015 `:109-113` says one does, and the unconditional restore would resurrect a withdrawal.
- **S-T2 — Who registers the reserved base type (high, blocks 9.2). [TYP-3, DIVERGENCES 25]** No document says. The PRD forbids the gear writing declarations except the restore (PRD `:1026`; `DESIGN.md:1327`). **Decide:** allow the gear to publish its own base as link-time content, or name the owner.
- **S-T3 — Declaration cache model (medium). [TYP-7, SPEC-13]** ADR-0008 statement 3 and PRD `:1027` say indefinite validity. DESIGN mentions "refresh and staleness accounting" (`:719`) and an alert on a "configured refresh interval" (`:1896`) but defines neither. **Decide:** indefinite, or TTL plus refresh (with config and outage behaviour), and whether eviction may drop the outage fallback.
- **S-T4 — Retention trait format (medium). [TYP-6]** The schema's `retention` is a free-text ISO 8601 string (`schemas/usage_record.v1.schema.json:56-60`), so `P1M` is ambiguous and invalid values pass registration. Add a pattern allowing fixed-length durations only.
- **S-T5 — Missing-fold alternative flow is unreachable (low). [TYP-8]** PRD `:1163` versus registration-time trait validation. Delete or reword it.
- **S-T6 — Resolver content in DESIGN (low). [TYP-9, SPEC-12]**
  - The emit sequence `:1397` returns "retention", but the resolver does not serve retention (`:716-718`).
  - `:715-716` exposes the nominal sampling interval, but PRD `:477` says the gear never reads it.
- **S-T7 — Base schema implies an open metadata surface is possible (low). [TYP-11]** `schemas/usage_record.v1.schema.json:150` versus `DESIGN.md:590` "no open-extras escape hatch". Reword the schema.
- **S-T8 — Lost declaration means data is kept forever (low). [TYP-10]** Record this in ADR-0015 Consequences.

### 12.5 Authorization

- **S-P1 — PEP `resource_id` binding is both fixed and open (medium). [SPEC-26, API-4]** The §3.9.6 table (`DESIGN.md:1667`) binds it to `resource_ref.resource_id`, and the code does the same. The §4 open item (`:1986-1990`) says it is "not settled". Close the open item.
- **S-P2 — No normative PDP action and permission list (low). [API-2]** DESIGN names no action vocabulary (`create`/`get`/`list`). Add one to §3.9.6.
- **S-P3 — Property name and subtree support (low). [API-3]** Either the code moves to `subject_id` and supports `InTenantSubtree`, or the §3.9.6 row changes.

### 12.6 Errors and metrics vocabularies

- **S-E1 — Error variants (low). [API-17, SPEC-10, SPEC-11]**
  - `PluginUnavailable` is named in `DESIGN.md:1330`, `:847`, `:1284` but has no variant. State that it maps to `ServiceUnavailable`.
  - `Unauthenticated` / `missing_security_context` are referenced but not modelled; in process the SDK requires `&SecurityContext`.
  - The code's `CursorRejected`: document it or fold it into `InvalidArgument`.
- **S-E2 — Rejection reason vocabulary is incomplete (medium). [SPEC-8, ING-20, API-18, DIVERGENCES 6]**
  - The PRD requires distinct actionable errors that have no published reason: faithful-copy mismatch naming the field, both-or-neither, target is an invalidation, key on an invalidation or `inv:` prefix, precision and control characters, unknown metadata key, metadata size, quantity range.
  - The code already emits `METADATA_VALIDATION`, `UNKNOWN_METADATA_KEY`, `INVALID_BASE_GTS_ID` and three `INVALIDATION_*` reasons.
  - **Decide:** enumerate them in the YAML, with the slot each uses.
- **S-O1 — Metric label sets are inconsistent (medium). [SPEC-9, API-28, API-29, API-30, API-31]** In DESIGN §3.11.5:
  - `uc_pdp_duration_seconds` says "nine-value set", but the set has seven values.
  - `missing_time_range` is unreachable because the range is typed.
  - There is no category for filter or cursor mismatch or the bucket cap.
  - The records counter lacks `metadata_size`/`quota`.
  - Both-or-neither and target-is-invalidation have no category.
  - `uc_plugin_accept_errors_total` uses `unready`/`timeout`, but the plugin error enum has no such variants.
  - `outcome="duplicate"` exists, but the wire has no duplicate outcome.
  - Request-level `authz`/`unresolved_type`/`metadata_size` cannot fire when these are per entry.
  - The workload-isolation alert `:1893` lacks `origin="live"`.
  - **Decide:** reconcile against the error taxonomy in one pass.
- **S-O2 — Small metric wording (low). [API-33, API-34]** Meter scope `usage_collector` versus the code's `usage-collector`. Accept an up/down counter as the realisation of the `uc_query_inflight` "gauge".

### 12.7 Sequences, configuration, contract tests

- **S-S1 — DESIGN sequences disagree with the component text (low). [SPEC-12]**
  - Quota placement: before PDP in the sequence, absent from §3.3 rule 1.
  - The invalidate sequence skips type resolution and metadata validation, and calls `get_usage_record` without `scope`.
  - The sequences show no post-permit scope check.
  - There is no point-lookup sequence.
- **S-S2 — Plugin binding: lazy or at startup (medium). [SPEC-3]**
  - "Lazily on first dispatch" (`DESIGN.md:84`, `:297`, `:602`, `:837`; ADR-0002 `:80`).
  - Versus "binds once at startup" (`:1278`, `:1301`) and "required for readiness" (PRD `:1041`, `:1487`).
  - **Suggested:** keep lazy; fix §3.3 and the PRD.
- **S-S3 — No configuration table (low). [SPEC-30]** These are referenced but never named together with defaults:
  - batch cap;
  - live tolerances;
  - backfill window;
  - replay horizon;
  - metadata size cap;
  - cache TTL/refresh;
  - quota limits.
  
  Add a DESIGN table.
- **S-S4 — Contract table misses DESIGN's own plugin obligations (low). [SPEC-35, SPI-19]** No checks exist for:
  - keyset pagination and `$orderby`;
  - bucket key encoding;
  - recomputation after invalidation;
  - "acknowledge only what is durable";
  - the idempotency horizon (ADR-0004 `:318-326`).
  
  Add the rows or mark them review-only.
- **S-S5 — Contract status text is wrong (medium). [SPEC-16, API-35, QRY-18]**
  - `DESIGN.md:1930-1931` and the YAML header (`usage-collector-v1.yaml:7-35`) say the six contract checks are `#[ignore]`d and the code is Phase 1.
  - In fact the checks run live, with three self-expiring excuse lists (`NOT_YET_IMPLEMENTED`, `UNDOCUMENTED_PARAMETERS`, `BODY_VS_QUERY_DRIFT` in `usage-collector/src/api/rest/routes/openapi_contract_tests.rs`), and the code implements most of the new model.
  - Nothing ties `x-contract-status` to a test any more.
  - **Decide:** refresh both texts. Keeping `unreleased` is right while feed and reconciliation are missing. Either add a test tying the marker to an empty `NOT_YET_IMPLEMENTED`, or drop the "a live test holds the two together" sentence.

### 12.8 PRD editorial fixes (low) [SPEC-17, ING-21, API-26]

- `PRD.md:825`: "catalog query surfaces" (no catalog).
- `:831`: cites DESIGN §5.1, which does not exist (should be §3.10).
- `:487`: "persisted `value`" (field is `quantity`).
- `:648`: "the explicit entry type" (entry type is derived).
- `:1508`: risks mention GTS type registration and deletion by operators.
- `:1034`, `:1481`: `contract-gts-registry` versus DESIGN's `contract-types-registry` (ADR-0008 `:135` also cites the old id).
- `:691` "MUST be able to carry" versus `:470-475` "MUST carry" retention.
- `:981`, `:993`: "Health" REST capability, but probes belong to the host (`DESIGN.md:1182-1183`).

### 12.9 Stale secondary docs [SPEC section 2, TYP-12]

They are all quarantined in `.cf-studio/config/artifacts.toml:74-82`. They also have problems in common:

- they link files that no longer exist (`sdk-trait.md`, `plugin-spi.md`);
- they link old ADR file names;
- they use the old ten-method plugin numbering;
- none covers backfill, invalidation, the declared fold, the covered-period model, dedup levels, the Type Resolver or quotas.

| Doc | Still describes | Verdict |
| --- | --- | --- |
| `DECOMPOSITION.md` | Catalog, deactivation and compensation features, `created_at` dedup and keyset, `AVG`, about 30 dangling ids. Assigns none of the current FRs. | Rewrite |
| `features/foundation.md` | Ten plugin methods, catalog gauge, deactivation wiring | Trim and resync |
| `features/usage-emission.md` | Catalog lookup, `created_at` dedup, compensation matrix, label-free duration | Rewrite |
| `features/usage-query.md` | `created_at` filters, caller-supplied op incl. `AVG`, active/inactive | Rewrite (cursor and OData parts reusable) |
| `features/usage-type-lifecycle.md` | Catalog CRUD (forbidden by `constraint-no-type-catalog`) | Delete |
| `features/event-deactivation.md` | Deactivation and cascade (DESIGN `:607`: "no deactivation handler") | Delete; write an invalidation feature |

### 12.10 DIVERGENCES.md against the current spec

`DIVERGENCES.md` was written against an older spec. Verdicts:

- **(a)** resolved by the spec;
- **(b)** still a spec problem (already folded into this section);
- **(c)** a code problem (already folded into sections 1–11);
- **(d)** not a spec matter.

| Entries | Verdict |
| --- | --- |
| 21 | (a) — the store-side rule is gone from the spec |
| 2, 17, 19 | partly (a): the YAML cursor text was fixed but DESIGN was not; `metadata.<key>` is documented but `$top` is not; `latest-tie-break` is now writable |
| 1, 3, 5, 6, 11, 13, 14, 15, 25, §G | (b) — see S-Q5, S-Q4, 12.9, S-E2, S-Q8, S-O1, S-Q2, S-T2 |
| 4, 9 | (b) for label sets (S-O1), (c) for renamed labels (10.4) |
| 7, 8, 10, 12, 18, 22 | (c) — see 7.2, 1.4, 1.1/1.2/1.5, 3.2, 8.1, 2.1 |
| 16, 20, 23, 24, §A, §B, §D, §E | (d) |
| §C, §F | (d) for the code parts; the spec parts are 12.9 (no feature owns backfill) and S-C1 / S-S4 (cursor minting, no keyset contract check) |

Once this file is acted on, `DIVERGENCES.md` can be retired.
