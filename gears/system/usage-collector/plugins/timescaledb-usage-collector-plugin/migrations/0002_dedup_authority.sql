-- Dedup authority: a NORMAL table (not a hypertable) so its unique key is the
-- true 3-tuple with no created_at. Its PK is the atomic serialization point
-- for concurrent same-key ingest (fixes the A1 TOCTOU race).
-- `IF NOT EXISTS` / `CREATE OR REPLACE` so a manual replay or partial-apply
-- recovery is idempotent (mirrors migration 0001's pattern).
CREATE TABLE IF NOT EXISTS usage_dedup (
    tenant_id         uuid        NOT NULL,
    gts_id            text        NOT NULL,
    idempotency_key   text        NOT NULL,
    record_uuid       uuid        NOT NULL,
    record_created_at timestamptz NOT NULL,
    PRIMARY KEY (tenant_id, gts_id, idempotency_key)
);

-- Serves the cleanup anti-join's time bound.
CREATE INDEX IF NOT EXISTS usage_dedup_record_created_at_idx ON usage_dedup (record_created_at);

-- Cleanup procedure for the scheduled job (registered by apply_dedup_cleanup_job).
-- Deletes a dedup row older than retention ONLY when its pointed-to record no
-- longer exists, holding the invariant "dedup row exists iff its record exists".
CREATE OR REPLACE PROCEDURE prune_usage_dedup(job_id int, config jsonb)
LANGUAGE plpgsql AS $$
DECLARE
    -- Parsed as bigint to match the type apply_dedup_cleanup_job writes
    -- (jsonb_build_object('retention_secs', $1::bigint)); make_interval's secs
    -- parameter takes it via the implicit bigint -> double precision promotion.
    retention interval := make_interval(secs => (config->>'retention_secs')::bigint);
BEGIN
    DELETE FROM usage_dedup d
     WHERE d.record_created_at < now() - retention
       AND NOT EXISTS (
           SELECT 1 FROM usage_records r
            WHERE r.uuid = d.record_uuid
              AND r.created_at = d.record_created_at);
END;
$$;
