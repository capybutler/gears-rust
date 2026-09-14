use super::{ADD_HISTORY_POLICY_SQL, ADD_LIVE_POLICY_SQL, DELETE_ROLLUP_POLICIES_SQL, ROLLUP_VIEW};

#[test]
fn every_policy_statement_names_the_rollup_view() {
    for sql in [
        DELETE_ROLLUP_POLICIES_SQL,
        ADD_LIVE_POLICY_SQL,
        ADD_HISTORY_POLICY_SQL,
    ] {
        assert!(sql.contains(ROLLUP_VIEW), "{sql}");
    }
}

#[test]
fn the_policy_delete_is_scoped_to_the_rollups_refresh_jobs() {
    assert!(
        DELETE_ROLLUP_POLICIES_SQL.contains("proc_name = 'policy_refresh_continuous_aggregate'")
    );
    assert!(DELETE_ROLLUP_POLICIES_SQL.contains("hypertable_schema = current_schema()"));
    assert!(DELETE_ROLLUP_POLICIES_SQL.contains("hypertable_name = 'usage_rollup_1h'"));
}

#[test]
fn the_history_policy_has_no_start_and_the_live_policy_does() {
    assert!(ADD_HISTORY_POLICY_SQL.contains("start_offset => NULL"));
    assert!(ADD_LIVE_POLICY_SQL.contains("start_offset => make_interval(secs => $1"));
}
