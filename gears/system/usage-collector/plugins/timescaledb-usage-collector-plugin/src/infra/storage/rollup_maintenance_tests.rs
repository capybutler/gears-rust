use super::{
    ADD_HISTORY_POLICY_SQL, ADD_LIVE_POLICY_SQL, DELETE_ROLLUP_POLICIES_SQL,
    MATERIALIZATION_TABLE_SQL, ROLLUP_VIEW, RefreshPolicy, delete_rollup_rows_sql,
    job_status_from_row,
};

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

#[test]
fn the_materialisation_table_is_read_from_the_public_information_view() {
    assert_eq!(
        MATERIALIZATION_TABLE_SQL,
        "SELECT format('%I.%I', materialization_hypertable_schema, materialization_hypertable_name) \
         FROM timescaledb_information.continuous_aggregates \
         WHERE view_schema = current_schema() AND view_name = 'usage_rollup_1h'"
    );
}

#[test]
fn the_rollup_cut_removes_only_buckets_wholly_inside_the_chunk_and_its_key_range() {
    assert_eq!(
        delete_rollup_rows_sql("_timescaledb_internal._materialized_hypertable_2"),
        "DELETE FROM _timescaledb_internal._materialized_hypertable_2 \
         WHERE type_key >= $1::bigint AND type_key < $2::bigint \
         AND bucket >= $3 AND bucket + INTERVAL '1 hour' <= $4"
    );
}

#[test]
fn a_policy_with_no_start_offset_is_the_history_policy() {
    assert_eq!(
        job_status_from_row(true, Some("Success"), Some(5.0)).policy,
        RefreshPolicy::History
    );
    assert_eq!(
        job_status_from_row(false, Some("Success"), Some(5.0)).policy,
        RefreshPolicy::Live
    );
}

#[test]
fn only_a_failed_last_run_is_failing() {
    assert!(job_status_from_row(false, Some("Failure"), Some(5.0)).failing);
    assert!(!job_status_from_row(false, Some("Success"), Some(5.0)).failing);
    assert!(
        !job_status_from_row(false, None, None).failing,
        "a policy that never ran has not failed"
    );
}

#[test]
fn a_policy_that_never_succeeded_reports_no_age() {
    // `last_successful_finish` is -infinity until the first success, so the
    // age reads back as +infinity.
    assert_eq!(
        job_status_from_row(false, None, Some(f64::INFINITY)).secs_since_success,
        None
    );
    assert_eq!(
        job_status_from_row(false, None, None).secs_since_success,
        None
    );
    assert_eq!(
        job_status_from_row(false, Some("Success"), Some(-1.0)).secs_since_success,
        Some(0.0)
    );
    assert_eq!(
        job_status_from_row(false, Some("Success"), Some(42.5)).secs_since_success,
        Some(42.5)
    );
}

#[test]
fn the_policy_labels_are_live_and_history() {
    assert_eq!(
        [
            RefreshPolicy::Live.as_label(),
            RefreshPolicy::History.as_label()
        ],
        ["live", "history"]
    );
}
