#![allow(clippy::panic)]

use time::OffsetDateTime;
use toolkit_odata::ast;
use usage_collector_sdk::{
    AggregationDimension, AggregationFold, MetadataFilter, MetadataKey, MeterTypeId, TimeRange,
};

use super::{FallbackReason, HourSplit, build_rollup_aggregate_sql, hour_split, rollup_eligible};
use crate::infra::storage::query::translate::SqlBind;

/// `2023-11-14T22:00:00Z`, an exact hour.
const H0: i64 = 1_699_999_200;
const HOUR: i64 = 3_600;

fn at(unix: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).expect("valid instant")
}

fn range(from: i64, to: i64) -> TimeRange {
    TimeRange::new(at(from), at(to)).expect("ordered range")
}

fn parsed(raw: &str) -> ast::Expr {
    toolkit_odata::parse_filter_string(raw)
        .unwrap_or_else(|e| panic!("the test's own filter must parse: {e}"))
        .into_expr()
}

// ── hour_split ─────────────────────────────────────────────────────────────

#[test]
fn an_aligned_range_has_no_edges() {
    let split = hour_split(range(H0, H0 + 24 * HOUR)).expect("whole hours");
    assert_eq!(split.whole_from, at(H0));
    assert_eq!(split.whole_to, at(H0 + 24 * HOUR));
    assert!(!split.has_lower_edge() && !split.has_upper_edge());
}

#[test]
fn an_unaligned_range_rounds_inward_on_both_sides() {
    let split = hour_split(range(H0 + 800, H0 + 5 * HOUR + 420)).expect("whole hours");
    assert_eq!(
        split,
        HourSplit {
            from: at(H0 + 800),
            whole_from: at(H0 + HOUR),
            whole_to: at(H0 + 5 * HOUR),
            to: at(H0 + 5 * HOUR + 420),
        }
    );
    assert!(split.has_lower_edge() && split.has_upper_edge());
}

#[test]
fn one_aligned_bound_leaves_one_edge() {
    let split = hour_split(range(H0, H0 + 2 * HOUR + 1)).expect("whole hours");
    assert!(!split.has_lower_edge() && split.has_upper_edge());
}

#[test]
fn exactly_one_hour_is_one_whole_bucket() {
    let split = hour_split(range(H0 + HOUR, H0 + 2 * HOUR)).expect("one bucket");
    assert_eq!(
        (split.whole_from, split.whole_to),
        (at(H0 + HOUR), at(H0 + 2 * HOUR))
    );
}

#[test]
fn no_whole_hour_means_no_split() {
    assert_eq!(
        hour_split(range(H0 + 600, H0 + 3_000)),
        None,
        "inside one hour"
    );
    assert_eq!(
        hour_split(range(H0 + 3_000, H0 + HOUR + 600)),
        None,
        "crosses a boundary"
    );
}

#[test]
fn a_sub_second_lower_bound_rounds_up_to_the_next_hour() {
    let from = at(H0) + time::Duration::nanoseconds(1);
    let split =
        hour_split(TimeRange::new(from, at(H0 + 3 * HOUR)).expect("ordered")).expect("whole hours");
    assert_eq!(split.whole_from, at(H0 + HOUR));
    assert_eq!(split.from, from, "the edge keeps the exact bound");
}

#[test]
fn a_bound_before_the_epoch_floors_toward_minus_infinity() {
    let split = hour_split(range(-5_400, 5_400)).expect("whole hours");
    assert_eq!((split.whole_from, split.whole_to), (at(-3_600), at(3_600)));
}

// ── rollup_eligible ────────────────────────────────────────────────────────

fn aligned() -> TimeRange {
    range(H0, H0 + 24 * HOUR)
}

#[test]
fn sum_and_count_over_whole_hours_with_a_tenant_filter_are_eligible() {
    let tenant_filters = [
        parsed("tenant_id eq 11111111-1111-1111-1111-111111111111"),
        parsed(
            "tenant_id eq 11111111-1111-1111-1111-111111111111 \
             or tenant_id eq 22222222-2222-2222-2222-222222222222",
        ),
        parsed("tenant_id in (11111111-1111-1111-1111-111111111111)"),
    ];
    for fold in [AggregationFold::Sum, AggregationFold::Count] {
        for group_by in [vec![], vec![AggregationDimension::TenantId]] {
            assert!(rollup_eligible(fold, None, &[], &group_by, aligned()).is_ok());
            for f in &tenant_filters {
                assert!(
                    rollup_eligible(fold, Some(f), &[], &group_by, aligned()).is_ok(),
                    "{fold:?} {group_by:?} {f:?}"
                );
            }
        }
    }
}

#[test]
fn the_other_folds_fall_back() {
    for fold in [
        AggregationFold::Min,
        AggregationFold::Max,
        AggregationFold::Latest,
    ] {
        assert_eq!(
            rollup_eligible(fold, None, &[], &[], aligned()),
            Err(FallbackReason::Fold)
        );
    }
}

#[test]
fn a_metadata_filter_falls_back() {
    let mf = [MetadataFilter::new("region", ["eu"]).expect("valid filter")];
    assert_eq!(
        rollup_eligible(AggregationFold::Sum, None, &mf, &[], aligned()),
        Err(FallbackReason::MetadataFilter)
    );
}

#[test]
fn grouping_by_anything_but_tenant_alone_falls_back() {
    let metadata = AggregationDimension::Metadata(MetadataKey::new("tier").expect("valid key"));
    for group_by in [
        vec![AggregationDimension::ResourceId],
        vec![AggregationDimension::ResourceType],
        vec![AggregationDimension::SubjectId],
        vec![AggregationDimension::SubjectType],
        vec![metadata],
        vec![
            AggregationDimension::TenantId,
            AggregationDimension::ResourceType,
        ],
    ] {
        assert_eq!(
            rollup_eligible(AggregationFold::Sum, None, &[], &group_by, aligned()),
            Err(FallbackReason::GroupBy),
            "{group_by:?}"
        );
    }
}

#[test]
fn a_filter_naming_a_field_outside_the_grain_falls_back() {
    for raw in [
        "origin eq 'live'",
        "entry_type eq 'record'",
        "invalidates eq 11111111-1111-1111-1111-111111111111",
        "resource_id eq 'r1'",
        "subject_id eq 's1'",
        "tenant_id eq 11111111-1111-1111-1111-111111111111 or origin eq 'backfill'",
    ] {
        assert_eq!(
            rollup_eligible(
                AggregationFold::Count,
                Some(&parsed(raw)),
                &[],
                &[],
                aligned()
            ),
            Err(FallbackReason::FilterField),
            "{raw}"
        );
    }
}

#[test]
fn a_range_without_a_whole_hour_falls_back() {
    assert_eq!(
        rollup_eligible(
            AggregationFold::Sum,
            None,
            &[],
            &[],
            range(H0 + 60, H0 + 3_000)
        ),
        Err(FallbackReason::SubHourRange)
    );
}

#[test]
fn every_fallback_reason_has_its_own_label() {
    let labels = [
        FallbackReason::Fold,
        FallbackReason::MetadataFilter,
        FallbackReason::GroupBy,
        FallbackReason::FilterField,
        FallbackReason::SubHourRange,
    ]
    .map(FallbackReason::as_label);
    assert_eq!(
        labels,
        [
            "fold",
            "metadata_filter",
            "group_by",
            "filter_field",
            "sub_hour_range"
        ]
    );
}

// ── build_rollup_aggregate_sql ─────────────────────────────────────────────

const VCPU_METER: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1~";
const TYPE_KEY: &str =
    "r.type_key = (SELECT k.type_key FROM usage_type_key k WHERE k.gts_type_id = $1)";

fn meter() -> MeterTypeId {
    MeterTypeId::new(VCPU_METER).expect("valid meter")
}

fn two_tenants() -> ast::Expr {
    parsed(
        "tenant_id eq 11111111-1111-1111-1111-111111111111 \
         or tenant_id eq 22222222-2222-2222-2222-222222222222",
    )
}

/// `[22:13:20, 03:07:00)`: both edges, whole hours `[23:00, 03:00)`.
fn both_edges() -> HourSplit {
    hour_split(range(H0 + 800, H0 + 5 * HOUR + 420)).expect("whole hours")
}

#[test]
#[allow(clippy::cognitive_complexity)]
fn a_grouped_statement_with_both_edges_and_a_filter_reads_as_the_spec_states() {
    let st = build_rollup_aggregate_sql(
        &meter(),
        both_edges(),
        AggregationFold::Sum,
        Some(&two_tenants()),
        &[AggregationDimension::TenantId],
    )
    .expect("renders");

    assert_eq!(
        st.sql,
        format!(
            "WITH parts AS (\
             SELECT r.tenant_id::text AS d, r.sum_value AS s, r.count_value AS c \
             FROM usage_rollup_1h r \
             WHERE r.gts_type_id = $1 AND {TYPE_KEY} \
             AND r.bucket >= $2 AND r.bucket < $3 \
             AND ((tenant_id = $4 OR tenant_id = $5)) \
             UNION ALL \
             SELECT r.tenant_id::text AS d, \
             SUM(CASE WHEN r.invalidates IS NULL THEN r.value ELSE -r.value END)::numeric AS s, \
             SUM(CASE WHEN r.invalidates IS NULL THEN 1 ELSE -1 END)::bigint AS c \
             FROM usage_records r \
             WHERE r.gts_type_id = $1 AND {TYPE_KEY} \
             AND ((r.window_end >= $6 AND r.window_end < $7) OR (r.window_end >= $8 AND r.window_end < $9)) \
             AND ((tenant_id = $10 OR tenant_id = $11)) \
             GROUP BY 1) \
             SELECT d, SUM(s)::numeric FROM parts GROUP BY 1 HAVING SUM(c) <> 0 LIMIT 100001"
        )
    );
    assert_eq!(st.dim_count, 1);

    let split = both_edges();
    assert_eq!(st.binds.len(), 11);
    assert!(matches!(&st.binds[0], SqlBind::Str(s) if s == VCPU_METER));
    assert!(matches!(st.binds[1], SqlBind::DateTime(t) if t == split.whole_from));
    assert!(matches!(st.binds[2], SqlBind::DateTime(t) if t == split.whole_to));
    assert!(matches!(st.binds[3], SqlBind::Uuid(_)));
    assert!(matches!(st.binds[4], SqlBind::Uuid(_)));
    assert!(matches!(st.binds[5], SqlBind::DateTime(t) if t == split.from));
    assert!(matches!(st.binds[6], SqlBind::DateTime(t) if t == split.whole_from));
    assert!(matches!(st.binds[7], SqlBind::DateTime(t) if t == split.whole_to));
    assert!(matches!(st.binds[8], SqlBind::DateTime(t) if t == split.to));
    assert!(matches!(st.binds[9], SqlBind::Uuid(_)));
    assert!(matches!(st.binds[10], SqlBind::Uuid(_)));
}

#[test]
fn an_aligned_ungrouped_sum_reads_the_rollup_alone_and_nulls_a_netted_selection() {
    let split = hour_split(aligned()).expect("whole hours");
    let st = build_rollup_aggregate_sql(&meter(), split, AggregationFold::Sum, None, &[])
        .expect("renders");
    assert_eq!(
        st.sql,
        format!(
            "WITH parts AS (\
             SELECT r.sum_value AS s, r.count_value AS c \
             FROM usage_rollup_1h r \
             WHERE r.gts_type_id = $1 AND {TYPE_KEY} \
             AND r.bucket >= $2 AND r.bucket < $3) \
             SELECT (CASE WHEN COALESCE(SUM(c), 0) = 0 THEN NULL ELSE SUM(s) END)::numeric FROM parts"
        )
    );
    assert_eq!((st.binds.len(), st.dim_count), (3, 0));
}

#[test]
fn an_ungrouped_count_is_zero_rather_than_null_when_nothing_is_left() {
    let split = hour_split(aligned()).expect("whole hours");
    let st = build_rollup_aggregate_sql(&meter(), split, AggregationFold::Count, None, &[])
        .expect("renders");
    assert!(
        st.sql
            .ends_with("SELECT COALESCE(SUM(c), 0)::numeric FROM parts"),
        "{}",
        st.sql
    );
    assert!(
        !st.sql.contains("HAVING") && !st.sql.contains("LIMIT"),
        "{}",
        st.sql
    );
}

#[test]
fn a_grouped_count_sums_the_counts_and_drops_netted_groups() {
    let split = hour_split(aligned()).expect("whole hours");
    let st = build_rollup_aggregate_sql(
        &meter(),
        split,
        AggregationFold::Count,
        None,
        &[AggregationDimension::TenantId],
    )
    .expect("renders");
    assert!(
        st.sql.ends_with(
            "SELECT d, SUM(c)::numeric FROM parts GROUP BY 1 HAVING SUM(c) <> 0 LIMIT 100001"
        ),
        "{}",
        st.sql
    );
}

#[test]
fn a_single_edge_renders_one_disjunct() {
    let split = hour_split(range(H0, H0 + 2 * HOUR + 1)).expect("whole hours");
    let st = build_rollup_aggregate_sql(&meter(), split, AggregationFold::Sum, None, &[])
        .expect("renders");
    assert!(
        st.sql
            .contains("AND ((r.window_end >= $4 AND r.window_end < $5))"),
        "{}",
        st.sql
    );
    assert_eq!(st.binds.len(), 5);
}

#[test]
fn no_part_of_the_statement_selects_on_window_start() {
    let st = build_rollup_aggregate_sql(
        &meter(),
        both_edges(),
        AggregationFold::Sum,
        Some(&two_tenants()),
        &[AggregationDimension::TenantId],
    )
    .expect("renders");
    assert!(!st.sql.contains("window_start"), "{}", st.sql);
}

#[test]
fn a_fold_the_rollup_cannot_answer_is_refused() {
    let split = hour_split(aligned()).expect("whole hours");
    for fold in [
        AggregationFold::Min,
        AggregationFold::Max,
        AggregationFold::Latest,
    ] {
        assert!(build_rollup_aggregate_sql(&meter(), split, fold, None, &[]).is_err());
    }
}
