// Test modules using bare `panic!` opt in explicitly
// (clippy.toml allows unwrap/expect in tests, not panic).
#![allow(clippy::panic)]

use usage_collector_sdk::AggregationFold;

use super::super::aggregate::{LATEST_SELECT_EXPR, fold_select_expr, withdrawal_exclusion_clause};
use super::{
    ReconciliationSummaryStatement, ReconciliationWatermarksStatement,
    accrued_summary_select_expr, build_reconciliation_summary_sql,
    build_reconciliation_watermarks_sql, observation_count_select_expr,
    observation_latest_select_expr,
};
use crate::infra::storage::query::translate::SqlBind;
use crate::infra::storage::test_support::{
    RANGE_FROM_UNIX, RANGE_TO_UNIX, RECONCILIATION_TENANT, SCOPE_TENANT_A, every_fold,
    list_meter, list_range, parse_scope,
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

// --- The assembled statements: `build_reconciliation_summary_sql` and
// `build_reconciliation_watermarks_sql` (spec §9.3) ---

#[test]
fn the_summary_statement_reports_accrued_for_sum() {
    // S1∧S2: `accepted_count` is a bare COUNT(*), and a `Sum` fold takes the
    // accrued branch alone — no observation columns beside it.
    let ReconciliationSummaryStatement { sql, .. } = build_reconciliation_summary_sql(
        list_meter(),
        list_range(),
        RECONCILIATION_TENANT,
        AggregationFold::Sum,
        &parse_scope(&format!("tenant_id eq {SCOPE_TENANT_A}")),
    )
    .expect("a well-formed reconciliation summary must render");

    assert!(
        sql.starts_with(&format!(
            "SELECT COUNT(*), {}",
            accrued_summary_select_expr()
        )),
        "a SUM fold must select COUNT(*) then the accrued branch alone. got: {sql}"
    );
    assert!(
        !sql.contains(&observation_count_select_expr()),
        "the accrued branch must not also carry the observation pair. got: {sql}"
    );
}

#[test]
fn the_summary_statement_reports_the_observation_pair_for_every_other_fold() {
    // Every fold but `Sum` takes the fixed Observations pair — never a
    // per-fold SELECT list, which is the "two-shape statement" this builder
    // is held to.
    for fold in every_fold() {
        if fold == AggregationFold::Sum {
            continue;
        }
        let ReconciliationSummaryStatement { sql, .. } = build_reconciliation_summary_sql(
            list_meter(),
            list_range(),
            RECONCILIATION_TENANT,
            fold,
            &parse_scope(&format!("tenant_id eq {SCOPE_TENANT_A}")),
        )
        .expect("a well-formed reconciliation summary must render");

        assert!(
            sql.starts_with(&format!(
                "SELECT COUNT(*), {}, {}",
                observation_count_select_expr(),
                observation_latest_select_expr()
            )),
            "fold {fold:?} must select COUNT(*), the observation count, then \
             latest_observation — the same fixed pair as every other \
             non-accruing fold. got: {sql}"
        );
        assert!(
            !sql.contains(&accrued_summary_select_expr()),
            "a non-accruing fold must not also carry the accrued branch. got: {sql}"
        );
    }
}

#[test]
fn the_summary_statement_binds_the_meter_range_tenant_then_scope() {
    let ReconciliationSummaryStatement { binds, .. } = build_reconciliation_summary_sql(
        list_meter(),
        list_range(),
        RECONCILIATION_TENANT,
        AggregationFold::Sum,
        &parse_scope(&format!("tenant_id eq {SCOPE_TENANT_A}")),
    )
    .expect("a well-formed reconciliation summary must render");

    assert_eq!(
        binds.len(),
        5,
        "the meter, both range bounds, the call's own tenant_id, and the \
         compiled scope's one operand. got: {binds:?}"
    );
    assert!(matches!(&binds[0], SqlBind::Uuid(u) if *u == list_meter()));
    assert!(matches!(&binds[1], SqlBind::DateTime(t) if t.unix_timestamp() == RANGE_FROM_UNIX));
    assert!(matches!(&binds[2], SqlBind::DateTime(t) if t.unix_timestamp() == RANGE_TO_UNIX));
    assert!(
        matches!(&binds[3], SqlBind::Uuid(u) if *u == RECONCILIATION_TENANT),
        "$4 is the call's own tenant_id, bound ahead of the scope. got: {:?}",
        binds[3]
    );
    assert!(
        matches!(&binds[4], SqlBind::Uuid(u) if u.to_string() == SCOPE_TENANT_A),
        "$5 is the compiled scope's tenant, a second and independent \
         predicate from $4. got: {:?}",
        binds[4]
    );
}

#[test]
fn the_summary_statement_carries_no_group_by_or_limit() {
    // Spec §9.3: one ranged scan, ungrouped. `PostgreSQL` answers exactly one
    // row for a bare aggregate, which is what lets `accepted_count == 0` and
    // an empty summary fall out with no branch of their own.
    let ReconciliationSummaryStatement { sql, .. } = build_reconciliation_summary_sql(
        list_meter(),
        list_range(),
        RECONCILIATION_TENANT,
        AggregationFold::Count,
        &parse_scope(&format!("tenant_id eq {SCOPE_TENANT_A}")),
    )
    .expect("a well-formed reconciliation summary must render");

    assert!(!sql.contains("GROUP BY"), "got: {sql}");
    assert!(!sql.contains("LIMIT"), "got: {sql}");
}

#[test]
fn the_summary_statement_never_references_the_rollup() {
    // DECOMPOSITION §2.8 puts "Serving the summary from the materialised
    // aggregate" explicitly out of scope, and `no-paging-no-rollup` states it as a
    // MUST. It cannot be pinned numerically for a `Sum` fold: `usage_rollup_1h` is
    // a real-time continuous aggregate, never empty after a write, and its
    // signed-netting formula is mathematically identical to the exact scan's
    // FILTER-excluded sum for every fixture shape. No numeric fixture can tell the
    // two apart, so a structural assertion on the rendered SQL is the only thing
    // that catches a change routing the accrued branch through `rollup_eligible`.
    for fold in every_fold() {
        let ReconciliationSummaryStatement { sql, .. } = build_reconciliation_summary_sql(
            list_meter(),
            list_range(),
            RECONCILIATION_TENANT,
            fold,
            &parse_scope(&format!("tenant_id eq {SCOPE_TENANT_A}")),
        )
        .expect("a well-formed reconciliation summary must render");

        assert!(
            !sql.to_lowercase().contains("rollup"),
            "fold {fold:?}: the reconciliation summary must always take the \
             exact scan over usage_records, never usage_rollup_1h. got: {sql}"
        );
    }
}

#[test]
fn an_untranslatable_scope_refuses_the_summary_statement() {
    let Err(err) = build_reconciliation_summary_sql(
        list_meter(),
        list_range(),
        RECONCILIATION_TENANT,
        AggregationFold::Sum,
        &parse_scope("gts_type_uuid eq 'gts.cf.core.uc.usage_record.v1~'"),
    ) else {
        panic!("an untranslatable scope must not yield a summary statement");
    };
    assert!(err.contains("gts_type_uuid"), "got: {err}");
}

#[test]
fn the_watermarks_statement_is_two_order_by_limit_one_subqueries_with_no_range() {
    // unbounded by the range, and each watermark its own `ORDER BY …
    // LIMIT 1` so the leading edge of its own index stays reachable — never
    // a bare `MAX(…)` and never folded into S1∧S2's ranged `WHERE`.
    let ReconciliationWatermarksStatement { sql, .. } = build_reconciliation_watermarks_sql(
        list_meter(),
        RECONCILIATION_TENANT,
        &parse_scope(&format!("tenant_id eq {SCOPE_TENANT_A}")),
    )
    .expect("a well-formed watermarks scope must render");

    assert!(
        sql.contains("ORDER BY r.accepted_at DESC LIMIT 1"),
        "got: {sql}"
    );
    assert!(
        sql.contains("ORDER BY r.window_end DESC LIMIT 1"),
        "got: {sql}"
    );
    assert!(
        !sql.contains("window_end >=") && !sql.contains("window_end <"),
        "S3 is unbounded by the range: it must carry no range predicate. got: {sql}"
    );
}

#[test]
fn the_watermarks_statement_binds_the_meter_tenant_and_scope_once_each_reused_by_both_subqueries() {
    let ReconciliationWatermarksStatement { sql, binds } = build_reconciliation_watermarks_sql(
        list_meter(),
        RECONCILIATION_TENANT,
        &parse_scope(&format!("tenant_id eq {SCOPE_TENANT_A}")),
    )
    .expect("a well-formed watermarks scope must render");

    assert_eq!(
        binds.len(),
        3,
        "the meter (its partition-key subquery reuses the same bind), the \
         tenant, and the scope's one operand — never doubled for the two \
         subqueries. got: {binds:?}"
    );
    assert!(matches!(&binds[0], SqlBind::Uuid(u) if *u == list_meter()));
    assert!(matches!(&binds[1], SqlBind::Uuid(u) if *u == RECONCILIATION_TENANT));
    assert!(matches!(&binds[2], SqlBind::Uuid(u) if u.to_string() == SCOPE_TENANT_A));
    assert_eq!(
        sql.matches("$1").count(),
        4,
        "$1 (the meter) is read twice per subquery — its own gts_type_uuid \
         predicate plus the partition-key subquery inside it — across both \
         subqueries, bound once. got: {sql}"
    );
    assert_eq!(
        sql.matches("$3").count(),
        2,
        "$3 (the scope) is spliced into both subqueries verbatim, bound \
         once. got: {sql}"
    );
}

#[test]
fn an_untranslatable_scope_refuses_the_watermarks_statement() {
    let Err(err) = build_reconciliation_watermarks_sql(
        list_meter(),
        RECONCILIATION_TENANT,
        &parse_scope("gts_type_uuid eq 'gts.cf.core.uc.usage_record.v1~'"),
    ) else {
        panic!("an untranslatable scope must not yield a watermarks statement");
    };
    assert!(err.contains("gts_type_uuid"), "got: {err}");
}

