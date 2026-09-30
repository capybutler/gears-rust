use super::HORIZON_LAG_SQL;

#[test]
fn the_horizon_lag_query_filters_to_backends_holding_a_write_transaction() {
    assert!(HORIZON_LAG_SQL.contains("backend_xid IS NOT NULL"));
    assert!(HORIZON_LAG_SQL.contains("pg_stat_activity"));
    assert!(HORIZON_LAG_SQL.contains("min(xact_start)"));
}
