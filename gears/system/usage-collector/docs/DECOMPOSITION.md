# Decomposition: Usage Collector

Splits the Usage Collector gear's PRD and DESIGN scope into fourteen
independently implementable and testable features, spanning the ingestion,
query, feed, typing, and pluggable-storage components plus the cross-cutting
attribution, backfill, operator, and NFR concerns that bind them.

<!-- toc -->

- [1. Overview](#1-overview)
- [2. Entries](#2-entries)
  - [2.1 Attribution, Authorization & Tenant Isolation - HIGH](#21-attribution-authorization--tenant-isolation---high)
  - [2.2 GTS Usage Type Resolution & Declaration Binding - HIGH](#22-gts-usage-type-resolution--declaration-binding---high)
  - [2.3 Pluggable Storage & Plugin Hosting - HIGH](#23-pluggable-storage--plugin-hosting---high)
  - [2.4 Usage Record Ingestion & Identity - HIGH](#24-usage-record-ingestion--identity---high)
  - [2.5 Record Invalidation & Corrections - HIGH](#25-record-invalidation--corrections---high)
  - [2.6 Usage Query — Raw & Aggregated - HIGH](#26-usage-query--raw--aggregated---high)
  - [2.7 Usage Feed for Downstream Consumers - HIGH](#27-usage-feed-for-downstream-consumers---high)
  - [2.8 Backfill Import & Retention Governance - MEDIUM](#28-backfill-import--retention-governance---medium)
  - [2.9 Ingestion Rate Limiting & Reconciliation Metadata - MEDIUM](#29-ingestion-rate-limiting--reconciliation-metadata---medium)
  - [2.10 Data Classification & Privacy Boundary - LOW](#210-data-classification--privacy-boundary---low)
  - [2.11 Read-Path Consistency & Freshness Contract - HIGH](#211-read-path-consistency--freshness-contract---high)
  - [2.12 Throughput, Latency & Availability SLOs - HIGH](#212-throughput-latency--availability-slos---high)
  - [2.13 Public Surface Contract Stability & Versioning - MEDIUM](#213-public-surface-contract-stability--versioning---medium)
  - [2.14 Operational Visibility & Telemetry - MEDIUM](#214-operational-visibility--telemetry---medium)
- [3. Feature Dependencies](#3-feature-dependencies)

<!-- /toc -->

**Overall implementation status:**
- [ ] `p1` - **ID**: `cpt-cf-usage-collector-status-usage-collector`

## 1. Overview

This decomposition splits the Usage Collector gear into fourteen features. The
split follows two axes at once, because neither one alone produces
independently testable units.

The first axis is the DESIGN §3.2 component model. Four of the fourteen
features map directly onto a domain component that owns a distinct
responsibility on the request path: the Ingestion Gateway (usage record
ingestion, and — because invalidation rides the same choke point — record
invalidation), the Query Gateway (raw and aggregated usage query), the Feed
Gateway (the downstream usage feed), and the Type Resolver (GTS type
resolution). A fifth component, the Plugin Host, becomes its own feature
because pluggable storage is a first-class product requirement (PRD §5.4),
not merely an implementation seam.

The second axis is cross-cutting concerns that no single component owns
outright: PDP-anchored attribution and tenant isolation gate every component;
backfill and retention governance span the Ingestion Gateway and the storage
plugin; rate limiting is an ingestion-side operator surface, while
reconciliation metadata is read through the Query Gateway as its fourth read
path; data classification is a data-handling boundary rather than a code
path; and the NFR envelope (throughput, latency, availability, consistency
freshness, contract stability, and operational visibility) binds every
component simultaneously rather than any one of them. Each of these becomes
its own feature so that its acceptance criteria, and the tests that verify
them, are not buried inside a single component's feature.

**Two feature shapes.** Entries 2.1 through 2.9 are component-owning
capability features: each anchors to at least one DESIGN component, and most
expose an API endpoint or a named sequence a test can drive end to end.
Entries 2.10 through 2.14 are cross-cutting contract and quality-attribute
features: each states a rule, a boundary, or a numeric envelope that binds
every component at once rather than living inside one of them. A
contract-shaped entry is verified differently: its acceptance criteria are
checked as assertions embedded in the sequences and endpoints owned by the
capability features it constrains, and it is owned, for review and change
control, by whichever team maintains the ADR or DESIGN section that states
the rule.

**Why `Data: None` recurs.** The Usage Collector owns exactly one durable,
gear-side table — a temporary declaration mirror described in DESIGN §3.7 —
and no `db` or `dbtable` component IDs are defined anywhere in DESIGN,
because the entry ledger itself is wholly plugin-owned and reached only
through the Plugin SPI (`cpt-cf-usage-collector-principle-pluggable-storage`,
ADR `cpt-cf-usage-collector-adr-pluggable-storage`). Every feature below
therefore carries `Data: None` for its gear-owned schema; this is stated once
here rather than repeated as a caveat in every entry.

Features are ordered so that foundational, dependency-free features
(attribution and authorization, GTS type resolution, and pluggable storage)
precede the features that consume them (ingestion, invalidation, query, and
backfill), which in turn precede the ingestion-adjacent operator features
(rate limiting, reconciliation, data classification) and the cross-cutting
NFR features (consistency and freshness, throughput and latency, contract
stability, and operational visibility). The usage feed (2.7) is the one
exception to this order: it is numbered and depends ahead of backfill and
retention governance (2.8) even though 2.8 is later in the list, because the
feed's cursor-refusal behavior must conform to the retention floor, whose
backfill-window term is a value backfill and retention governance
configures, even though the storage plugin enforces the refusal itself.

## 2. Entries

### 2.1 [Attribution, Authorization & Tenant Isolation](feature-attribution-authorization/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-feature-attribution-authorization`

- **Purpose**: Establishes the PDP-anchored security boundary that every
  write and read operation must pass before it touches domain logic: caller-
  supplied tenant, resource, and subject attribution, verified against the
  platform PDP, with tenant isolation enforced independently per tenant scope
  and no implicit cross-tenant access. This is the architectural decision
  recorded in `cpt-cf-usage-collector-adr-pdp-centric-authorization` and
  `cpt-cf-usage-collector-adr-caller-supplied-attribution`: the gear never
  derives identity from the caller's own `SecurityContext`, and it never
  caches a PDP decision or relaxes a denial.

- **Depends On**: None

- **Scope**:
  - Structural validation and PDP authorization of the caller-supplied
    attribution tuple (tenant, resource, optional subject) on every
    ingestion, query, and feed operation.
  - Independent per-tenant PDP scope evaluation, including parent-to-subtenant
    and platform-administrative cross-tenant scenarios.
  - Fail-closed behavior on PDP unavailability or denial across every
    surface.
  - Applying PDP-returned constraints as query filters before any
    user-supplied filter narrows the result further.

- **Out of scope**:
  - The domain validation that runs after attribution is authorized
    (covered by usage record ingestion, query, and invalidation features).
  - Ingestion quota enforcement, which is a separate, quota-keyed control
    (covered by rate limiting and reconciliation).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-fr-tenant-attribution`
  - [ ] `p1` - `cpt-cf-usage-collector-fr-resource-attribution`
  - [ ] `p1` - `cpt-cf-usage-collector-fr-subject-attribution`
  - [ ] `p1` - `cpt-cf-usage-collector-fr-tenant-isolation`
  - [ ] `p1` - `cpt-cf-usage-collector-fr-ingestion-authorization`

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-principle-pdp-centric-authorization`
  - [ ] `p1` - `cpt-cf-usage-collector-principle-fail-closed`

- **Design Constraints Covered**: None — the PII-identity-layer constraint is
  owned by data classification and privacy boundary, which states the
  identifier-versus-PII distinction directly; attribution and authorization
  only consumes opaque identifiers that constraint defines.

- **Domain Model Entities**:
  - SecurityContext
  - ResourceRef
  - SubjectRef

- **Design Components**:

  - [ ] `p1` - `cpt-cf-usage-collector-component-ingestion-gateway`
  - [ ] `p1` - `cpt-cf-usage-collector-component-query-gateway`
  - [ ] `p1` - `cpt-cf-usage-collector-component-feed-gateway`

- **API**:
  - POST /usage-collector/v1/records (authorization gate)
  - GET /usage-collector/v1/records (authorization gate)
  - POST /usage-collector/v1/records/aggregate (authorization gate)
  - GET /usage-collector/v1/feed (authorization gate)

- **Sequences**: None — PDP authorization is a cross-cutting step embedded in
  every sequence listed under the ingestion, query, invalidation, and feed
  features rather than a sequence of its own.

- **Data**: None (see §1 Overview).

### 2.2 [GTS Usage Type Resolution & Declaration Binding](feature-usage-type-resolution/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-feature-usage-type-resolution`

- **Purpose**: Resolves every `gts_type_id` reference to its `types-registry`-
  owned declaration (aggregation fold, canonical metering unit, metadata
  surface, retention policy, optional nominal sampling interval), serves the
  steady state from a local cache, and recovers a declaration the registry has
  lost. The Usage Collector mints no type of its own and maintains no second
  catalog, per `cpt-cf-usage-collector-adr-registry-owned-typing` and
  `cpt-cf-usage-collector-adr-declared-fold`; declaration mirroring and
  restore behavior follow `cpt-cf-usage-collector-adr-declaration-rehydration`.

- **Depends On**: None

- **Scope**:
  - Resolution of a GTS type reference to its declaration on the ingestion
    and query paths, fail-closed on an unresolvable reference.
  - The declared aggregation fold as a closed, immutable, per-type property.
  - The declared canonical metering unit, bound at registration and refused
    if absent or non-canonical.
  - Caching, refresh, and best-effort mirror/restore of resolved
    declarations to keep ingestion available through a registry outage.

- **Out of scope**:
  - Minting, amending, or withdrawing a GTS type declaration — that is a
    `types-registry` operation this gear never exposes.
  - Applying the resolved fold to a query result (covered by usage query).
  - Validating a record's metadata content against the resolved schema
    (covered by usage record ingestion).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-fr-usage-type-declaration`
  - [ ] `p1` - `cpt-cf-usage-collector-fr-usage-type-resolution`
  - [ ] `p1` - `cpt-cf-usage-collector-fr-aggregation-fold`
  - [ ] `p2` - `cpt-cf-usage-collector-fr-metering-unit-binding`

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-principle-registry-owned-typing`
  - [ ] `p1` - `cpt-cf-usage-collector-principle-declared-fold`

- **Design Constraints Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-constraint-no-type-catalog`

- **Domain Model Entities**:
  - AggregationFold
  - MeterTypeId

- **Design Components**:

  - [ ] `p1` - `cpt-cf-usage-collector-component-type-resolver`

- **API**: None — GTS type declarations have no endpoint, read or write, on
  any Usage Collector surface (PRD §7.1); resolution is internal to the
  ingestion and query paths.

- **Sequences**: None — resolution is a step embedded in the emit, invalidate,
  and query sequences owned by other features, not a sequence of its own.

- **Data**: None (see §1 Overview).

### 2.3 [Pluggable Storage & Plugin Hosting](feature-pluggable-storage/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-feature-pluggable-storage`

- **Purpose**: Provides the single seam through which every domain component
  reaches durable state: a Plugin Host that lazily resolves the operator-
  selected storage backend through `types-registry` and `ClientHub`, and
  dispatches persistence, query, and feed calls to it. The core carries no
  compile-time dependency on any plugin crate and no backend-specific SQL,
  schema, or client library, per `cpt-cf-usage-collector-adr-pluggable-storage`.

- **Depends On**: None

- **Scope**:
  - Lazy resolution and caching of the bound storage plugin instance for the
    service's lifetime.
  - Dispatch of persistence, raw query, aggregated query, and feed calls to
    the active plugin, and classification of plugin errors into the gear's
    error taxonomy.
  - Operator selection of the active backend via configuration, without a
    change to Usage Collector product behavior.
  - Fail-closed unavailability handling when no plugin is bound.

- **Out of scope**:
  - The plugin's own storage schema, retention mechanism, and materialized
    views, which are plugin-internal per `DATA-DESIGN-NO-001`.
  - Domain validation of a record before it reaches the plugin (covered by
    usage record ingestion).
  - The consistency and freshness bounds a plugin must publish (covered by
    the consistency and freshness contract feature).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-fr-pluggable-storage`

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-principle-pluggable-storage`
  - [ ] `p1` - `cpt-cf-usage-collector-principle-plugin-resolution-via-client-hub`

- **Design Constraints Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-constraint-vendor-pluggable`

- **Domain Model Entities**:
  - Keyset
  - FeedPosition

- **Design Components**:

  - [ ] `p1` - `cpt-cf-usage-collector-component-plugin-host`

- **API**: None — the Plugin SPI is an internal Rust trait contract
  (`cpt-cf-usage-collector-interface-plugin`) implemented by storage
  extensions, not a REST or CLI surface.

- **Sequences**: None — plugin dispatch is a step embedded in every sequence
  owned by the ingestion, invalidation, query, feed, and backfill features.

- **Data**: None (see §1 Overview).

### 2.4 [Usage Record Ingestion & Identity](feature-usage-record-ingestion/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-feature-usage-record-ingestion`

- **Purpose**: Accepts a **Usage Record** on the live ingestion path,
  validates its covered period, quantity, and per-type metadata, derives its
  server-assigned identity, and deduplicates it against prior submissions
  under the same dedup identity. This is the ledger's write path decided by
  `cpt-cf-usage-collector-adr-mandatory-idempotency`,
  `cpt-cf-usage-collector-adr-record-identity-derivation`, and
  `cpt-cf-usage-collector-adr-quantity-precision`.

- **Depends On**: `cpt-cf-usage-collector-feature-attribution-authorization`,
  `cpt-cf-usage-collector-feature-usage-type-resolution`,
  `cpt-cf-usage-collector-feature-pluggable-storage`

- **Scope**:
  - Structural validation of the covered period (half-open, UTC, live-path
    future and past tolerance), the signed quantity (finite decimal, within
    the published range and precision), and the closed per-type metadata
    surface.
  - Client-provided idempotency key handling: silent absorption of an
    exact-equality retry, fail-closed conflict on a divergent field, and
    resolution of a race by the plugin's declared dedup level.
  - Server-derived, offline-reproducible entry identity (UUIDv5 over the
    six-part dedup identity) and acceptance-instant stamping.
  - Enforcement of the canonical metering unit and the fold-relative meaning
    of a quantity under the entry's covered period.

- **Out of scope**:
  - PDP authorization of the attribution tuple (covered by attribution and
    authorization).
  - GTS type resolution itself (covered by usage type resolution).
  - Invalidation-specific validation — faithful-copy checking, target
    resolution, and reason-code enforcement (covered by record invalidation).
  - The dedicated backfill route and its window bound (covered by backfill
    and retention governance).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-fr-ingestion`
  - [ ] `p1` - `cpt-cf-usage-collector-fr-idempotency`
  - [ ] `p1` - `cpt-cf-usage-collector-fr-record-metadata`
  - [ ] `p1` - `cpt-cf-usage-collector-fr-record-identity`
  - [ ] `p1` - `cpt-cf-usage-collector-fr-usage-windows`
  - [ ] `p2` - `cpt-cf-usage-collector-fr-live-future-time-bound`
  - [ ] `p2` - `cpt-cf-usage-collector-fr-canonical-units`
  - [ ] `p1` - `cpt-cf-usage-collector-fr-record-quantity`
  - [ ] `p1` - `cpt-cf-usage-collector-fr-quantity-semantics`

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-principle-idempotency-by-key`
  - [ ] `p1` - `cpt-cf-usage-collector-principle-canonical-errors`

- **Design Constraints Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-constraint-no-business-logic`

- **Domain Model Entities** (`cpt-cf-usage-collector-entity-model`):
  - UsageRecord
  - CreateUsageRecord
  - EntryType
  - RecordOrigin
  - IdempotencyKey
  - RecordMetadata

- **Design Components**:

  - [ ] `p1` - `cpt-cf-usage-collector-component-ingestion-gateway`

- **API**:
  - POST /usage-collector/v1/records (usage_collector.create_usage_records)
  - SDK trait: in-process **Usage Record** submission
    (`cpt-cf-usage-collector-interface-sdk-client`)

- **Sequences**:

  - [ ] `p1` - `cpt-cf-usage-collector-seq-emit-usage`

- **Data**: None (see §1 Overview).

### 2.5 [Record Invalidation & Corrections](feature-record-invalidation/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-feature-record-invalidation`

- **Purpose**: Implements the sole correction mechanism the ledger offers: an
  appended invalidation entry that faithfully copies its target's
  caller-supplied fields, carries a reason code, and withdraws the target from
  every fold while both entries remain persisted and readable. This keeps the
  ledger append-only in the strict sense, per
  `cpt-cf-usage-collector-adr-append-only-invalidation`.

- **Depends On**: `cpt-cf-usage-collector-feature-usage-record-ingestion`,
  `cpt-cf-usage-collector-feature-attribution-authorization`,
  `cpt-cf-usage-collector-feature-usage-type-resolution`,
  `cpt-cf-usage-collector-feature-pluggable-storage`

- **Scope**:
  - Explicit `entry_type` discrimination between a **Usage Record** and an
    invalidation entry, with no default and no inference from quantity.
  - Target resolution from the invalidation entry's own tenant, GTS type,
    idempotency key, and covered period, converged-only.
  - Faithful-copy validation of resource, subject, quantity, and metadata
    against the resolved target.
  - Mandatory, non-empty reason code on every invalidation entry, and its
    rejection on an ordinary **Usage Record**.
  - At-most-one-invalidation-per-record enforcement via the shared dedup
    identity, and the impossibility of invalidating an invalidation.

- **Out of scope**:
  - The withdrawal exclusion rule applied inside a fold (covered by usage
    query) and inside the feed (covered by the usage feed feature).
  - Path-owned period bounds for a backfilled invalidation (covered by
    backfill and retention governance).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-fr-record-invalidation`
  - [ ] `p2` - `cpt-cf-usage-collector-fr-invalidation-reason-code`

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-principle-append-only-ledger`

- **Design Constraints Covered**: None — this feature is bound entirely by
  the requirements and the principle above; no §2.2 constraint constrains it
  beyond what usage record ingestion already carries.

- **Domain Model Entities**:
  - UsageRecord (invalidation entry)
  - RecordOrigin

- **Design Components**:

  - [ ] `p1` - `cpt-cf-usage-collector-component-ingestion-gateway`

- **API**:
  - POST /usage-collector/v1/records (entry_type=invalidation)
  - POST /usage-collector/v1/records/backfill (entry_type=invalidation)

- **Sequences**:

  - [ ] `p1` - `cpt-cf-usage-collector-seq-invalidate-record`

- **Data**: None (see §1 Overview).

### 2.6 [Usage Query — Raw & Aggregated](feature-usage-query/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-feature-usage-query`

- **Purpose**: Serves the two read-side query surfaces over accepted
  entries: a raw, cursor-paginated ledger read returning persisted fact, and
  an aggregated read serving the queried GTS type's declared fold as a
  derived view. Both are bounded to exactly one GTS type and a mandatory time
  range, and both apply PDP-returned constraints ahead of any user filter, per
  `cpt-cf-usage-collector-adr-window-end-selection` and
  `cpt-cf-usage-collector-adr-feed-aggregate-split`.

- **Depends On**: `cpt-cf-usage-collector-feature-usage-record-ingestion`,
  `cpt-cf-usage-collector-feature-record-invalidation`,
  `cpt-cf-usage-collector-feature-attribution-authorization`,
  `cpt-cf-usage-collector-feature-usage-type-resolution`,
  `cpt-cf-usage-collector-feature-pluggable-storage`

- **Scope**:
  - Aggregated query: mandatory time range and single GTS type, grouping and
    equality filtering on fixed dimensions and declared metadata properties,
    serving the declared fold and excluding withdrawn pairs.
  - Raw query: mandatory time range and single GTS type, cursor-paginated
    keyset scan over `(window_end, id)`, returning withdrawn pairs as
    persisted with unstripped billing fields.
  - Point lookup of a single entry by its identifier.
  - Period-end range selection (`from <= window_end < to`) as the sole
    admissible comparison of period against range on every read path.

- **Out of scope**:
  - The usage feed's replay-safe, snapshot-consistent read path (covered by
    the usage feed feature).
  - Operator-only reconciliation counters (covered by rate limiting and
    reconciliation).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-fr-query-aggregation`
  - [ ] `p1` - `cpt-cf-usage-collector-fr-query-raw`
  - [ ] `p1` - `cpt-cf-usage-collector-fr-billing-fields-on-read`

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-principle-aggregate-asymmetry`
  - [ ] `p1` - `cpt-cf-usage-collector-principle-canonical-page`
  - [ ] `p1` - `cpt-cf-usage-collector-principle-cursor-gateway-ownership`

- **Design Constraints Covered**: None — this feature inherits its
  constraints from the features it depends on; no §2.2 constraint applies
  uniquely to query.

- **Domain Model Entities**:
  - UsageRecordFilterField
  - AggregationDimension
  - AggregationResult
  - Keyset

- **Design Components**:

  - [ ] `p1` - `cpt-cf-usage-collector-component-query-gateway`

- **API**:
  - GET /usage-collector/v1/records (usage_collector.list_usage_records)
  - GET /usage-collector/v1/records/{id} (usage_collector.get_usage_record)
  - POST /usage-collector/v1/records/aggregate
    (usage_collector.query_aggregated_usage_records)

- **Sequences**:

  - [ ] `p1` - `cpt-cf-usage-collector-seq-query-aggregated`
  - [ ] `p1` - `cpt-cf-usage-collector-seq-query-raw`

- **Data**: None (see §1 Overview).

### 2.7 [Usage Feed for Downstream Consumers](feature-usage-feed/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-feature-usage-feed`

- **Purpose**: Provides the pull-based, deterministic, replay-safe read path
  a charging consumer depends on: per-GTS-type subscription, opaque bounded
  cursors, snapshot-consistent pages, and corrections delivered as ordinary
  entries at their own feed position after the entry they withdraw. This is
  the split decided by `cpt-cf-usage-collector-adr-feed-aggregate-split` and
  the recovery objective bound by `cpt-cf-usage-collector-nfr-replay-throughput`.

- **Depends On**: `cpt-cf-usage-collector-feature-usage-record-ingestion`,
  `cpt-cf-usage-collector-feature-record-invalidation`,
  `cpt-cf-usage-collector-feature-attribution-authorization`,
  `cpt-cf-usage-collector-feature-pluggable-storage`,
  `cpt-cf-usage-collector-feature-backfill-retention` (a later-numbered
  entry: the retention floor's backfill-window term is a value backfill and
  retention governance configures, so the feed's conformance to that floor
  depends forward on it, even though the storage plugin enforces the
  cursor refusal itself)

- **Scope**:
  - Subscription to a caller-declared set of GTS types, excluded from
    everything else including the cursor.
  - Deterministic feed order with corrections following their target, and a
    defined start position (`Oldest`) with no head-start option.
  - Snapshot-consistent, cursor-paginated pages bounded in size regardless of
    subscription breadth. A cursor whose continuation is intact is served,
    but a cursor after which retention has removed an entry of a subscribed
    GTS type is refused rather than served as a short page.
  - Scope-change handling: a narrowed authorization scope skips silently as
    the cursor advances; a widened scope requires consumer-side bootstrap.

- **Out of scope**:
  - The retention floor formula and the backfill window it composes with
    (covered by backfill and retention governance).
  - Reconciliation-based detection of an unintended scope narrowing (covered
    by rate limiting and reconciliation).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-fr-billing-usage-feed`
  - [ ] `p1` - `cpt-cf-usage-collector-nfr-billing-feed-freshness`
  - [ ] `p2` - `cpt-cf-usage-collector-nfr-replay-throughput`

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-principle-cursor-gateway-ownership`

- **Design Constraints Covered**: None — the feed's constraints are the ones
  already carried by pluggable storage and contract stability; no §2.2
  constraint applies uniquely to it.

- **Domain Model Entities**:
  - FeedSubscription
  - FeedPage
  - FeedPosition
  - FeedStart

- **Design Components**:

  - [ ] `p1` - `cpt-cf-usage-collector-component-feed-gateway`

- **API**:
  - GET /usage-collector/v1/feed (usage_collector.read_usage_feed)

- **Sequences**:

  - [ ] `p1` - `cpt-cf-usage-collector-seq-read-feed`

- **Data**: None (see §1 Overview).

### 2.8 [Backfill Import & Retention Governance](feature-backfill-retention/) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-usage-collector-feature-backfill-retention`

- **Purpose**: Provides the dedicated, permission-gated bulk-import path for
  historical entries, isolated from live ingestion workload, and ties it to
  the retention floor every GTS type's declaration must meet so that no
  admitted entry ever outlives its own idempotency and replay horizon. This is
  the isolation decision in `cpt-cf-usage-collector-adr-backfill-isolation`.

- **Depends On**: `cpt-cf-usage-collector-feature-usage-record-ingestion`,
  `cpt-cf-usage-collector-feature-usage-type-resolution`,
  `cpt-cf-usage-collector-feature-attribution-authorization`,
  `cpt-cf-usage-collector-feature-pluggable-storage`

- **Scope**:
  - Dedicated backfill route that admits **Usage Records** and invalidation
    entries alike, with a configurable, hard window bound, an origin marker
    on every entry it admits, and validation identical to the live path
    except for the past tolerance it replaces. The invalidation rules
    applied to an imported invalidation entry are owned by
    `record-invalidation`, not by this feature.
  - A permission distinct from live ingestion, required for every entry the
    route admits, whatever the entry kind.
  - Workload isolation from live ingestion so backfill load never breaches
    live-path SLOs.
  - The retention floor formula (`backfill window + operational replay
    horizon`) and its revalidation obligation whenever either term widens.

- **Out of scope**:
  - The live ingestion path's own validation rules (covered by usage record
    ingestion).
  - The feed's servable-cursor behavior at the retention boundary (covered by
    the usage feed feature, which depends on this one).

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-usage-collector-fr-backfill`
  - [ ] `p2` - `cpt-cf-usage-collector-fr-billing-retention-floor`

- **Design Principles Covered**: None — this feature is governed by the
  principles already carried by usage record ingestion and pluggable
  storage; it introduces no principle of its own.

- **Design Constraints Covered**: None — retention and backfill are bound by
  the requirements above rather than by a distinct §2.2 constraint.

- **Domain Model Entities**:
  - RecordOrigin

- **Design Components**:

  - [ ] `p1` - `cpt-cf-usage-collector-component-ingestion-gateway`

- **API**:
  - POST /usage-collector/v1/records/backfill
    (usage_collector.backfill_usage_records)
  - SDK trait: in-process backfill import
    (`cpt-cf-usage-collector-interface-sdk-client`)

- **Sequences**:

  - [ ] `p2` - `cpt-cf-usage-collector-seq-backfill-import`

- **Data**: None (see §1 Overview).

### 2.9 [Ingestion Rate Limiting & Reconciliation Metadata](feature-rate-limiting-reconciliation/) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-usage-collector-feature-rate-limiting-reconciliation`

- **Purpose**: Bounds what one calling subject can submit across every
  ingestion path with a per-replica quota, and exposes per-(tenant, GTS type)
  accepted counts, quantity summaries, and watermarks so an external
  reconciliation job can compare gear-side totals against a consumer's
  processed totals without a full raw scan.

- **Depends On**: `cpt-cf-usage-collector-feature-usage-record-ingestion`,
  `cpt-cf-usage-collector-feature-usage-query`,
  `cpt-cf-usage-collector-feature-attribution-authorization`,
  `cpt-cf-usage-collector-feature-pluggable-storage`

- **Scope**:
  - Per-subject, per-replica ingestion quota charged on submitted entry
    count, rejecting an over-quota submission whole with an actionable
    throttle error carrying retry guidance.
  - Per-(tenant, GTS type) reconciliation metadata: accepted entry counts,
    a fold-appropriate quantity summary, and the acceptance-instant and
    covered-period-end watermarks.
  - The deferred (calling gear) and (calling gear, tenant) reconciliation
    granularities, blocked on a platform identity plane that does not yet
    carry both gear name and tenant.

- **Out of scope**:
  - Stall detection and threshold evaluation, which are consumer-side
    responsibilities the gear does not perform.
  - Any per-tenant quota tier, which the security context does not carry
    attribution to support.

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-usage-collector-fr-rate-limiting`
  - [ ] `p2` - `cpt-cf-usage-collector-fr-reconciliation-metadata`
  - [ ] `p3` - `cpt-cf-usage-collector-fr-reconciliation-caller-scopes`

- **Design Principles Covered**: None — rate limiting and reconciliation are
  operator-facing capabilities layered on the ingestion path; they introduce
  no principle beyond fail-closed behavior already carried by attribution and
  authorization.

- **Design Constraints Covered**: None — none of the six §2.2 constraints
  binds rate limiting or reconciliation uniquely; both are operator-facing
  capabilities layered on top of ingestion.

- **Domain Model Entities**:
  - ReconciliationMetadata
  - ReconciliationScope

- **Design Components**:

  - [ ] `p1` - `cpt-cf-usage-collector-component-ingestion-gateway`
  - [ ] `p1` - `cpt-cf-usage-collector-component-query-gateway`

- **API**:
  - POST /usage-collector/v1/records (quota-gated)
  - POST /usage-collector/v1/records/backfill (quota-gated)
  - GET /usage-collector/v1/reconciliation
    (usage_collector.get_reconciliation_metadata)

- **Sequences**: None — the quota check and reconciliation read are steps
  embedded in the ingestion and query sequences owned by other features, not
  sequences of their own.

- **Data**: None (see §1 Overview).

### 2.10 [Data Classification & Privacy Boundary](feature-data-classification/) - LOW

- [ ] `p3` - **ID**: `cpt-cf-usage-collector-feature-data-classification`

- **Purpose**: States and enforces the three-class treatment of data the
  Usage Collector persists — opaque platform identifiers, operational
  telemetry, and caller-supplied metadata — so PII, payment, and regulated
  data obligations stay delegated to the platform identity layer rather than
  being interpreted or classified by this gear.

- **Depends On**: `cpt-cf-usage-collector-feature-attribution-authorization`

- **Scope**:
  - Treatment of tenant ID, subject ID, resource ID, and GTS type reference
    as opaque platform identifiers, never interpreted, decoded, or
    correlated to a natural person.
  - Treatment of the quantity, window bounds, acceptance instant,
    idempotency key, and correction references as non-personal operational
    telemetry.
  - Treatment of caller-supplied metadata as opaque, with the product-level
    contract that a usage source must not place PII, payment data, regulated
    health data, or credentials into it.

- **Out of scope**:
  - Metadata schema validation itself, which is a structural concern of
    usage record ingestion.
  - Any gear-local consent, DSR, or purge workflow, which the platform
    identity and governance layers own.

- **Requirements Covered**:

  - [ ] `p3` - `cpt-cf-usage-collector-fr-data-classification`

- **Design Principles Covered**: None — data classification restates a
  boundary already implied by fail-closed and PDP-centric authorization; it
  introduces no principle of its own.

- **Design Constraints Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-constraint-pii-identity-layer`

- **Domain Model Entities**:
  - RecordMetadata

- **Design Components**: None — data classification is a data-handling
  contract that spans every component rather than a component-level
  responsibility.

- **API**: None — classification is a documentation and validation contract,
  not an endpoint of its own.

- **Sequences**: None — DESIGN Section 3.6 defines six sequences (emit, invalidate, query-aggregated, query-raw, read-feed, backfill) and none is dedicated to the data-classification boundary; it is checked inside each of those.

- **Data**: None (see §1 Overview).

### 2.11 [Read-Path Consistency & Freshness Contract](feature-consistency-freshness-contract/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-feature-consistency-freshness-contract`

- **Purpose**: Publishes the plugin-agnostic floor-and-ceiling consistency
  contract between the synchronous ingestion acknowledgement and the raw,
  aggregated, and feed query surfaces, so that every consumer and every
  storage plugin codes against one documented staleness model rather than an
  implicit, backend-specific one. This is
  `cpt-cf-usage-collector-adr-consistency-contract`.

- **Depends On**: `cpt-cf-usage-collector-feature-usage-query`,
  `cpt-cf-usage-collector-feature-usage-feed`,
  `cpt-cf-usage-collector-feature-pluggable-storage`

- **Scope**:
  - The gear-level floor: ingestion acknowledgement durability, no upper
    bound on raw/aggregate/feed query visibility, no monotonic-reads
    guarantee, and dedup-identity visibility bounded by declared retention.
  - The per-plugin ceiling a deployment guide must publish across the four
    consistency dimensions (write-path finality, raw visibility, aggregate
    visibility, feed visibility).
  - The aggregate path's readiness gate: a materialized aggregate is fit to
    serve a consumer that acts on it only when its published ceiling is
    finite and, where the consumer acts on it, at or below 5 minutes p95.
  - The consumer rule that read-after-write flows must consume the
    ingestion acknowledgement rather than the query surfaces.

- **Out of scope**:
  - The numeric throughput, latency, and availability thresholds themselves
    (covered by throughput, latency, and availability SLOs).
  - The feed's own recovery-time and bulk-read-rate objective (covered by the
    usage feed feature).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-nfr-query-freshness`
  - [ ] `p2` - `cpt-cf-usage-collector-nfr-aggregate-freshness`

- **Design Principles Covered**: None — the consistency contract is stated at
  NFR level in DESIGN §3.10 rather than as a §2.1 design principle.

- **Design Constraints Covered**: None — the numeric threshold constraint is
  owned by the throughput, latency, and availability SLOs feature, per the
  Out of scope bullet above.

- **Domain Model Entities**: None — the contract governs cross-cutting
  visibility behavior rather than introducing a domain entity of its own.

- **Design Components**: None — the contract binds every component's read
  and write paths rather than one component in particular.

- **API**: None — the consistency contract is a published behavioral
  guarantee, not an endpoint.

- **Sequences**: None — none of the six DESIGN Section 3.6 sequences is dedicated to the consistency and freshness contract; the contract is a cross-cutting property checked across all six.

- **Data**: None (see §1 Overview).

### 2.12 [Throughput, Latency & Availability SLOs](feature-throughput-latency-availability/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-feature-throughput-latency-availability`

- **Purpose**: Binds the ingestion and query paths to the numeric performance
  envelope the platform's high-volume usage sources (LLM Gateway, API
  Gateway) and interactive consumers require: sustained and burst ingestion
  throughput, ingestion and aggregation latency ceilings, workload isolation
  between the two, and monthly ingestion availability, all measured against
  one shared throughput profile.

- **Depends On**: `cpt-cf-usage-collector-feature-usage-record-ingestion`,
  `cpt-cf-usage-collector-feature-usage-query`,
  `cpt-cf-usage-collector-feature-pluggable-storage`

- **Scope**:
  - The shared throughput-profile envelope: sustained rate, peak burst,
    concurrent aggregation queries, and daily transaction volume.
  - Ingestion latency (p95 ≤ 200ms) and ingestion throughput (≥ 10,000
    entries/sec) under that envelope.
  - Aggregation query latency (p95 ≤ 500ms over a 30-day single-tenant
    range) under the same envelope.
  - Workload isolation so concurrent aggregation queries do not degrade
    ingestion latency, and 99.95% monthly ingestion availability.

- **Out of scope**:
  - Staleness and freshness bounds, which are the consistency and freshness
    contract's concern rather than a throughput or latency figure.
  - The feed's own bulk-replay recovery objective (covered by the usage
    feed feature).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-nfr-query-latency`
  - [ ] `p1` - `cpt-cf-usage-collector-nfr-availability`
  - [ ] `p1` - `cpt-cf-usage-collector-nfr-throughput`
  - [ ] `p1` - `cpt-cf-usage-collector-nfr-ingestion-latency`
  - [ ] `p2` - `cpt-cf-usage-collector-nfr-workload-isolation`
  - [ ] `p1` - `cpt-cf-usage-collector-nfr-throughput-profile`

- **Design Principles Covered**: None — the throughput and latency envelope
  is an NFR allocation rather than a §2.1 design principle.

- **Design Constraints Covered**:

  - [ ] `p2` - `cpt-cf-usage-collector-constraint-nfr-thresholds`

- **Domain Model Entities**: None — this feature binds measured behavior
  rather than introducing a domain entity.

- **Design Components**: None — the SLOs bind the ingestion and query
  components' runtime behavior rather than naming one component uniquely.

- **API**: None — the SLOs bind existing ingestion and query endpoints
  rather than adding a surface of their own.

- **Sequences**: None — none of the six DESIGN Section 3.6 sequences is dedicated to the throughput, latency, or availability SLOs; they are measured against all six rather than owning one.

- **Data**: None (see §1 Overview).

### 2.13 [Public Surface Contract Stability & Versioning](feature-contract-stability/) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-usage-collector-feature-contract-stability`

- **Purpose**: Guarantees that the SDK trait, the Plugin SPI, and the REST
  API each stay stable within a major version from their 1.0 release onward,
  so plugin authors, in-process consumer gears, and remote usage sources
  migrate on schedules independent of the Usage Collector. This is
  `cpt-cf-usage-collector-adr-contract-stability`.

- **Depends On**: `cpt-cf-usage-collector-feature-usage-record-ingestion`,
  `cpt-cf-usage-collector-feature-usage-query`,
  `cpt-cf-usage-collector-feature-usage-feed`,
  `cpt-cf-usage-collector-feature-backfill-retention`,
  `cpt-cf-usage-collector-feature-pluggable-storage`

- **Scope**:
  - The major-version stability contract across all three public surfaces,
    starting at each surface's 1.0 release.
  - The definition of additive versus breaking change per surface, and the
    one-migration-window support policy for a superseded major version.
  - The structural encoding of a surface's major version (Rust trait name
    suffix; REST path version; SPI type suffix).

- **Out of scope**:
  - The functional content of any one surface, which is owned by the
    feature exposing it (ingestion, query, feed, backfill, pluggable
    storage).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-nfr-plugin-contract-stability`

- **Design Principles Covered**:

  - [ ] `p2` - `cpt-cf-usage-collector-principle-contract-stability`

- **Design Constraints Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-constraint-plugin-contract-stability`

- **Domain Model Entities**: None — versioning is a contract-evolution rule
  rather than a domain entity.

- **Design Components**: None — the stability contract spans the REST
  surface, the SDK trait, and the Plugin SPI rather than one domain
  component.

- **API**: None — the contract governs how existing endpoints and traits may
  evolve, not a new endpoint.

- **Sequences**: None — none of the six DESIGN Section 3.6 sequences is dedicated to contract stability; the stability guarantee spans the REST, SDK, and Plugin SPI surfaces exercised by all six.

- **Data**: None (see §1 Overview).

### 2.14 [Operational Visibility & Telemetry](feature-operational-visibility/) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-usage-collector-feature-operational-visibility`

- **Purpose**: Integrates ingestion latency, ingestion error rate, query
  latency, PDP error rate, storage-plugin readiness, and GTS type resolution
  failure and cache-staleness metrics into shared platform dashboards and
  alert routing, and emits a structured, correlation-tagged log entry for
  every accepted and rejected operation, pushed via OTLP from the platform's
  global meter provider.

- **Depends On**: `cpt-cf-usage-collector-feature-usage-record-ingestion`,
  `cpt-cf-usage-collector-feature-usage-query`,
  `cpt-cf-usage-collector-feature-attribution-authorization`,
  `cpt-cf-usage-collector-feature-pluggable-storage`,
  `cpt-cf-usage-collector-feature-usage-type-resolution`

- **Scope**:
  - Domain metrics for ingestion latency, ingestion error rate, query
    latency, PDP error rate, storage-plugin readiness, and GTS type
    resolution failures and declaration-cache staleness.
  - Structured logging of every accepted and rejected API operation,
    carrying the inbound correlation identifier unchanged.
  - OTLP push emission of operational telemetry via the platform's global
    `SdkMeterProvider`.

- **Out of scope**:
  - Definition of the numeric thresholds those metrics are measured against
    (covered by throughput, latency, and availability SLOs, and by the
    consistency and freshness contract).
  - Consumer-side stall detection, which reads the watermarks reconciliation
    metadata exposes but is not performed by this gear.

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-nfr-operational-visibility`

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-usage-collector-principle-otlp-push-emission`

- **Design Constraints Covered**: None — none of the six §2.2 constraints
  binds telemetry uniquely; it observes the other features' behavior rather
  than constraining the architecture itself.

- **Domain Model Entities**: None — telemetry is emitted about domain
  operations rather than modeled as a domain entity.

- **Design Components**: None — metrics and logs are emitted from every
  component rather than owned by one.

- **API**: None — telemetry is emitted alongside existing endpoints, not
  exposed as one.

- **Sequences**: None — none of the six DESIGN Section 3.6 sequences is dedicated to operational visibility; telemetry is emitted as a cross-cutting step inside each of them.

- **Data**: None (see §1 Overview).

---

## 3. Feature Dependencies

```text
cpt-cf-usage-collector-feature-attribution-authorization      (cross-cutting; direct prerequisite for 8 of the other 13 features)
cpt-cf-usage-collector-feature-usage-type-resolution           (cross-cutting; direct prerequisite for 5: ingestion, invalidation, query, backfill-retention, operational-visibility)
cpt-cf-usage-collector-feature-pluggable-storage                (cross-cutting; direct prerequisite for 10 of the other 13 -- all but its two foundation peers above and data-classification)

(the three foundation features above, jointly)
    |
    +-- cpt-cf-usage-collector-feature-usage-record-ingestion
            |
            +-- cpt-cf-usage-collector-feature-record-invalidation
            |       |
            |       +-- cpt-cf-usage-collector-feature-usage-query
            |               |
            |               +-- cpt-cf-usage-collector-feature-rate-limiting-reconciliation
            |               +-- cpt-cf-usage-collector-feature-throughput-latency-availability
            |               +-- cpt-cf-usage-collector-feature-operational-visibility
            |
            +-- cpt-cf-usage-collector-feature-backfill-retention
                    |
                    +-- cpt-cf-usage-collector-feature-usage-feed (also <- record-invalidation; numbered ahead of backfill-retention but depends forward on it -- see rationale)
                            |
                            +-- cpt-cf-usage-collector-feature-consistency-freshness-contract (also <- usage-query)
                            +-- cpt-cf-usage-collector-feature-contract-stability (also <- usage-query)

cpt-cf-usage-collector-feature-attribution-authorization (repeated from the top of this diagram, where its full fan-out of 8 is stated; only the data-classification edge is drawn here)
    |
    +-- cpt-cf-usage-collector-feature-data-classification (attribution-authorization is its only prerequisite, and it is the only non-foundation feature that does not depend on pluggable-storage)
```

**Dependency Rationale**:

- `cpt-cf-usage-collector-feature-attribution-authorization`,
  `cpt-cf-usage-collector-feature-usage-type-resolution`, and
  `cpt-cf-usage-collector-feature-pluggable-storage` depend on nothing else in
  this list. Each states a rule that other features consume rather than one
  that itself needs a prior capability. The PDP gate, the GTS declaration,
  and the plugin dispatch seam are all independent of how ingestion, query,
  or the feed later use them. Together they form the foundation tier every
  other feature is built on. The diagram does not draw a separate arrow from
  each of these three foundation features to every one of their dependents.
  Instead it states the dependent count once at the top. Concretely,
  attribution-authorization directly gates ingestion, invalidation, query,
  the feed, backfill-retention, rate-limiting-reconciliation,
  data-classification, and operational-visibility. Usage-type-resolution
  directly gates ingestion, invalidation, query, backfill-retention, and
  operational-visibility. Pluggable-storage directly gates every feature
  below it in the tree except data-classification, which is a data-handling
  contract rather than a storage-facing code path.
- `cpt-cf-usage-collector-feature-usage-record-ingestion` depends on all three
  foundation features. Every accepted **Usage Record** must first pass the
  PDP-authorized attribution check. It must also resolve its `gts_type_id` to
  a declaration before validation and identity derivation can run. Then it
  persists through the Plugin Host's dispatch. Ingestion performs none of
  this itself (DESIGN §3.4, §3.6 emit-usage sequence).
- `cpt-cf-usage-collector-feature-record-invalidation` depends on
  `usage-record-ingestion` because an invalidation entry resolves its target
  from the same dedup identity. It also reuses the same identity derivation
  and metadata validation ingestion establishes. It depends on
  attribution-authorization, usage-type-resolution, and pluggable-storage for
  the same reasons ingestion does. Every invalidation entry is itself a write
  that must clear the PDP gate, resolve the target's GTS type, and dispatch
  through the same plugin.
- `cpt-cf-usage-collector-feature-usage-query` depends on
  `usage-record-ingestion` because it reads the entries ingestion writes, and
  on `record-invalidation` because both read paths share invalidation's
  entry-type and target-resolution identity. The raw path returns a withdrawn
  record and its invalidation entry as persisted, applying no fold and
  marking nothing. Callers therefore interpret `entry_type` and `invalidates`
  themselves (DESIGN §3.2 Query Gateway). The aggregation path excludes
  withdrawn pairs, and that exclusion is pushed down to the storage plugin
  (DESIGN §3.6). It depends on attribution-authorization (PDP constraints
  applied before any user filter), usage-type-resolution (the declared fold
  aggregation serves), and pluggable-storage (the plugin executes the
  query).
- `cpt-cf-usage-collector-feature-backfill-retention` depends on
  `usage-record-ingestion` because its validation is identical to the live
  path except for the past-tolerance it replaces. It depends on
  `usage-type-resolution` because backfill validation, like the live path,
  must resolve the entry's `gts_type_id` to its declaration for unit binding
  and metadata validation. The retention floor formula is `backfill window +
  operational replay horizon`. The horizon is a fixed deployment parameter
  the gear does not read (DESIGN §3.8). The per-type value in play is the
  declared retention policy the storage plugin reads directly from
  `types-registry` (DESIGN §1.2). It depends on attribution-authorization and
  pluggable-storage for the same PDP-gating and plugin-dispatch reasons as
  ingestion. Its relationship to `record-invalidation` is an open coupling,
  not a resolved dependency. The backfill route also admits invalidation
  entries (§2.5's own API field). `record-invalidation` owns the invalidation
  rules applied on that route. `backfill-retention` owns only the route
  itself, its window bound, and its distinct permission and workload
  isolation. This decomposition does not resolve the shared path to a
  one-way dependency. The FEATURE documents for these two features must
  settle which one delivers the backfill-invalidation variant.
- `cpt-cf-usage-collector-feature-usage-feed` depends on
  `usage-record-ingestion` because it replays the same entries. It also
  depends on `record-invalidation`. A correction must appear at its own feed
  position immediately after the entry it withdraws, which requires
  invalidation's target-and-withdrawal semantics. It depends on
  attribution-authorization and pluggable-storage for the same gating and
  dispatch reasons as query. It does not depend on usage-type-resolution,
  because subscription is by caller-declared GTS type rather than by
  resolving a declaration for read shaping. Uniquely, it also depends on
  `backfill-retention`, a later-numbered, lower-priority entry. The retention
  floor is `backfill window + operational replay horizon`, and the backfill
  window is a value backfill-retention configures. The feed's conformance to
  that floor therefore depends forward on backfill-retention. The storage
  plugin still enforces the cursor refusal itself, from what it still holds
  (DESIGN §3.2 Feed Gateway). This is the one ordering irregularity in the
  set, already called out in §1 Overview and in this entry's own Depends On
  field.
- `cpt-cf-usage-collector-feature-rate-limiting-reconciliation` depends on
  `usage-record-ingestion` because its per-subject quota is charged on the
  ingestion path. It also depends on `usage-query`, because reconciliation
  metadata is served as the Query Gateway's fourth read path (DESIGN §3.5,
  External Dependencies). It depends on attribution-authorization and
  pluggable-storage as well. The reconciliation counters and watermarks the
  Plugin SPI exposes must still clear the PDP gate before a caller can read
  them.
- `cpt-cf-usage-collector-feature-data-classification` depends only on
  `attribution-authorization`. Its three-class data treatment builds directly
  on attribution's opaque-identifier boundary: tenant, subject, resource, and
  GTS type reference are never interpreted beyond that boundary. It has no
  Design Component of its own, so it does not gate or get gated by the
  ingestion, query, or feed code paths.
- `cpt-cf-usage-collector-feature-consistency-freshness-contract` depends on
  `usage-query` and `usage-feed` because it publishes the plugin-agnostic
  staleness floor and ceiling those two read surfaces must honor. It also
  depends on `pluggable-storage`, because the per-plugin ceiling it requires
  is a property the bound plugin, not the gear, must publish.
- `cpt-cf-usage-collector-feature-throughput-latency-availability` depends on
  `usage-record-ingestion` and `usage-query`. Its numeric envelope —
  ingestion latency and throughput, aggregation query latency, workload
  isolation — is measured directly against those two paths. It also depends
  on `pluggable-storage`, because the bound plugin's own performance is what
  the envelope ultimately bounds.
- `cpt-cf-usage-collector-feature-contract-stability` depends on
  `usage-record-ingestion`, `usage-query`, `usage-feed`, and
  `backfill-retention`, because the REST endpoints and SDK trait it
  stabilizes belong to those four features. It also depends on
  `pluggable-storage`, because the Plugin SPI is the third public surface the
  stability contract covers.
- `cpt-cf-usage-collector-feature-operational-visibility` depends on
  `usage-record-ingestion`, `usage-query`, `attribution-authorization`,
  `pluggable-storage`, and `usage-type-resolution`. Its metrics are named
  directly after those features' failure and latency modes. They cover
  ingestion latency and error rate, query latency, PDP error rate,
  storage-plugin readiness, and GTS resolution failure and cache staleness.
  Each metric needs its source feature defined first.

**Parallelization**:

- Tier 0 — `attribution-authorization`, `usage-type-resolution`, and
  `pluggable-storage` — can be built fully in parallel; none reads the others'
  output.
- Tier 1 — `usage-record-ingestion` and `data-classification` — can be built
  in parallel once tier 0 lands. Data-classification only needs
  attribution-authorization; ingestion needs all three tier-0 features.
- Tier 2 — `record-invalidation` and `backfill-retention` — can be built in
  parallel once ingestion lands. Each reuses ingestion's validation and
  identity logic independently of the other.
- Tier 3 — `usage-query` and `usage-feed` — can be built in parallel once
  their respective tier-2 prerequisites land. Usage-feed's build order still
  has to wait on `backfill-retention`, not on `usage-query`, so the pair
  genuinely does not block on each other.
- Tier 4 — `rate-limiting-reconciliation`, `consistency-freshness-contract`,
  `throughput-latency-availability`, `contract-stability`, and
  `operational-visibility` — are all leaf features with no dependents. That
  is 2.9 and 2.11 through 2.14 in the numbering; 2.10 is tier 1, not tier 4.
  Nothing in the document builds on top of them, so they can be delivered
  last, in any order relative to one another. They wait only on their
  respective tier-1-through-3 prerequisites.
