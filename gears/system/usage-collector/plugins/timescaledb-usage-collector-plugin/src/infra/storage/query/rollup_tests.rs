#![allow(clippy::panic)]

use time::OffsetDateTime;
use toolkit_odata::ast;
use usage_collector_sdk::{
    AggregationDimension, AggregationFold, MetadataFilter, MetadataKey, TimeRange,
};

use super::{FallbackReason, HourSplit, hour_split, rollup_eligible};

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
