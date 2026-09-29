use super::{
    CHUNK_HIGHEST_POSITIONS_SQL, DROP_CHUNK_SQL, LIST_CHUNKS_SQL, RAISE_MARKS_SQL,
    SET_LOCK_TIMEOUT_SQL, SWEEP_ADVISORY_LOCK_KEY,
};

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

#[test]
fn the_mark_raise_only_ever_raises() {
    // The `WHERE` on the `DO UPDATE` is the whole of "raises a row, never
    // lowers it" (this plugin's DESIGN §3.7). Chunks are swept in catalog
    // order rather than in time order, so a later sweep reaching an older
    // chunk presents a lower position for a type a newer chunk already
    // marked; without this predicate that sweep lowers the mark and the feed
    // then serves a range retention has truncated.
    assert!(
        RAISE_MARKS_SQL.contains("ON CONFLICT (gts_type_id) DO UPDATE"),
        "the mark is keyed on gts_type_id, which is the table's PRIMARY KEY: {RAISE_MARKS_SQL}"
    );
    assert!(
        RAISE_MARKS_SQL.contains(
            "WHERE (excluded.xact_id, excluded.id) > (usage_feed_retention_marks.xact_id, \
             usage_feed_retention_marks.id)"
        ),
        "without this conjunct the update lowers a mark: {RAISE_MARKS_SQL}"
    );
}

#[test]
fn the_chunk_position_read_groups_by_one_row_per_type() {
    // `ON CONFLICT ... DO UPDATE` raises "command cannot affect row a second
    // time" when one statement presents two rows sharing the conflict key, so
    // the read that feeds `RAISE_MARKS_SQL` must yield at most one row per
    // `gts_type_id`. The GROUP BY is what makes that true, and it is asserted
    // rather than trusted because the failure is a runtime error in a
    // transaction that also drops a chunk.
    assert!(
        CHUNK_HIGHEST_POSITIONS_SQL.contains("DISTINCT ON (gts_type_id)"),
        "one row per type or the raise errors: {CHUNK_HIGHEST_POSITIONS_SQL}"
    );
    assert!(
        CHUNK_HIGHEST_POSITIONS_SQL.contains("ORDER BY gts_type_id, xact_id DESC, id DESC"),
        "and the row `DISTINCT ON` keeps must be the greatest *pair*: this ORDER \
         BY is what picks it, and taking max(xact_id) beside max(id) would name \
         a position no entry carries: {CHUNK_HIGHEST_POSITIONS_SQL}"
    );
}

#[test]
fn the_drop_transaction_bounds_every_lock_wait_and_forces_no_commit_mode() {
    // DESIGN §3.6's sweep sequence puts `SET LOCAL lock_timeout = '5s'` on
    // the BEGIN line and bounds *every* lock wait in the transaction, not
    // only the chunk lock — "so a sweep waiting for a busy chunk does not
    // hold feed pages and other reads queued behind its lock request for
    // long".
    assert_eq!(SET_LOCK_TIMEOUT_SQL, "SET LOCAL lock_timeout = '5s'");
    // Ruling C19: §3.6's BEGIN line for *this* transaction omits the
    // durability statement deliberately, and the owner scoped §3.5's
    // universal to the paths that persist acknowledged entries. A lost sweep
    // commit costs nothing: the next sweep retries the chunk.
    assert!(
        !SET_LOCK_TIMEOUT_SQL.contains("synchronous_commit"),
        "ruling C19 keeps synchronous_commit off the sweep transaction"
    );
}
