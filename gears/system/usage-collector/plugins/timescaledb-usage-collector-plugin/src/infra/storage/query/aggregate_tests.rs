//! Unit tests for the aggregation SELECT-expression builders. Pure (no DB):
//! they pin the exact SQL each [`AggregationOp`] emits, so a cast regression is
//! caught without Docker.

use usage_collector_sdk::AggregationOp;

use super::agg_select_expr;

#[test]
fn every_aggregate_op_casts_to_numeric() {
    assert_eq!(agg_select_expr(AggregationOp::Sum), "SUM(value)::numeric");
    assert_eq!(agg_select_expr(AggregationOp::Count), "COUNT(*)::numeric");
    assert_eq!(agg_select_expr(AggregationOp::Min), "MIN(value)::numeric");
    assert_eq!(agg_select_expr(AggregationOp::Max), "MAX(value)::numeric");
    assert_eq!(agg_select_expr(AggregationOp::Avg), "AVG(value)::numeric");
}
