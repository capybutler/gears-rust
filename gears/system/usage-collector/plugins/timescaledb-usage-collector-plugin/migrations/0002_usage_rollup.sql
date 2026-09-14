-- TimescaleDB Usage Collector storage backend — the SUM/COUNT rollup.
--
-- An hourly real-time continuous aggregate over the ledger. The aggregate read
-- path serves whole UTC hours of a SUM or COUNT query from it; everything else
-- reads the ledger. It is plugin-internal: no SPI shape names it.
--
-- Signed netting is exact. The scan excludes a withdrawn pair; this view adds
-- +value/+1 for a record and -value/-1 for its invalidation instead. The two
-- agree because (1) an invalidation copies its target's window_end, tenant_id
-- and type, so both land in one row here; (2) a record carries at most one
-- invalidation (usage_records_one_invalidation_uniq plus the gateway;
-- DIVERGENCES.md entry 21); (3) the gateway admits an invalidation only for an
-- existing record; (4) the pair shares a chunk, so retention drops it together.
--
-- type_key adds no rows (it is a function of gts_type_id). It is in the grain
-- so reads and the retention sweep's rollup cut can prune by type.
--
-- Refresh policies are not created here: startup setup applies them from
-- configuration (pool::apply_post_migration_setup).
CREATE MATERIALIZED VIEW IF NOT EXISTS usage_rollup_1h
WITH (timescaledb.continuous, timescaledb.materialized_only = false) AS
SELECT time_bucket(INTERVAL '1 hour', window_end) AS bucket,
       tenant_id,
       gts_type_id,
       type_key,
       SUM(CASE WHEN invalidates IS NULL THEN value ELSE -value END) AS sum_value,
       SUM(CASE WHEN invalidates IS NULL THEN 1 ELSE -1 END)::bigint AS count_value
FROM usage_records
GROUP BY 1, 2, 3, 4
WITH NO DATA;
