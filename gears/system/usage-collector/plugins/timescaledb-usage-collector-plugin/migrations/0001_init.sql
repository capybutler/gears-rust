-- TimescaleDB Usage Collector storage backend — base schema.
--
-- One ledger table plus the small keyed tables it needs: per-scope sequence
-- counters, per-type partitioning keys, and per-type feed retention marks.
-- There is no usage-type catalog: declarations live in types-registry and the
-- storage SPI never sees one (the gear's DESIGN §3.7; this plugin's own §3.7
-- states the target schema, which the migrations on this branch still trail).
--
-- This file replaces the pre-slice-4 schema and its rename migration outright
-- rather than migrating from them. The gear is unreleased, so no deployment
-- holds rows worth a migration path; the retired 0002 said as much in its own
-- header.
CREATE EXTENSION IF NOT EXISTS timescaledb;

CREATE TABLE IF NOT EXISTS usage_records (
    -- Deterministic gateway-derived entry identity: UUIDv5 over the 6-tuple
    -- dedup identity (cpt-cf-usage-collector-adr-record-identity-derivation).
    -- `entry_type` is its sixth input, and the only one a withdrawal does not
    -- share with the entry it withdraws.
    id                  uuid        NOT NULL,
    tenant_id           uuid        NOT NULL,
    gts_type_id         text        NOT NULL,
    -- The plugin-internal integer key of `gts_type_id`, assigned once per type
    -- from `usage_type_key` below. It is the hypertable's second partitioning
    -- dimension, so a chunk holds a slice of types and the retention sweep can
    -- drop it by the retention of the types in it. It is not a declared
    -- attribute (cpt-cf-usage-collector-adr-declaration-rehydration statement
    -- 6): it names the type, and a type's key never changes.
    type_key            int         NOT NULL,
    quantity            numeric     NOT NULL,
    -- The covered period [window_start, window_end). The only emitter-supplied
    -- time attribution. The time-range predicate reads the end alone
    -- (cpt-cf-usage-collector-adr-window-end-selection), which is why the end
    -- is the hypertable partition column.
    window_start        timestamptz NOT NULL,
    window_end          timestamptz NOT NULL,
    resource_id         text        NOT NULL,
    resource_type       text        NOT NULL,
    subject_id          text,
    subject_type        text,
    idempotency_key     text        NOT NULL,
    -- The append-only invalidation pair. An invalidation entry names the entry
    -- it withdraws and carries a reason; an ordinary measurement carries
    -- neither (cpt-cf-usage-collector-adr-append-only-invalidation).
    invalidates         uuid,
    reason_code         text,
    origin              text        NOT NULL
        CONSTRAINT usage_records_origin_valid
        CHECK (origin IN ('live', 'backfill')),
    -- Materialized so `$filter=entry_type eq 'invalidation'` resolves to a
    -- column. The SDK spells out exactly this expression and notes that the
    -- value hook cannot carry the field instead (models.rs, UsageRecordQuery).
    --
    -- Generated, and this migration trails the design on that (this plugin's
    -- DESIGN §4.5): §3.7's target schema makes it a `usage_entry_type` enum
    -- "written from the dispatched entry's declared kind and never derived
    -- from another column". When that lands it takes two things with it — the
    -- STORED rationale on the dedup UNIQUE below, which is what lets a
    -- generated column sit in one, and `row_dedup_key`'s reading of an entry's
    -- kind off `invalidates` (`record_store.rs`).
    entry_type          text        GENERATED ALWAYS AS
        (CASE WHEN invalidates IS NULL THEN 'record' ELSE 'invalidation' END) STORED,
    -- Strictly monotonic per (tenant_id, gts_type_id); assigned by this plugin,
    -- never by the gear (the gear's DESIGN §3.7). Claimed from
    -- `usage_acceptance_sequence` below. Gaps are permitted: the obligation is
    -- monotonicity, not density, and an absorbed idempotent retry consumes a
    -- value it does not store.
    --
    -- The counter row is the sole authority and the ledger does not re-check
    -- what it hands out, because no constraint here could. A hypertable UNIQUE
    -- must contain the partition column, and unlike the invalidation index
    -- below there is no reason two entries in one scope would share a
    -- `window_end` — so `UNIQUE (tenant_id, gts_type_id, acceptance_sequence,
    -- window_end)` admits a repeated sequence instead of rejecting it, and the
    -- form that would reject it is refused by the hypertable.
    acceptance_sequence bigint      NOT NULL,
    -- Gear-assigned acceptance instant, stamped by the Ingestion Gateway and
    -- written as given. An absorbed retry returns this stored value. `xact_id`
    -- below is declared between this column and `metadata` and is in neither
    -- `INSERT_COLUMNS` (`record_store.rs`) nor the binds, so that constant is
    -- this declaration order with `xact_id` dropped out of it — which still
    -- leaves `metadata` last, where the batch insert needs it (see the
    -- constant's doc).
    accepted_at         timestamptz NOT NULL,
    -- The inserting transaction's id, and the feed order's first key
    -- (this plugin's DESIGN §3.6 `cpt-cf-uc-plugin-seq-feed-page`). Stamped by
    -- this default and never bound by the Record Store, which is what makes
    -- every entry of one batch share one value and what lets a page read a
    -- settled horizon off `pg_snapshot_xmin` rather than off anything the
    -- plugin computes.
    --
    -- `xid8` rather than `xid`: `xid8` is 64-bit and totally ordered, where
    -- `xid` wraps around and compares only modulo 2^32.
    xact_id             xid8        NOT NULL DEFAULT pg_current_xact_id(),
    metadata            jsonb       NOT NULL DEFAULT '{}'::jsonb,

    -- A hypertable's PRIMARY KEY and every UNIQUE must contain every partition
    -- column, so both carry `window_end` and `type_key`. `type_key` is a
    -- function of `gts_type_id`, so adding it separates no two rows either key
    -- would otherwise join.
    --
    -- This is the same key as the dedup UNIQUE below, since `id` is a UUIDv5
    -- over that same 6-tuple. It is kept as defense in depth: while the
    -- derivation is correct the two are redundant, and a defect in it cannot
    -- then produce two rows for one identity.
    PRIMARY KEY (id, window_end, type_key),

    -- The gear's DESIGN §3.7 dedup obligation, over the §3.1 6-tuple verbatim, plus
    -- the partition key the hypertable requires (see the PRIMARY KEY above).
    --
    -- `entry_type` has to be in it. A withdrawal repeats its target's tenant,
    -- type, idempotency key and covered period, so a constraint over the first
    -- five alone would make every invalidation a collision with the very entry
    -- it withdraws. It can be in it because it is GENERATED … STORED: a stored
    -- generated column is an ordinary column to an index, so it may sit in a
    -- UNIQUE and be named as an ON CONFLICT arbiter. A VIRTUAL one could not.
    CONSTRAINT usage_records_dedup_uniq
        UNIQUE (tenant_id, gts_type_id, idempotency_key, window_start, window_end, entry_type, type_key),

    -- A point event is window_start == window_end; a period is strictly
    -- ordered. Nothing admits window_end < window_start.
    CONSTRAINT usage_records_window_ordered
        CHECK (window_start <= window_end),

    -- The pair is all-or-nothing: an invalidation names a target and carries a
    -- reason, an ordinary measurement does neither. A reason without a target
    -- would be an unmarked correction, which the model has no room for.
    CONSTRAINT usage_records_invalidation_pairing
        CHECK (
            (invalidates IS NULL AND reason_code IS NULL)
            OR (invalidates IS NOT NULL AND reason_code IS NOT NULL)
        ),

    -- `SubjectRef` makes `subject_id` required and `subject_type` optional
    -- (models.rs), so a type without an id is unrepresentable upstream. Pinned
    -- here for the same reason as the invalidation pair above: the ledger
    -- should not accept a shape the model cannot describe.
    CONSTRAINT usage_records_subject_pairing
        CHECK (subject_type IS NULL OR subject_id IS NOT NULL)
);

-- Partitioned on the covered-period end, then on the per-type key. The key's
-- slice width and the time interval are configuration, applied at startup to
-- chunks created afterwards (`pool::apply_post_migration_setup`).
SELECT create_hypertable('usage_records', by_range('window_end'), if_not_exists => TRUE);
SELECT add_dimension('usage_records', by_range('type_key', 1), if_not_exists => TRUE);

-- Lookup index for the fold's second withdrawal-exclusion obligation: "is this
-- entry named by an accepted invalidation?" `invalidates` leads it under
-- exactly that partial predicate. It is not a rule. At most one invalidation
-- per entry follows from the shared identity: every withdrawal of one entry
-- repeats that entry's five shared components and carries
-- `entry_type = invalidation`, so all of them land on one identity and a
-- second withdrawal is an ordinary collision on `usage_records_dedup_uniq`
-- (the gear's DESIGN §3.1 "At most one invalidation").
-- `window_end` and `type_key` are carried because an invalidation copies both
-- from its target, so a lookup by target can prune chunks on them.
CREATE INDEX IF NOT EXISTS usage_records_invalidates_idx
    ON usage_records (invalidates, window_end, type_key)
    WHERE invalidates IS NOT NULL;

-- Feed order, per this plugin's DESIGN §3.7. The subscription selects on
-- `gts_type_id`, the order is `(xact_id, id)` beneath it, and the compiled
-- scope is applied as a filter on the index-ordered merge rather than as a
-- leading key (§4.1 item 7). The retention sweep reads each type's highest
-- position in a chunk off this same index.
CREATE INDEX IF NOT EXISTS usage_records_feed_idx
    ON usage_records (gts_type_id, xact_id, id);

-- The reconciliation acceptance watermark, `MAX(accepted_at)` per
-- (gts_type_id, tenant_id) and unbounded by any range, per this plugin's
-- DESIGN §3.6 `cpt-cf-uc-plugin-seq-reconciliation`. No other index reaches
-- `accepted_at`, which is stamped upstream by the Ingestion Gateway and so does
-- not share an order with anything the plugin assigns.
CREATE INDEX IF NOT EXISTS usage_records_watermark_idx
    ON usage_records (gts_type_id, tenant_id, accepted_at DESC);

-- Per-GTS-type feed retention marks.
--
-- One row per GTS type that has lost an entry to retention, holding the highest
-- feed position among the entries of that type retention has deleted. A feed
-- page refuses a position a mark stands above (this plugin's DESIGN §3.6,
-- Retention refusal).
--
-- Created empty and stays empty in this slice: the retention sweep raises a
-- mark in the transaction that drops a chunk, and `read_feed_page` reads it,
-- and both are slice 3. An empty table here is the designed state, not an
-- unfinished one.
CREATE TABLE IF NOT EXISTS usage_feed_retention_marks (
    gts_type_id text NOT NULL PRIMARY KEY,
    xact_id     xid8 NOT NULL,
    id          uuid NOT NULL
);

-- Per-scope acceptance-sequence counters.
--
-- A Postgres SEQUENCE is global, and per-scope monotonicity would need one
-- sequence per (tenant, meter) — unbounded DDL driven by tenant data. A counter
-- row claimed with `ON CONFLICT DO UPDATE … RETURNING` is per-scope by
-- construction and serializes concurrent ingest for one scope on the row lock,
-- which is what strict monotonicity costs.
CREATE TABLE IF NOT EXISTS usage_acceptance_sequence (
    tenant_id   uuid   NOT NULL,
    gts_type_id text   NOT NULL,
    next_value  bigint NOT NULL,
    PRIMARY KEY (tenant_id, gts_type_id)
);

-- Per-type partitioning keys.
--
-- One row per GTS type this plugin has written, mapping the type to a small
-- integer the hypertable can partition on (`by_range` refuses a text column).
-- It stores no declared attribute and nothing references it; it is not a type
-- catalog. A key is assigned by the first write of its type and never changes,
-- which is what lets it sit inside the ledger's unique constraints.
CREATE TABLE IF NOT EXISTS usage_type_key (
    gts_type_id text NOT NULL PRIMARY KEY,
    type_key    int  GENERATED ALWAYS AS IDENTITY UNIQUE
);

-- Read paths select on the period end within a (tenant, meter) scope. The
-- trailing `acceptance_sequence` carries the LATEST fold's declared tie-break,
-- which is greatest `window_end` *then* greatest `acceptance_sequence` — so the
-- tie-break column has to follow the period end in the same index to be usable.
CREATE INDEX IF NOT EXISTS usage_records_tenant_type_window_idx
    ON usage_records (tenant_id, gts_type_id, window_end DESC, acceptance_sequence DESC);
CREATE INDEX IF NOT EXISTS usage_records_tenant_window_idx
    ON usage_records (tenant_id, window_end DESC);
-- The feed's future keyset: it orders by arrival rather than by the column
-- selection reads, scoped per (tenant, meter).
CREATE INDEX IF NOT EXISTS usage_records_acceptance_seq_idx
    ON usage_records (tenant_id, gts_type_id, acceptance_sequence DESC);
