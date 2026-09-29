# Decomposition: TimescaleDB Usage Collector Storage Plugin

Splits the plugin's PRD and DESIGN scope into ten independently implementable
and testable features, spanning schema provisioning, ingestion, query, the
usage feed, retention, and the cross-cutting observability and error-contract
concerns that bind them.

<!-- toc -->

- [1. Overview](#1-overview)
- [2. Entries](#2-entries)
  - [2.1 Registration & Schema Provisioning - HIGH](#21-registration--schema-provisioning---high)
  - [2.2 Record Ingestion & Idempotency - HIGH](#22-record-ingestion--idempotency---high)
  - [2.3 Invalidation Persistence - HIGH](#23-invalidation-persistence---high)
  - [2.4 Aggregated Query & Rollup - HIGH](#24-aggregated-query--rollup---high)
  - [2.5 Raw Query & Converged Lookup - HIGH](#25-raw-query--converged-lookup---high)
  - [2.6 Usage Feed - HIGH](#26-usage-feed---high)
  - [2.7 Per-Type Retention - HIGH](#27-per-type-retention---high)
  - [2.8 Reconciliation Metadata - MEDIUM](#28-reconciliation-metadata---medium)
  - [2.9 Observability & Metrics - MEDIUM](#29-observability--metrics---medium)
  - [2.10 Error Classification & Transport Security - MEDIUM](#210-error-classification--transport-security---medium)
- [3. Feature Dependencies](#3-feature-dependencies)

<!-- /toc -->

**Overall implementation status:**
- [ ] `p1` - **ID**: `cpt-cf-uc-plugin-status-timescaledb-usage-collector-plugin`

## 1. Overview

This decomposition splits the TimescaleDB Usage Collector Storage Plugin into
ten features. The plugin is a storage backend for the Usage Collector gear: it
implements the gear's storage SPI (Service Provider Interface, the in-process
Rust trait `UsageCollectorPluginV1`) on PostgreSQL with the TimescaleDB
extension, and it is pure persistence and query. Every product-level rule --
authentication, authorization, attribution, type resolution, the declared
aggregation fold -- is enforced upstream by the gear core and is therefore not
decomposed here.

**The checkboxes are the status, and nothing else here is.** DESIGN section 4.5
is normative for the gear's seven-method SPI and the crate is still catching up
to it, so this document is part target and part record: an entry whose boxes are
checked names work that has landed, and an unchecked box names work that has
not. No prose summary of which is which is written here, because a summary is
what goes stale between the checkbox and the reader. What this backend does not
yet answer of the SPI contract suite is declared, check by check with the slice
that closes it, in `NOT_YET_CONFORMING` (`tests/contract_conformance_pg.rs`),
which the suite asserts is exactly the set that fails.

Three absences this document once described as present are asserted rather than
asserted-about: there is no usage-type catalog
(`schema_integration_pg::there_is_no_usage_type_catalog`, read back from a live
database), no superseded schema (the same suite reads the whole ledger back
against DESIGN section 3.7), and no deactivation method (the crate implements
the SDK's `UsageCollectorPluginV1` and nothing else is on its public surface, so
a method it does not declare is a compile error away from being noticed).

**The split axis is the SPI surface, not the component model.** The plugin's
eight DESIGN components do not partition cleanly into deliverable units: the
Record Store and the Query component together serve all seven SPI methods, and
a feature that owned both would be seven capabilities in one basket. Each entry
below instead owns one behavior a test can drive through the SPI or through a
background task, and pulls in whichever components that behavior needs. The Record Store is claimed once, by record ingestion (2.2),
because that is where its write path lives; the read paths that also execute
through it are claimed by the features that define their semantics, and name
the sequences rather than the component. A component claim marks review and
change-control ownership, not a build prerequisite: each entry adds its own
statement builders to the component it names.

Entry 2.1 is the foundation: nothing works until the schema exists and the
backend is registered. Entries 2.2 and 2.3 build the write path. Entries 2.4
through 2.8 are the read paths and the lifecycle sweep, each independently
testable once the write path can produce entries. Entries 2.9 and 2.10 are
cross-cutting: the metric inventory every other component records through, and
the error vocabulary plus transport obligations every SPI call passes through.
Each entry is a single-phase work package; sub-decomposition into milestones is
deferred to its FEATURE document.

**Why `Data: None` recurs, and why 2.1 differs.** Unlike the parent gear,
which is storage-agnostic under `cpt-cf-usage-collector-adr-pluggable-storage`
and defines no `db` or `dbtable` identifiers at all, this plugin owns a schema
and four tables (DESIGN section 3.7). All five data identifiers belong to
entry 2.1, because the schema is provisioned as one idempotent migration set
at startup before any other feature can run. Every later entry reads or writes
those same tables but provisions none of them, so each states `Data: None`
with that reason named in place.

**Open questions are recorded, not deferred silently.** PRD section 13 carries
two open questions for the gateway and one known shortfall. The raw-list SPI
return type is surfaced in entry 2.5, and the replay-horizon delivery question
plus the long-transaction cursor shortfall are surfaced in entry 2.6. Each is
an explicit scope caveat with a pointer back to PRD section 13, not a
placeholder.

**Inherited NFR exclusions.** PRD section 6.2's NFR exclusions are inherited,
not decomposed. One of them is a published shortfall rather than an omission:
the single connection pool entry 2.1 creates serves every path, so workload
isolation is not realised and DESIGN section 4.1 item 1 publishes that.

**Pre-release.** The Usage Collector subsystem and this plugin have no
installations and no production data. No feature below carries a migration,
backward-compatibility, or upgrade-path obligation; schema work is forward
setup only.

## 2. Entries

### 2.1 [Registration & Schema Provisioning](feature-registration-schema-provisioning/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-uc-plugin-feature-registration-schema-provisioning`

  Open on the two definitions of done its feature document leaves unchecked,
  both slice 3's: the background loop's feed settled-horizon sampling, and the
  contract suite as a release gate with nothing declared non-conforming.

- **Purpose**: Brings the backend into existence and makes it discoverable.
  At startup the plugin loads and validates its configuration, creates the
  connection pool, provisions the whole database schema idempotently, applies
  the configuration-driven partitioning setup, and then registers itself as a
  scoped SPI client under a GTS (Global Type System) instance identifier
  carrying its configured vendor and priority. This is the discovery half of
  `cpt-cf-usage-collector-adr-pluggable-storage`, which puts every persistence
  and query call behind one SPI seam and lets operator configuration bind the
  active backend. The schema deliberately holds no type catalog and no foreign
  key to one, per `cpt-cf-usage-collector-adr-registry-owned-typing`:
  declarations live in `types-registry` and never reach this plugin. The plugin
  realizes the storage half of `cpt-cf-usage-collector-fr-pluggable-storage`
  and provides the gear's registry contract
  `cpt-cf-usage-collector-contract-gts-registry` through its own registration
  contract `cpt-cf-uc-plugin-contract-gts-registration`.

- **Depends On**: None (this is the foundation entry)

- **Scope**:
  - Typed configuration load and validation, including the required
    `database_url` and `feed_replay_horizon_secs`, and the rule that
    `transaction_timeout_secs` exceeds `statement_timeout_secs`.
  - Connection-pool creation over the operator-provisioned database that the
    external contract `cpt-cf-uc-plugin-contract-timescaledb` describes, and
    the startup durability checks that refuse to start when the server has
    `fsync` or `full_page_writes` off.
  - Idempotent schema migrations that build the ledger hypertable, the type-key
    table, the feed retention marks table, and the hourly continuous aggregate,
    so a restart re-runs provisioning as a no-op.
  - Post-migration setup driven by configuration: the hypertable's chunk time
    interval and type-key slice width, plus removal of any table-wide
    declarative retention policy an earlier build may have registered.
  - The GTS handshake: building the plugin registration, publishing it to
    `types-registry`, and registering the SPI client in `ClientHub` under the
    GTS instance scope with the configured vendor and priority.
  - Keeping every backend-specific dependency, SQL statement and schema object
    inside this crate, with no compile-time dependency on the host gear.
  - Compile-time conformance to the seven-method SPI trait -- the plugin's sole
    public surface `cpt-cf-uc-plugin-interface-storage-spi`, realized as
    `cpt-cf-uc-plugin-interface-spi` -- and a green run of the gear's full SPI
    contract suite as the release gate.

- **Out of scope**:
  - Deciding which plugin the host binds; vendor and priority selection is
    host-side.
  - The rollup's refresh-policy application, which entry 2.4 owns even though
    the Gear component invokes it during `init`.
  - The content of any runtime query or retention decision, which the features
    below own.
  - Data migration from a prior release: the subsystem is pre-release, with no
    installations and nothing to migrate.

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-uc-plugin-fr-registration`
  - [x] `p1` - `cpt-cf-uc-plugin-fr-schema-provisioning`
  - [ ] `p1` - `cpt-cf-uc-plugin-fr-durable-ack`
  - [ ] `p1` - `cpt-cf-uc-plugin-nfr-spi-stability`

  The durable-acknowledgement requirement is covered twice on purpose: this
  entry owns the startup check that refuses an unsafe server setting, and 2.2
  owns the per-transaction commit guarantee.

  Three of the four stay open, each on something outside this entry's reach
  today. `cpt-cf-uc-plugin-fr-registration` waits on the background loop's feed
  settled-horizon sampling, which is slice 3's. `cpt-cf-uc-plugin-fr-durable-ack`
  waits on 2.2's half, below. `cpt-cf-uc-plugin-nfr-spi-stability` waits on the
  contract suite running with nothing declared non-conforming, which is slice
  3's six feed rows in `NOT_YET_CONFORMING`.

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-uc-plugin-principle-spi-conformance`

  Open on the same six rows as `cpt-cf-uc-plugin-nfr-spi-stability`: slice 3.

- **Design Constraints Covered**:

  - [x] `p1` - `cpt-cf-uc-plugin-constraint-vendor-isolation`

- **Domain Model Entities**:
  - TimescaleDbPluginConfig
  - TypeKeyCache
  - Connection pool handle

- **Design Components**:

  - [ ] `p2` - `cpt-cf-uc-plugin-component-gear`
  - [x] `p1` - `cpt-cf-uc-plugin-component-migrations`

  The Gear component stays open for the reason `cpt-cf-uc-plugin-fr-registration`
  does: its background loop does not yet sample the feed's settled-horizon lag,
  which is slice 3's.

- **API**:
  - Gear `init` lifecycle hook (config load, pool, migrations, registration)
  - Gear `start` / `stop` lifecycle hooks for the background task
  - `UsageCollectorPluginV1` trait registration under the GTS instance scope
  - No REST or network-exposed surface; the SPI is in-process only

- **Sequences**: None -- startup is the Gear component's `init` and is
  described as a component responsibility in DESIGN section 3.2 rather than as
  a numbered sequence.

- **Data**:

  - [x] `p1` - `cpt-cf-uc-plugin-db-schema`
  - [x] `p1` - `cpt-cf-uc-plugin-dbtable-usage-records`
  - [x] `p1` - `cpt-cf-uc-plugin-dbtable-usage-type-key`
  - [x] `p1` - `cpt-cf-uc-plugin-dbtable-usage-rollup-1h`
  - [x] `p1` - `cpt-cf-uc-plugin-dbtable-usage-feed-retention-marks`

### 2.2 [Record Ingestion & Idempotency](feature-record-ingestion-idempotency/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-uc-plugin-feature-record-ingestion-idempotency`

  Open on the one definition of done its feature document leaves unchecked,
  `cpt-cf-uc-plugin-dod-durable-acknowledgement`, and on the throughput rate no
  suite here measures. Both are named under Requirements Covered below.

- **Purpose**: Delivers the plugin's write path: single and batch persistence
  of usage entries, deduplicated in the backend on the gear's six-part
  identity, acknowledged only once durable, with every caller-supplied value
  stored verbatim. Deduplication at the storage boundary is what
  `cpt-cf-usage-collector-adr-mandatory-idempotency` assigns to the plugin, so
  that at-least-once emission from callers is safe for every declared fold.
  The entry identifier the plugin stores is derived by the gateway as a UUIDv5
  over that same identity, per
  `cpt-cf-usage-collector-adr-record-identity-derivation`; the plugin mints no
  identity of its own. This feature realizes the storage side of
  `cpt-cf-usage-collector-fr-ingestion`,
  `cpt-cf-usage-collector-fr-record-metadata`,
  `cpt-cf-usage-collector-fr-idempotency` and
  `cpt-cf-usage-collector-fr-record-quantity`.

- **Depends On**: `cpt-cf-uc-plugin-feature-registration-schema-provisioning`

- **Scope**:
  - Single-entry insert as one guarded statement that computes its own
    admission verdict from the statement's timestamp, inserts only an admitted
    row, and reports whether that row won its identity.
  - Batch insert over the same guarded statement shape, with one result per
    input entry positionally aligned to input order, so a conflict or rejection
    on one entry never fails the others.
  - Deduplication on the tenant, GTS type, idempotency key, covered period and
    entry type, enforced by the ledger's unique constraint and its conflict
    target, with the read-back keyed on the entry identifier.
  - Resolution of a duplicate identity: identical caller-supplied fields are a
    silent absorb returning the stored entry, divergent fields an idempotency
    conflict carrying the stored entry.
  - In-batch resolution of two same-identity entries, the later against the
    earlier.
  - Refusal, as a retryable error, of an entry whose acceptance instant differs
    from the store's clock by more than the configured acceptance slack.
  - The declared dedup level `linearizable` with a convergence bound of zero,
    established from the store's commit order rather than from elapsed time,
    including discarding a write whose caller was already answered.
  - Durable acknowledgement: synchronous commit forced on every write
    transaction, no in-memory buffering of acknowledged entries.
  - Digit-for-digit quantity round-trip on every read path that returns an
    entry, negative half included, with no conversion, scaling, rounding or
    truncation.
  - Per-type key assignment and caching on an entry's first write, resolved
    before the write transaction opens (DESIGN section 3.2 lists the assignment
    under the Retention component and the resolution under the Record Store; it
    is claimed here because it runs on the write path).

- **Out of scope**:
  - Idempotency-key presence, attribution and metadata shape validation, all
    enforced by the gear core before the call reaches the SPI.
  - Persistence of a withdrawal entry and its at-most-one rule, which entry 2.3
    owns even though it reuses this write path.
  - Preservation of a dedup identity beyond the referenced type's declared
    retention; that bound is entry 2.7's rule and the gear's adopted floor.
  - The startup durability checks that refuse an unsafe server setting, which
    entry 2.1 owns; `cpt-cf-uc-plugin-fr-durable-ack` is split between the two,
    and this feature owns its per-transaction commit guarantee.

- **Requirements Covered**:

  - [x] `p1` - `cpt-cf-uc-plugin-fr-record-persistence`
  - [x] `p1` - `cpt-cf-uc-plugin-fr-idempotent-dedup`
  - [ ] `p1` - `cpt-cf-uc-plugin-fr-durable-ack`
  - [x] `p1` - `cpt-cf-uc-plugin-fr-quantity-fidelity`
  - [x] `p1` - `cpt-cf-uc-plugin-fr-dedup-level`
  - [ ] `p1` - `cpt-cf-uc-plugin-nfr-ingestion-throughput`

  Two stay open. `cpt-cf-uc-plugin-fr-durable-ack` waits on this entry's half
  of it: the write transaction does not force `synchronous_commit`, so an
  operator setting can still weaken an acknowledgement, and DESIGN section 3.5
  requires it. `cpt-cf-uc-plugin-nfr-ingestion-throughput` is a rate the
  repository runs no load test for, so no suite here can close it; the
  batch-shape half of it is checked through
  `cpt-cf-uc-plugin-dod-batch-positional-results`.

- **Design Principles Covered**:

  - [x] `p1` - `cpt-cf-uc-plugin-principle-pure-persistence`

- **Design Constraints Covered**:

  - [x] `p1` - `cpt-cf-uc-plugin-constraint-dedup-key-preservation`

- **Domain Model Entities**:
  - UsageRecord
  - UsageRecordRow
  - Transaction id (`xact_id`)
  - Type key

- **Design Components**:

  - [x] `p1` - `cpt-cf-uc-plugin-component-record-store`

- **API**:
  - `create_usage_record` (SPI)
  - `create_usage_records` (SPI)

- **Sequences**:

  - [x] `p1` - `cpt-cf-uc-plugin-seq-ingest-dedup`
  - [x] `p1` - `cpt-cf-uc-plugin-seq-ingest-batch`

- **Data**: None -- the ledger and type-key tables this feature writes are
  provisioned by 2.1; this feature adds no schema object of its own.

### 2.3 [Invalidation Persistence](feature-invalidation-persistence/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-uc-plugin-feature-invalidation-persistence`

  Open on the withdrawal-reference lookup, named under Requirements Covered
  below.

- **Purpose**: Persists a withdrawal as an ordinary appended entry that names
  the entry it withdraws and carries a reason code, never rewriting the
  withdrawn entry, and admits at most one withdrawal per target. This is a
  narrow feature on purpose. It reuses 2.2's write path, but it is its own
  behavior with its own correctness rule: every withdrawal of one target shares
  a single dedup identity -- the target's tenant, GTS type, idempotency key and
  covered period with entry type `invalidation` -- so a second withdrawal
  collides with the first, and the exactness of the fold's netting rests on
  that bound. It mirrors the parent gear's record-invalidation feature at the
  storage tier, and it is the storage realization of
  `cpt-cf-usage-collector-adr-append-only-invalidation`, which makes a
  correction a new fact rather than a mutation of history. It realizes
  `cpt-cf-usage-collector-fr-record-invalidation` on the write side. Its
  assertions land in the same target Record ingest suite as 2.2 (DESIGN section
  4.6); the split is by correctness rule and review ownership, not by test
  binary.

- **Depends On**: `cpt-cf-uc-plugin-feature-record-ingestion-idempotency`

- **Scope**:
  - Persisting a withdrawal through the ordinary insert path, with its
    withdrawal reference and reason code stored as the gateway supplied them.
  - The storage-enforced pairing rule 2.1's schema carries: the withdrawal
    reference and the reason code are set exactly when the entry declares itself
    an invalidation.
  - At most one withdrawal per target, established by the shared dedup identity
    rather than by a separate store-side rule.
  - A record and its withdrawal persisting as two distinct entries under the
    same idempotency key and covered period, each retry absorbed against its
    own stored row.
  - Withdrawal-reference lookups served by the ledger's withdrawal-reference
    index that 2.1 provisions, which the fold's exclusion rule reads.
  - Never mutating or deleting the withdrawn entry.

- **Out of scope**:
  - Faithful-copy validation of a withdrawal against its target, which the gear
    core performs before dispatch.
  - Excluding a withdrawn pair from a fold, which entry 2.4 owns, and ordering
    a withdrawal after its target in the feed, which entry 2.6 owns.
  - Resolving the target by lookup, which entry 2.5 owns.

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-uc-plugin-fr-invalidation-persistence`

  Open on the withdrawal-reference lookup alone. The index is provisioned by
  2.1 and the fold's exclusion predicate reads the reference, but nothing
  matches on the covered-period end beside it and no test reads a plan back, so
  `cpt-cf-uc-plugin-algo-withdrawal-reference-lookup` and
  `cpt-cf-uc-plugin-dod-withdrawal-reference-index` stay unchecked. They close
  with the query rules of slice 7, which owns the read paths that issue the
  lookup. Every other part of this entry is checked.

- **Design Principles Covered**: None -- the write path's pure-persistence
  principle is carried by 2.2, whose insert path this feature reuses unchanged.

- **Design Constraints Covered**: None -- the dedup-key constraint that
  produces the at-most-one bound is carried by 2.2; this feature applies it to
  the withdrawal identity rather than restating it.

- **Domain Model Entities**:
  - Invalidation
  - UsageRecord (withdrawal entry)
  - ReasonCode

- **Design Components**: None -- the withdrawal rides the Record Store insert
  path claimed by 2.2; this feature adds behavior to that path rather than a
  component of its own.

- **API**:
  - `create_usage_record` (SPI, entry type `invalidation`)
  - `create_usage_records` (SPI, entry type `invalidation`)

- **Sequences**: None -- a withdrawal follows the ingest sequences 2.2 owns;
  its distinguishing rule is the shared dedup identity described in DESIGN
  section 3.1, not a separate flow.

- **Data**: None -- the ledger table and its withdrawal-reference index are
  provisioned by 2.1.

### 2.4 [Aggregated Query & Rollup](feature-aggregated-query-rollup/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-uc-plugin-feature-aggregated-query-rollup`

- **Purpose**: Executes the host-supplied aggregation fold inside the backend,
  with grouping, filtering and scope pushed down, and serves eligible queries
  from an hourly materialised aggregate instead of scanning the ledger. The
  plugin never chooses a fold: the fold is declared on the type and arrives as
  a typed parameter, per `cpt-cf-usage-collector-adr-declared-fold`. Every
  range predicate selects on the covered-period end alone, per
  `cpt-cf-usage-collector-adr-window-end-selection`. The materialised aggregate
  exists because `cpt-cf-usage-collector-adr-feed-aggregate-split` makes the
  aggregate a derived view a plugin may materialise, while a charging consumer
  reads the entry feed instead. This feature is the plugin's allocation target
  for `cpt-cf-usage-collector-nfr-query-latency` and
  `cpt-cf-usage-collector-nfr-aggregate-freshness`, and realizes
  `cpt-cf-usage-collector-fr-query-aggregation`.

- **Depends On**: `cpt-cf-uc-plugin-feature-record-ingestion-idempotency`,
  `cpt-cf-uc-plugin-feature-invalidation-persistence`

- **Scope**:
  - Pushed-down `SUM`, `COUNT`, `MIN`, `MAX` and `LATEST` with grouping over
    the requested dimensions, applying the host-supplied filter, scope and
    metadata filter, and returning no raw rows for client-side aggregation.
  - Exclusion of a withdrawn entry and the withdrawal that removed it from
    every fold.
  - The `LATEST` total order: greatest covered-period end, then latest
    acceptance instant, then greatest entry identifier in byte order.
  - Metadata filtering that ORs the values of one key and ANDs distinct keys.
  - Bucket-key rendering: a tenant dimension as the lowercase hyphenated UUID,
    every other dimension verbatim; grouping on subject identifier or subject
    type excludes entries without a subject.
  - Empty-selection values: zero under `SUM` and `COUNT`, null under `MAX`,
    `MIN` and `LATEST`, with a group nothing survives in yielding no bucket.
  - An unimplemented fold answered as an internal error, never by substituting
    another fold.
  - The five rollup-eligibility conditions, the split between whole-hour
    rollup reads and partial-hour ledger edges, and reporting which path served
    a query and why a fallback occurred.
  - Idempotent application of the live and history continuous-aggregate refresh
    policies at startup, each committing batch by batch, plus sampling of each
    policy's last-run status and age since success.
  - Publication of the acceptance-to-aggregate and withdrawal-propagation
    bounds the aggregate path carries.
  - Publication of the `LATEST` fold's memory bound: peak memory grows with the
    row count of the largest group, and the caller's covered period is the only
    thing that bounds it (DESIGN section 4.2).

- **Out of scope**:
  - Resolving which fold a type declares, owned by the gear core.
  - Raw row retrieval and point lookup, covered by entry 2.5.
  - Dropping rollup rows during a retention sweep, covered by entry 2.7, which
    performs the drop using the materialisation-table name this feature
    resolves.

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-uc-plugin-fr-aggregated-query`
  - [ ] `p2` - `cpt-cf-uc-plugin-fr-rollup-aggregation`
  - [ ] `p2` - `cpt-cf-uc-plugin-nfr-aggregate-freshness`
  - [ ] `p1` - `cpt-cf-uc-plugin-nfr-query-latency`

- **Design Principles Covered**: None -- the aggregate path adds no principle
  beyond the pure-persistence rule 2.2 carries, which is why the fold arrives
  as a parameter rather than being resolved here.

- **Design Constraints Covered**:

  - [ ] `p2` - `cpt-cf-uc-plugin-constraint-rollup-ledger-coupling`

- **Domain Model Entities**:
  - AggregationSpec
  - AggregationResult
  - AggregationFold

- **Design Components**:

  - [ ] `p1` - `cpt-cf-uc-plugin-component-query`
  - [ ] `p1` - `cpt-cf-uc-plugin-component-rollup`

- **API**:
  - `query_aggregated_usage_records` (SPI)

- **Sequences**:

  - [ ] `p1` - `cpt-cf-uc-plugin-seq-query-aggregated`
  - [ ] `p1` - `cpt-cf-uc-plugin-seq-rollup-refresh`

- **Data**: None -- the ledger table and the hourly continuous aggregate this
  feature reads and refreshes are provisioned by 2.1.

### 2.5 [Raw Query & Converged Lookup](feature-raw-query-converged-lookup/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-uc-plugin-feature-raw-query-converged-lookup`

- **Purpose**: Serves the two exact read paths over the ledger: keyset-
  paginated raw pages over the host-supplied order, and a scoped point lookup
  by entry identifier that the gateway uses to resolve a withdrawal's target
  before it accepts the correction. Both paths return what is stored, with no
  derived view in between. The lookup's guarantees follow
  `cpt-cf-usage-collector-adr-consistency-contract`, which sets a floor and
  obliges every plugin to publish its own ceiling, its dedup level and its
  convergence bound; on this plugin's single transactional primary both the
  convergence bound and the query-path lag bound are zero, so a converged-only
  read answers immediately and the not-converged variant never occurs. These
  paths realize `cpt-cf-usage-collector-fr-query-raw` and the lookup half of
  `cpt-cf-usage-collector-fr-record-invalidation` and
  `cpt-cf-usage-collector-fr-record-identity`.

- **Depends On**: `cpt-cf-uc-plugin-feature-record-ingestion-idempotency`

- **Scope**:
  - Keyset seek pagination from the structured keyset the gateway decoded, over
    the effective order the host supplies, returning the page's rows together
    with the keyset of its last row.
  - Fetching one row beyond the page size to detect whether a next page exists.
  - Never widening the host-supplied filter and never using an offset scan.
  - An order key on a field that may be absent answered as an internal error,
    because the seek predicate is a row-value comparison sound only over
    non-null columns.
  - Returning entries as persisted, so a withdrawn record and the withdrawal
    that removed it both appear and are not guaranteed to land on one page.
  - Point lookup by entry identifier with the host-supplied scope applied in
    the same predicate, so an out-of-scope entry answers exactly as an absent
    one.
  - Converged-only semantics: a surviving entry returned once its identity has
    converged, an acknowledged and retained entry never reported absent, and a
    definite answer within the convergence bound plus the published query-path
    lag bound.
  - Publication of the full nine-item consistency profile, including the dedup
    level, the convergence bound and the query-path lag bound, and the
    obligation on a deployment outside the single-primary posture to republish
    the affected items before serving traffic.

- **Out of scope**:
  - Encoding, decoding, signing or validating a wire cursor, which the gateway
    owns and entry 2.6 states as a constraint.
  - Aggregation over raw rows, covered by entry 2.4.
  - Deriving the per-path bounds the profile carries: the dedup level and
    convergence bound are 2.2's, the aggregate bounds 2.4's, the feed's
    settled-horizon bound 2.6's. This feature assembles the published nine-item
    profile from them.
  - **The raw-list SPI return type is an open question for the gateway**: the
    gear's trait returns a page envelope while this plugin's raw-query sequence
    returns a keyset. This feature follows the sequence, and the reconciliation
    of the two shapes is recorded in PRD section 13 rather than settled here.

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-uc-plugin-fr-raw-query`
  - [ ] `p1` - `cpt-cf-uc-plugin-fr-converged-lookup`
  - [ ] `p1` - `cpt-cf-uc-plugin-nfr-consistency-profile`

- **Design Principles Covered**: None -- both read paths run under the
  pure-persistence principle 2.2 carries; neither adds a principle of its own.

- **Design Constraints Covered**: None -- the gateway-owned-cursor constraint
  that governs the keyset handoff is carried by 2.6, which owns the feed's
  position issuance as well; this feature consumes the rule rather than stating
  it twice.

- **Domain Model Entities**:
  - ODataQuery
  - Page
  - UsageRecord

- **Design Components**: None -- the seek and point-read statements are built
  by the Query component that 2.4 claims and executed by the Record Store that
  2.2 claims; this feature defines their semantics.

- **API**:
  - `list_usage_records` (SPI)
  - `get_usage_record` (SPI)

- **Sequences**:

  - [ ] `p1` - `cpt-cf-uc-plugin-seq-list-keyset`
  - [ ] `p1` - `cpt-cf-uc-plugin-seq-converged-lookup`

- **Data**: None -- the ledger table and the indexes both paths read are
  provisioned by 2.1.

### 2.6 [Usage Feed](feature-usage-feed/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-uc-plugin-feature-usage-feed`

- **Purpose**: Serves replay-safe feed pages over a subscription of GTS types
  under the host-supplied compiled scope, in the plugin's own deterministic
  order below the instance-wide settled horizon. A charging consumer reads this
  feed and derives its charges from entries, which is the split
  `cpt-cf-usage-collector-adr-feed-aggregate-split` decides; that decision also
  fixes that the gateway owns the wire cursor, that pages come in a
  deterministic order the plugin chooses, and that a withdrawal follows the
  entry it withdraws. Completeness and snapshot consistency are gear-level feed
  guarantees (`cpt-cf-usage-collector-adr-consistency-contract` states the
  snapshot guarantee as one the append-only ledger purchases); what this plugin
  publishes above the eventual floor is its own freshness and lag bounds. The
  feature realizes `cpt-cf-usage-collector-fr-billing-usage-feed`,
  `cpt-cf-usage-collector-fr-billing-fields-on-read` and the refusal half of
  `cpt-cf-usage-collector-fr-billing-retention-floor`, and is the allocation
  target for `cpt-cf-usage-collector-nfr-billing-feed-freshness` and
  `cpt-cf-usage-collector-nfr-replay-throughput`.

- **Depends On**: `cpt-cf-uc-plugin-feature-record-ingestion-idempotency`,
  `cpt-cf-uc-plugin-feature-invalidation-persistence`

- **Scope**:
  - Feed order by inserting-transaction identifier and then entry identifier,
    served from the dedicated feed index, with the compiled scope applied as a
    bound predicate so an out-of-scope entry is absent.
  - The page protocol that fixes a snapshot before planning and re-checks the
    retention marks authoritatively after commit (steps in DESIGN section 3.6
    `cpt-cf-uc-plugin-seq-feed-page`).
  - Completeness: no entry becoming visible at or before a returned position,
    whatever the concurrency, commit order or number of gateway replicas, for
    an unchanged compiled scope.
  - Snapshot consistency across a paginated scan, and a bounded replay that
    returns the same entries in the same order and no next position once its
    bound is reached.
  - A live head: a page that reaches the settled head returns a position at the
    head even when it carries no entries.
  - A named start: the oldest-entry start begins at the oldest entry the
    subscription retains rather than at the head, and an unknown start mode a
    later gear version adds fails loudly as an internal error.
  - Retention refusal: a position after which retention has removed an entry of
    a subscribed type is refused with the cursor-beyond-retention error, read
    from the per-type retention marks rather than from the position's age, so a
    position whose continuation is intact is served whatever its age.
  - The acceptance-order slack derivation that the refusal argument rests on.
  - Sustained bulk read rate sufficient for a consumer a day behind to reach
    the head within the recovery window, via the index-ordered merge across
    chunks with the scope applied as a filter.
  - Bounding acceptance-to-feed-visibility, which the oldest running write
    transaction in the instance determines, and the horizon-lag gauge that
    surfaces a long transaction.

- **Out of scope**:
  - Deleting entries and raising the retention marks the refusal reads, which
    entry 2.7 owns.
  - Minting, encoding or interpreting a wire cursor, which the gateway owns;
    the plugin issues only its own opaque position.
  - **Two gateway-side items are recorded rather than resolved here**, both in
    PRD section 13. First, the deployment supplies the replay horizon as plugin
    configuration because the SPI does not carry it; whether it should reach
    the plugin through the SPI instead is an open question. Second, a known
    shortfall stands against the gateway's cursor zones: the mark check reads
    settled entries only, so a transaction holding an identifier open for at
    least the replay horizon can leave a position polled at the head refusable.
    Nothing is silently truncated in either case.

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-uc-plugin-fr-usage-feed`
  - [ ] `p1` - `cpt-cf-uc-plugin-nfr-feed-freshness`
  - [ ] `p2` - `cpt-cf-uc-plugin-nfr-replay-throughput`

- **Design Principles Covered**: None -- the feed reads the ledger under the
  pure-persistence principle 2.2 carries and adds no principle of its own.

- **Design Constraints Covered**:

  - [ ] `p1` - `cpt-cf-uc-plugin-constraint-gateway-owned-cursors`

- **Domain Model Entities**:
  - FeedPosition
  - FeedPage
  - FeedStart
  - Settled horizon

- **Design Components**: None -- feed pages are read through the Record Store
  that 2.2 claims and their statements built by the Query component that 2.4
  claims; this feature defines the page protocol and its guarantees.

- **API**:
  - `read_feed_page` (SPI)

- **Sequences**:

  - [ ] `p1` - `cpt-cf-uc-plugin-seq-feed-page`

- **Data**: None -- the ledger table, its feed index and the retention-marks
  table are provisioned by 2.1.

### 2.7 [Per-Type Retention](feature-per-type-retention/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-uc-plugin-feature-per-type-retention`

- **Purpose**: Enforces retention per GTS type from each type's current
  declared retention policy, measured from the end of the covered period, by a
  background sweep that drops whole storage chunks and raises the feed's
  retention marks in the same transaction. No declarative database policy can
  express this, because the retention trait lives in `types-registry` rather
  than in the database. Retention is measured on the covered-period end, the
  column every read-path range predicate already selects on
  (`cpt-cf-usage-collector-adr-window-end-selection`). The feature realizes
  `cpt-cf-usage-collector-fr-billing-retention-floor` on the deletion side, and
  the mark it raises is what lets entry 2.6 refuse a position rather than serve
  a silently truncated range.

- **Depends On**: `cpt-cf-uc-plugin-feature-usage-feed` (the retention mark is
  what lets the feed refuse a position)

- **Scope**:
  - The periodic sweep, admitted one replica at a time by an advisory lock held
    on a detached connection so it releases on close whatever the outcome.
  - Listing every ledger chunk with its covered-period and type-key ranges, and
    resolving the current declared retention of every type in a chunk's key
    range from the registry on every sweep, never cached, because retention is
    mutable and `cpt-cf-usage-collector-adr-registry-owned-typing` keeps it off
    the cacheable part of a declaration for that reason.
  - The pure drop decision: a chunk drops only once every type sharing it has
    passed its declared retention, and a type whose retention cannot be
    resolved keeps the chunk and is counted.
  - Two permitted over-retention effects -- whole-chunk granularity, and a
    shared chunk held to the longest retention among the types in it -- with
    under-retention never permitted.
  - The single bounded transaction in which the mark raise, the chunk drop and
    the rollup-row deletion commit together (DESIGN section 3.6
    `cpt-cf-uc-plugin-seq-retention-sweep`).
  - Dropping nothing in a cycle when the rollup's materialisation table cannot
    be found, rather than cutting a chunk whose rollup rows have no table to
    cut.
  - Keeping the chunk and counting a drop failure when a lock wait times out or
    the transaction is aborted, so the next sweep retries.

- **Out of scope**:
  - Per-entry purge or erasure; disposal is the chunk drop alone, and a
    data-subject erasure is an operator database action outside the SPI.
  - Refusing a feed position, which entry 2.6 owns and which reads the mark
    this feature raises.
  - Assigning and caching a type's partitioning key, which entry 2.2 owns
    because it runs on the write path, even though DESIGN section 3.2 lists the
    assignment under the Retention component this feature claims.
  - Checking that a deployment declared enough retention for replay and dedup
    preservation; that is a deployer obligation the plugin does not verify.

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-uc-plugin-fr-per-type-retention`

- **Design Principles Covered**: None -- the sweep is a lifecycle process
  rather than a request path, and no design principle in DESIGN section 2.1
  binds it beyond what 2.2 already carries.

- **Design Constraints Covered**:

  - [ ] `p1` - `cpt-cf-uc-plugin-constraint-retention`

- **Domain Model Entities**:
  - Retention policy
  - Chunk drop decision
  - Feed retention mark

- **Design Components**:

  - [ ] `p1` - `cpt-cf-uc-plugin-component-retention`

- **API**:
  - Background retention sweep, started and stopped by the gear lifecycle
  - No SPI method; the sweep is not caller-invoked

- **Sequences**:

  - [ ] `p1` - `cpt-cf-uc-plugin-seq-retention-sweep`

- **Data**: None -- the ledger chunks, the rollup's materialisation table and
  the retention-marks table the sweep operates on are provisioned by 2.1.

### 2.8 [Reconciliation Metadata](feature-reconciliation-metadata/) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-uc-plugin-feature-reconciliation-metadata`

- **Purpose**: Reports, for one tenant and GTS type per call, the count of
  accepted entries in the requested range, a fold-appropriate quantity summary
  over the same selection, and two watermarks that the range does not bound.
  Revenue assurance compares emitter, gear and consumer totals with these
  figures and spots a stalled emitter. The counters live in the plugin because
  the gear is stateless. The summary follows whichever fold the host passes,
  never one the plugin picks, per
  `cpt-cf-usage-collector-adr-declared-fold`, and the range selects on the
  covered-period end per
  `cpt-cf-usage-collector-adr-window-end-selection`. It realizes
  `cpt-cf-usage-collector-fr-reconciliation-metadata`.

- **Depends On**: `cpt-cf-uc-plugin-feature-record-ingestion-idempotency`,
  `cpt-cf-uc-plugin-feature-invalidation-persistence`

- **Scope**:
  - The accepted count over entries whose covered-period end falls in the
    requested range, counting every accepted entry, withdrawals included,
    because it reports ingestion activity.
  - The fold-appropriate summary: an accrued sum for a summing type, otherwise
    an observation count and the latest observation by the same total order the
    aggregate path uses, with withdrawn pairs excluded.
  - The two watermarks, the latest acceptance instant and the latest
    covered-period end, read through their dedicated indexes and unbounded by
    the range.
  - One entry per dedup identity in every figure.
  - The host-supplied scope applied first, so a tenant outside it answers
    exactly as one holding no entries: a zero count, an empty-selection
    summary, and both watermarks absent.
  - Defined empty-selection values: an accrual over an empty set is zero, an
    observation over one is absent.

- **Out of scope**:
  - Any claim about feed completeness; these figures prove nothing about it.
  - Paging, which the endpoint does not need, since the gear serves one scope
    per call.
  - Serving the summary from the materialised aggregate; the summary always
    takes the exact scan.

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-uc-plugin-fr-reconciliation-metadata`

- **Design Principles Covered**: None -- the reconciliation read runs under
  the pure-persistence principle 2.2 carries and adds none of its own.

- **Design Constraints Covered**: None -- no constraint in DESIGN section 2.2
  binds this read beyond the injection-safe translation entry 2.10 states for
  every host-supplied filter.

- **Domain Model Entities**:
  - ReconciliationMetadata
  - AggregationFold

- **Design Components**: None -- the reconciliation statements are built by
  the Query component that 2.4 claims and executed by the Record Store that 2.2
  claims.

- **API**:
  - `get_reconciliation_metadata` (SPI)

- **Sequences**:

  - [ ] `p2` - `cpt-cf-uc-plugin-seq-reconciliation`

- **Data**: None -- the ledger table and the watermark indexes this read uses
  are provisioned by 2.1.

### 2.9 [Observability & Metrics](feature-observability-metrics/) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-uc-plugin-feature-observability-metrics`

- **Purpose**: Provides the one OpenTelemetry instrument inventory every other
  component records through, under the plugin's own metric sub-namespace,
  distinct from the gear's request-path signals. The plugin owns the
  backend-internal series the gear cannot see: how long an insert took, how a
  deduplication resolved, which aggregate path served a query, how saturated
  the pool is, how far the feed's settled horizon lags, and whether the backend
  is ready. Because
  `cpt-cf-usage-collector-adr-pluggable-storage` puts the backend entirely
  behind the SPI seam, these signals exist nowhere else. It is the allocation
  target for `cpt-cf-usage-collector-nfr-operational-visibility`.

- **Depends On**: `cpt-cf-uc-plugin-feature-registration-schema-provisioning`

- **Scope**:
  - Declaration of every push-based counter, gauge and histogram the plugin
    emits, in one module, so each instrument has exactly one name.
  - At minimum: ingestion latency, deduplication outcomes, query latency,
    connection-pool saturation, backend error rate by classification, and
    backend readiness.
  - The backend-specific series the other features feed: stale-acceptance
    rejections, aggregate path and fallback reason, rollup refresh status and
    age since success, retention drops, drop failures and chunks kept for an
    unresolved type, feed cursor refusals, and the settled-horizon lag gauge.
  - Bounded label cardinality: no unbounded identifier is ever used as a label.
  - Test assertions that read instrument names from the declarations rather
    than from hand-copied strings, so a rename fails loudly.

- **Out of scope**:
  - Deciding when a metric fires; that is each calling component's
    responsibility, defined in the feature that owns the path.
  - The gear's own request-path signals and the shared metric namespace above
    this plugin's sub-namespace.
  - Histogram bucket layouts for the feed-page and reconciliation duration
    histograms, which DESIGN section 4.5 records as still open in the design.

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-uc-plugin-nfr-operational-visibility`

- **Design Principles Covered**: None -- metric emission states no design
  principle; it observes the paths the other entries define.

- **Design Constraints Covered**: None -- no constraint in DESIGN section 2.2
  binds the instrument inventory.

- **Domain Model Entities**:
  - Metric instrument inventory
  - Backend readiness signal

- **Design Components**:

  - [ ] `p1` - `cpt-cf-uc-plugin-component-metrics`

- **API**:
  - Push-based OpenTelemetry export under the plugin's metric sub-namespace
  - No SPI method; metrics are emitted, not queried

- **Sequences**: None -- recording an instrument is a step inside the
  sequences the other entries own rather than a flow of its own.

- **Data**: None -- metrics are exported, not stored; the schema is
  provisioned by 2.1 and this feature adds no table.

### 2.10 [Error Classification & Transport Security](feature-error-classification-transport-security/) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-uc-plugin-feature-error-classification-transport-security`

- **Purpose**: Owns the boundary every SPI call crosses: the adapter that
  translates backend failures into the SPI's six-variant error vocabulary, and
  the two security obligations that fall to this plugin alone. Because
  `cpt-cf-usage-collector-adr-pluggable-storage` makes the SPI the single seam
  to storage, the plugin is the only component in the gear-plus-plugin split
  that holds a database credential, opens the connection, and translates
  untrusted query shapes; transport confidentiality, credential
  non-disclosure and injection safety are therefore its obligations, while
  caller authentication and authorization remain gear-core concerns. A stable,
  classified vocabulary is what lets the host apply retry and fail-closed
  behavior without backend-specific parsing, realizing
  `cpt-cf-usage-collector-nfr-plugin-contract-stability` at the storage tier.

- **Depends On**: `cpt-cf-uc-plugin-feature-registration-schema-provisioning`

- **Scope**:
  - The six-variant error vocabulary returned from every SPI method, with each
    backend error classified as transient and retryable or internal and
    non-retryable.
  - A retry hint on a transient raised because the connection pool was
    saturated, so a caller can tell a busy backend from a failed one, and no
    hint on a transient from another cause.
  - The cursor-beyond-retention variant raised by the feed read alone, and the
    not-converged variant declared unreachable at this plugin's level.
  - A malformed or unauthorized call reaching the SPI surfaced as internal,
    because it is a host-contract breach.
  - TLS by default on every database connection: the silent-fallback SSL modes
    raised to require, a stronger operator choice preserved, and an explicit
    disable honoured as the one plaintext path with a warning emitted once per
    pool built.
  - The connection string held in a redacted secret wrapper so credentials
    never reach logs, error messages or debug output.
  - Injection-safe translation: every comparison value bound as a parameter and
    every SQL identifier mapped through a closed column allowlist, with an
    unrecognized identifier rejected as internal rather than emitted.
  - Metadata predicates parameterizing both the key and the compared value,
    without validating the key against the type's declared metadata fields.

- **Out of scope**:
  - Caller authentication, authorization and attribution enforcement, all
    performed by the gear core before every SPI call.
  - At-rest encryption, key management and masking, delegated to the operator's
    database deployment.
  - Closed-shape metadata validation, which stays upstream.

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-uc-plugin-fr-error-classification`
  - [ ] `p1` - `cpt-cf-uc-plugin-nfr-transport-security`

- **Design Principles Covered**: None -- the SPI-conformance principle that
  fixes the error vocabulary is carried by 2.1, which gates release on the
  contract suite; this feature implements the classification behind it.

- **Design Constraints Covered**:

  - [ ] `p1` - `cpt-cf-uc-plugin-constraint-injection-safe-translation`

- **Domain Model Entities**:
  - UsageCollectorPluginError
  - Redacted connection secret
  - SqlBind

- **Design Components**:

  - [ ] `p1` - `cpt-cf-uc-plugin-component-adapter`

  The claim covers the adapter's error-translation and transport
  responsibilities. Each SPI method arm the adapter carries is delivered by the
  feature that owns that method, so this entry does not wait on them.

- **API**:
  - All seven `UsageCollectorPluginV1` methods (error contract applies to each)
  - Database connection over the PostgreSQL wire protocol, TLS by default

- **Sequences**: None -- error translation and parameter binding are steps
  inside every sequence the other entries own rather than a flow of their own.

- **Data**: None -- the schema and the column allowlist's source of truth are
  provisioned by 2.1.

---

## 3. Feature Dependencies

```text
cpt-cf-uc-plugin-feature-registration-schema-provisioning   (foundation; owns the whole schema and the registration handshake)
    |
    +-- cpt-cf-uc-plugin-feature-record-ingestion-idempotency
    |       |
    |       +-- cpt-cf-uc-plugin-feature-invalidation-persistence
    |       |       |
    |       |       +-- cpt-cf-uc-plugin-feature-aggregated-query-rollup      (also <- record-ingestion-idempotency)
    |       |       +-- cpt-cf-uc-plugin-feature-reconciliation-metadata      (also <- record-ingestion-idempotency)
    |       |       +-- cpt-cf-uc-plugin-feature-usage-feed                   (also <- record-ingestion-idempotency)
    |       |               |
    |       |               +-- cpt-cf-uc-plugin-feature-per-type-retention
    |       |
    |       +-- cpt-cf-uc-plugin-feature-raw-query-converged-lookup
    |
    +-- cpt-cf-uc-plugin-feature-observability-metrics
    +-- cpt-cf-uc-plugin-feature-error-classification-transport-security
```

**Dependency Rationale**:

- `cpt-cf-uc-plugin-feature-record-ingestion-idempotency` requires
  `cpt-cf-uc-plugin-feature-registration-schema-provisioning`: the write path
  inserts into the ledger hypertable and resolves a type key from the type-key
  table, and the dedup identity is enforced by a unique constraint. All three
  are schema objects 2.1 provisions. There is nothing to write into until
  provisioning has run.
- `cpt-cf-uc-plugin-feature-invalidation-persistence` requires
  `cpt-cf-uc-plugin-feature-record-ingestion-idempotency`: a withdrawal is
  inserted through the same guarded statement, and its at-most-one bound is the
  dedup identity that feature establishes, applied with the entry type set to
  invalidation. It is separated out because the shared-identity rule is its own
  correctness property with its own tests, not because it needs a second write
  path.
- `cpt-cf-uc-plugin-feature-aggregated-query-rollup` requires both
  `cpt-cf-uc-plugin-feature-record-ingestion-idempotency` and
  `cpt-cf-uc-plugin-feature-invalidation-persistence`: it folds stored entries,
  and its exclusion rule and the rollup's signed netting are both defined over
  a record and the withdrawal that removed it. Neither can be tested until both
  kinds of entry can be persisted.
- `cpt-cf-uc-plugin-feature-raw-query-converged-lookup` requires
  `cpt-cf-uc-plugin-feature-record-ingestion-idempotency`: it reads the entries
  that path writes, and its converged-only semantics are stated against the
  dedup level that path declares. It does not depend on invalidation
  persistence, because the raw path returns the ledger as persisted and applies
  no withdrawal rule.
- `cpt-cf-uc-plugin-feature-usage-feed` requires both
  `cpt-cf-uc-plugin-feature-record-ingestion-idempotency` and
  `cpt-cf-uc-plugin-feature-invalidation-persistence`: feed order is keyed on
  the inserting transaction identifier the write path stamps, and the guarantee
  that a withdrawal follows the entry it withdraws holds only because the
  gateway accepts a withdrawal after its target has converged, so the
  withdrawal's transaction identifier is larger.
- `cpt-cf-uc-plugin-feature-per-type-retention` requires
  `cpt-cf-uc-plugin-feature-usage-feed`. This is the one dependency that runs
  against intuition, since deletion looks independent of reading. It does not
  hold in the other direction either: the sweep's drop transaction reads each
  type's highest feed position from the feed index and raises a retention mark
  in the same transaction that drops the chunk, and that mark exists only so
  the feed can refuse a position. The feed defines both the order the sweep
  reads and the refusal the mark drives, so retention is built on top of it.
- `cpt-cf-uc-plugin-feature-reconciliation-metadata` requires both
  `cpt-cf-uc-plugin-feature-record-ingestion-idempotency` and
  `cpt-cf-uc-plugin-feature-invalidation-persistence`: its accepted count
  includes withdrawals while its summary excludes withdrawn pairs, so the
  figures are only well-defined once both kinds of entry exist.
- `cpt-cf-uc-plugin-feature-observability-metrics` requires
  `cpt-cf-uc-plugin-feature-registration-schema-provisioning`: the instrument
  inventory is created during startup and the readiness signal is a property of
  the bound pool. It depends on nothing else, because it records what it is
  told and never decides when a metric fires.
- `cpt-cf-uc-plugin-feature-error-classification-transport-security` requires
  `cpt-cf-uc-plugin-feature-registration-schema-provisioning`: the SSL-mode
  resolution and the redacted connection secret are properties of the pool 2.1
  builds, and the column allowlist is defined over the schema 2.1 provisions.
  It does not depend on the read and write paths it classifies errors for; the
  adapter's translation is written against the backend's error surface, not
  against any one path.
- `cpt-cf-uc-plugin-feature-aggregated-query-rollup` and
  `cpt-cf-uc-plugin-feature-raw-query-converged-lookup` are independent of each
  other and can be developed in parallel, as can
  `cpt-cf-uc-plugin-feature-observability-metrics` and
  `cpt-cf-uc-plugin-feature-error-classification-transport-security`.

**Parallelization**:

- Tier 0 -- `registration-schema-provisioning` -- is the only feature with no
  prerequisite and must land first.
- Tier 1 -- `record-ingestion-idempotency`, `observability-metrics` and
  `error-classification-transport-security` -- can be built in parallel once
  tier 0 lands; none reads the others' output.
- Tier 2 -- `invalidation-persistence` and `raw-query-converged-lookup` -- can
  be built in parallel once ingestion lands. Each reuses ingestion's write path
  or reads its output independently of the other.
- Tier 3 -- `aggregated-query-rollup`, `usage-feed` and
  `reconciliation-metadata` -- can be built in parallel once invalidation
  persistence lands.
- Tier 4 -- `per-type-retention` -- is the only leaf below tier 3, waiting on
  the usage feed for the order it reads and the mark it raises.
