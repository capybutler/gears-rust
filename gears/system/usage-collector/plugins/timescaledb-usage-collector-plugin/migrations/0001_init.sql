-- TimescaleDB Usage Collector storage backend — base schema.
--
-- One ledger table plus its per-scope sequence counters. There is no
-- usage-type catalog: declarations live in types-registry and the storage SPI
-- never sees one (the gear's DESIGN §3.7 — not this plugin's, whose §3.7 still
-- describes the retired schema).
--
-- This file replaces the pre-slice-4 schema and its rename migration outright
-- rather than migrating from them. The gear is unreleased, so no deployment
-- holds rows worth a migration path; the retired 0002 said as much in its own
-- header.
CREATE EXTENSION IF NOT EXISTS timescaledb;

CREATE TABLE IF NOT EXISTS usage_records (
    -- Deterministic gateway-derived entry identity: UUIDv5 over the 5-tuple
    -- dedup identity (cpt-cf-usage-collector-adr-record-identity-derivation).
    -- `entry_type` is deliberately not an input to it.
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
    value               numeric     NOT NULL,
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
    metadata            jsonb       NOT NULL DEFAULT '{}'::jsonb,
    ingested_at         timestamptz NOT NULL DEFAULT now(),

    -- A hypertable's PRIMARY KEY and every UNIQUE must contain every partition
    -- column, so both carry `window_end` and `type_key`. `type_key` is a
    -- function of `gts_type_id`, so adding it separates no two rows either key
    -- would otherwise join.
    --
    -- This is the same key as the dedup UNIQUE below, since `id` is a UUIDv5
    -- over that same 5-tuple. It is kept as defense in depth: while the
    -- derivation is correct the two are redundant, and a defect in it cannot
    -- then produce two rows for one identity.
    PRIMARY KEY (id, window_end, type_key),

    -- The gear's DESIGN §3.7 dedup obligation, over the 5-tuple verbatim, plus
    -- the partition key the hypertable requires (see the PRIMARY KEY above).
    CONSTRAINT usage_records_dedup_uniq
        UNIQUE (tenant_id, gts_type_id, idempotency_key, window_start, window_end, type_key),

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

-- At most one accepted invalidation per entry, enforced by the database rather
-- than by a read-then-write in the store.
--
-- Why this can be a plain UNIQUE despite the hypertable partition-column rule:
-- an invalidation is a faithful copy of the entry it withdraws, so it shares
-- that entry's covered period and therefore its `window_end`. Two invalidations
-- of one target necessarily collide on (invalidates, window_end), so including
-- the partition columns costs nothing — an invalidation also shares its
-- target's type, and so its `type_key`.
--
-- It also answers the fold's second withdrawal-exclusion obligation — "is this
-- entry named by an accepted invalidation?" — since `invalidates` leads it
-- under exactly that partial predicate. A separate index on (invalidates)
-- would be wholly subsumed by this one; there deliberately is not one.
CREATE UNIQUE INDEX IF NOT EXISTS usage_records_one_invalidation_uniq
    ON usage_records (invalidates, window_end, type_key)
    WHERE invalidates IS NOT NULL;

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
