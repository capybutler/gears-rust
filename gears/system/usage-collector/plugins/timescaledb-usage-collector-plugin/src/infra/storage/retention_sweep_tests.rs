use super::{
    CHUNK_HIGHEST_POSITIONS_SQL, DROP_CHUNK_SQL, LIST_CHUNKS_SQL, RAISE_MARKS_SQL,
    SET_LOCK_TIMEOUT_SQL, SWEEP_ADVISORY_LOCK_KEY,
};

// **These assert the query's *shape*, not that it runs.** A `contains` over a
// literal cannot tell a real catalog column from an invented one, so nothing
// here can catch a column the server lacks; that is
// `schema_integration_pg::the_chunk_catalog_query_reads_one_row_per_chunk_with_both_ranges`'s
// job. These earn their place only for the decisions below, each of which
// that test would pass either way.

#[test]
fn the_catalog_query_collapses_each_chunk_before_converting_its_time() {
    // Chaining the two dimension ranges without collapsing lets the planner
    // convert the key range as a timestamp.
    assert!(LIST_CHUNKS_SQL.contains("FILTER (WHERE d.column_name = 'window_end')"));
    assert!(LIST_CHUNKS_SQL.contains("FILTER (WHERE d.column_name = 'type_key')"));
    assert!(LIST_CHUNKS_SQL.contains("GROUP BY ch.id, ch.schema_name, ch.table_name"));
    assert!(LIST_CHUNKS_SQL.contains("WHERE h.table_name = 'usage_records'"));
    assert!(LIST_CHUNKS_SQL.contains("AND h.schema_name = current_schema()"));
}

#[test]
fn the_catalog_query_reaches_slices_through_chunk_constraint() {
    // On the pinned TimescaleDB `dimension_slice` has no `chunk_id`, so the
    // hop goes through `chunk_constraint`. A later release added that column,
    // making the direct join a *newer* spelling rather than a typo -- which
    // is why the longer form is pinned here instead of left to read as an
    // oversight.
    assert!(
        LIST_CHUNKS_SQL
            .contains("JOIN _timescaledb_catalog.chunk_constraint cc ON cc.chunk_id = ch.id"),
        "the chunk->slice hop must go through chunk_constraint: {LIST_CHUNKS_SQL}"
    );
    assert!(
        LIST_CHUNKS_SQL.contains(
            "JOIN _timescaledb_catalog.dimension_slice ds ON ds.id = cc.dimension_slice_id"
        ),
        "and the slice must be reached by its id: {LIST_CHUNKS_SQL}"
    );
}

#[test]
fn the_catalog_query_orders_by_chunk_id_not_by_chunk_name() {
    // No drop decision depends on visit order -- `RAISE_MARKS_SQL`'s own
    // conjunct already makes the committed mark independent of it -- but
    // `a_later_sweep_of_an_older_chunk_does_not_lower_the_mark` needs the
    // higher-position chunk visited first.
    //
    // **`ch.id`, never the formatted name.** Name order is *lexicographic*:
    // `_hyper_2_10_chunk` sorts before `_hyper_2_9_chunk`, silently reversing
    // that pair once a hypertable reaches ten chunks.
    assert!(
        LIST_CHUNKS_SQL.ends_with("ORDER BY ch.id"),
        "visit order must be creation order: {LIST_CHUNKS_SQL}"
    );
    assert!(
        !LIST_CHUNKS_SQL.contains("ORDER BY chunk"),
        "ordering by the formatted name is lexicographic, not creation order: {LIST_CHUNKS_SQL}"
    );
}

#[test]
fn the_chunk_name_is_quoted_and_schema_qualified_by_the_database() {
    // `drop_chunk_and_rollup_rows` interpolates this value into `LOCK TABLE
    // {} IN ACCESS EXCLUSIVE MODE` under `AssertSqlSafe`, and `%I` quoting is
    // what makes that sound. Plain concatenation would yield an unquoted name
    // and invalidate the justification at that call site.
    assert!(
        LIST_CHUNKS_SQL.contains("format('%I.%I', ch.schema_name, ch.table_name) AS chunk"),
        "the chunk name must be quoted by the database: {LIST_CHUNKS_SQL}"
    );
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
        RAISE_MARKS_SQL.contains("ON CONFLICT (gts_type_uuid) DO UPDATE"),
        "the mark is keyed on gts_type_uuid, which is the table's PRIMARY KEY: {RAISE_MARKS_SQL}"
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
    // `gts_type_uuid`. The GROUP BY is what makes that true, and it is asserted
    // rather than trusted because the failure is a runtime error in a
    // transaction that also drops a chunk.
    assert!(
        CHUNK_HIGHEST_POSITIONS_SQL.contains("DISTINCT ON (gts_type_uuid)"),
        "one row per type or the raise errors: {CHUNK_HIGHEST_POSITIONS_SQL}"
    );
    assert!(
        CHUNK_HIGHEST_POSITIONS_SQL.contains("ORDER BY gts_type_uuid, xact_id DESC, id DESC"),
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
    // §3.6's BEGIN line for *this* transaction omits the
    // durability statement deliberately, and the owner scoped §3.5's
    // universal to the paths that persist acknowledged entries. A lost sweep
    // commit costs nothing: the next sweep retries the chunk.
    assert!(
        !SET_LOCK_TIMEOUT_SQL.contains("synchronous_commit"),
        "synchronous_commit is deliberately off the sweep transaction"
    );
}
