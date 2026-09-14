use super::{DROP_CHUNK_SQL, LIST_CHUNKS_SQL, SWEEP_ADVISORY_LOCK_KEY};

#[test]
fn the_catalog_query_collapses_each_chunk_before_converting_its_time() {
    // Chaining the two dimension ranges without collapsing lets the planner
    // convert the key range as a timestamp (`TIMESCALEDB-RETENTION.md` §8.1).
    assert!(LIST_CHUNKS_SQL.contains("FILTER (WHERE d.column_name = 'window_end')"));
    assert!(LIST_CHUNKS_SQL.contains("FILTER (WHERE d.column_name = 'type_key')"));
    assert!(LIST_CHUNKS_SQL.ends_with("GROUP BY ch.relid"));
    assert!(LIST_CHUNKS_SQL.contains("WHERE h.table_name = 'usage_records'"));
    assert!(LIST_CHUNKS_SQL.contains("AND h.schema_name = current_schema()"));
}

#[test]
fn a_chunk_is_dropped_by_its_regclass() {
    assert_eq!(
        DROP_CHUNK_SQL,
        "SELECT _timescaledb_functions.drop_chunk($1::regclass)"
    );
}

#[test]
fn the_sweep_lock_is_not_the_init_lock() {
    // `pool::INIT_ADVISORY_LOCK_KEY` is `0x7563_7462`; sharing it would make a
    // sweep and a replica's startup setup block each other.
    assert_ne!(SWEEP_ADVISORY_LOCK_KEY, 0x7563_7462);
}

#[test]
fn the_catalog_query_reads_the_time_range_start_too() {
    assert!(LIST_CHUNKS_SQL.contains(
        "_timescaledb_functions.to_timestamp(\
         max(ds.range_start) FILTER (WHERE d.column_name = 'window_end')) AS time_start"
    ));
}
