//! `PluginOp::ALL` compile-time-pin coverage.

use super::PluginOp;

#[test]
fn plugin_op_all_lists_every_variant() {
    use crate::domain::ports::metrics::PluginOp as P;
    // No wildcard: a seventh variant fails to compile here until `ALL` is
    // updated, and once updated the span's operation set follows with no
    // further edit.
    fn listed(op: P) -> bool {
        match op {
            P::CreateUsageRecords
            | P::QueryAggregatedUsageRecords
            | P::ListUsageRecords
            | P::GetUsageRecord
            | P::ReadFeedPage
            | P::GetReconciliationMetadata => true,
        }
    }
    assert_eq!(PluginOp::ALL.len(), 6);
    for op in PluginOp::ALL {
        assert!(listed(*op));
    }
}
