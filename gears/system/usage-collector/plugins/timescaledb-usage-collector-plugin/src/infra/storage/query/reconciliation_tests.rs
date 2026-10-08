// Test modules using bare `panic!` opt in explicitly
// (clippy.toml allows unwrap/expect in tests, not panic).
#![allow(clippy::panic)]

use usage_collector_sdk::AggregationFold;

use super::super::aggregate::{LATEST_SELECT_EXPR, fold_select_expr, withdrawal_exclusion_clause};
use super::{
    accrued_summary_select_expr, observation_count_select_expr, observation_latest_select_expr,
};

#[test]
fn the_accrued_summary_expr_is_the_sum_fold_with_the_exclusion_filtered_on_the_call() {
    // Oracle: before this function existed there was no `FILTER`-qualified
    // SUM anywhere in the crate, so this string did not exist to pin.
    // Transcribed by hand from `fold_select_expr(Sum)` with `FILTER (WHERE
    // <exclusion>)` spliced after `SUM(r.quantity)` and before the `, 0)`
    // the COALESCE closes with.
    assert_eq!(
        accrued_summary_select_expr(),
        format!(
            "COALESCE(SUM(r.quantity) FILTER (WHERE {}), 0)::numeric",
            withdrawal_exclusion_clause()
        )
    );
}

#[test]
fn the_accrued_summary_expr_wraps_exactly_the_sum_folds_own_shape() {
    // The COALESCE/cast shape must be `fold_select_expr(Sum)`'s own, not a
    // second transcription: strip the spliced FILTER clause back out and the
    // two must agree byte for byte.
    let filtered = accrued_summary_select_expr();
    let spliced = format!(" FILTER (WHERE {})", withdrawal_exclusion_clause());
    let unfiltered = filtered.replacen(&spliced, "", 1);
    assert_eq!(unfiltered, fold_select_expr(AggregationFold::Sum));
}

#[test]
fn the_observation_count_expr_is_a_filtered_count_star() {
    assert_eq!(
        observation_count_select_expr(),
        format!("COUNT(*) FILTER (WHERE {})", withdrawal_exclusion_clause())
    );
}

#[test]
fn the_observation_latest_expr_is_the_latest_pick_with_the_exclusion_filtered_on_array_agg() {
    // Transcribed by hand: the FILTER clause sits on the ARRAY_AGG call
    // itself, inside the parens `[1]` picks the head of — not on the whole
    // expression and not after the `::numeric` cast.
    assert_eq!(
        observation_latest_select_expr(),
        "(ARRAY_AGG(r.quantity ORDER BY r.window_end DESC, r.accepted_at DESC, r.id DESC) \
         FILTER (WHERE r.invalidates IS NULL AND NOT EXISTS (SELECT 1 FROM usage_records w \
         WHERE w.invalidates = r.id AND w.type_key = r.type_key)))[1]::numeric"
    );
}

#[test]
fn the_observation_latest_expr_wraps_exactly_latest_select_exprs_own_shape() {
    // Same drift guard as the accrued case: strip the FILTER back out and the
    // two must agree byte for byte with the aggregate path's own constant.
    let filtered = observation_latest_select_expr();
    let spliced = format!(" FILTER (WHERE {})", withdrawal_exclusion_clause());
    let unfiltered = filtered.replacen(&spliced, "", 1);
    assert_eq!(unfiltered, LATEST_SELECT_EXPR);
}

#[test]
fn every_reconciliation_summary_expr_casts_to_numeric() {
    // So every summary column reads back uniformly, exactly like every
    // `fold_select_expr` arm.
    assert!(accrued_summary_select_expr().ends_with("::numeric"));
    assert!(observation_latest_select_expr().ends_with("::numeric"));
}

#[test]
fn the_observation_pair_shares_one_filter_predicate() {
    // The two halves of the `Observations` branch must exclude the identical
    // selection, or `count` and `latest` could disagree about which rows
    // were withdrawn.
    let count_expr = observation_count_select_expr();
    let latest_expr = observation_latest_select_expr();
    let predicate = withdrawal_exclusion_clause();
    assert!(count_expr.contains(predicate));
    assert!(latest_expr.contains(predicate));
}
