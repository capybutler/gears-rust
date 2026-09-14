#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! The rollup read path against a live `TimescaleDB`, compared with the exact
//! scan over the same database. Requires Docker.
//!
//! The equivalence test is what holds the rollup to the contract's rules: the
//! SDK contract checks use sub-hour ranges, which always take the scan.

mod common;

use bigdecimal::BigDecimal;
use rust_decimal::Decimal;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use toolkit_odata::ODataQuery;
use toolkit_odata::ast::{CompareOperator, Expr, Value};
use usage_collector_sdk::{
    AggregationDimension, AggregationFold, AggregationResult, RecordOrigin, TimeRange, UsageRecord,
};

use timescaledb_usage_collector_plugin::domain::ports::RecordStore;
use timescaledb_usage_collector_plugin::infra::storage::pool::apply_post_migration_setup;
use timescaledb_usage_collector_plugin::infra::storage::record_store::PgRecordStore;
use timescaledb_usage_collector_plugin::infra::storage::rollup_maintenance::refresh_job_statuses;

const TENANT_A: Uuid = Uuid::from_u128(0xA0);
const TENANT_B: Uuid = Uuid::from_u128(0xB0);
/// Every entry of this tenant is withdrawn.
const TENANT_C: Uuid = Uuid::from_u128(0xC0);

/// `2023-11-14T22:00:00Z`.
fn h0() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_699_999_200).expect("valid instant")
}

fn tenant_eq(t: Uuid) -> Expr {
    Expr::Compare(
        Box::new(Expr::Identifier("tenant_id".to_owned())),
        CompareOperator::Eq,
        Box::new(Expr::Value(Value::Uuid(t))),
    )
}

fn origin_eq(origin: &str) -> Expr {
    Expr::Compare(
        Box::new(Expr::Identifier("origin".to_owned())),
        CompareOperator::Eq,
        Box::new(Expr::Value(Value::String(origin.to_owned()))),
    )
}

fn range(from: OffsetDateTime, to: OffsetDateTime) -> TimeRange {
    TimeRange::new(from, to).expect("ordered range")
}

/// Buckets as sorted `(key, normalized value)` pairs, so two results compare
/// as sets whatever order `PostgreSQL` emitted them in.
fn canonical(result: &AggregationResult) -> Vec<(Vec<String>, Option<BigDecimal>)> {
    let mut rows: Vec<_> = result
        .buckets
        .iter()
        .map(|b| (b.key.clone(), b.value.as_ref().map(BigDecimal::normalized)))
        .collect();
    rows.sort();
    rows
}

struct Stores {
    h: common::TsHarness,
    rollup: PgRecordStore,
    scan: PgRecordStore,
}

async fn stores() -> Stores {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let rollup = common::record_store(&h.pool);
    let scan = common::record_store(&h.pool).without_rollup();
    Stores { h, rollup, scan }
}

/// Deterministic fixture: two meters, three tenants, 30 hours of entries at
/// minute offsets that include exact hour boundaries and point events,
/// negative quantities, withdrawals (one of them on the backfill path of a live
/// record), and a tenant whose every entry is withdrawn.
async fn seed(store: &PgRecordStore) {
    let meters = [
        common::meter(common::VCPU_METER),
        common::meter(common::GB_METER),
    ];
    let mut n = 0_u32;
    for meter in &meters {
        for tenant in [TENANT_A, TENANT_B, TENANT_C] {
            for hour in 0_i64..30 {
                for minute in [0_i64, 17, 59] {
                    n += 1;
                    let end = h0() + Duration::hours(hour) + Duration::minutes(minute);
                    let start = if minute == 17 {
                        end
                    } else {
                        end - Duration::minutes(45)
                    };
                    let value = Decimal::from(i64::from(n % 7) - 2);
                    let record =
                        common::entry_over(meter, tenant, &format!("e-{n}"), value, start, end);
                    let withdraw = tenant == TENANT_C || n.is_multiple_of(5);
                    store.create(record.clone()).await.expect("seed record");
                    if withdraw {
                        let mut w = common::withdrawal_of(&record, &format!("w-{n}"));
                        if n.is_multiple_of(10) {
                            w.origin = RecordOrigin::Backfill;
                        }
                        store.create(w).await.expect("seed withdrawal");
                    }
                }
            }
        }
    }
}

fn ranges() -> Vec<TimeRange> {
    vec![
        range(h0(), h0() + Duration::hours(24)),
        range(
            h0() + Duration::minutes(13),
            h0() + Duration::hours(5) + Duration::minutes(7),
        ),
        range(h0() + Duration::minutes(10), h0() + Duration::minutes(50)),
        range(h0() - Duration::hours(1), h0() + Duration::hours(40)),
        range(h0() + Duration::hours(1), h0() + Duration::hours(2)),
        range(
            h0() + Duration::hours(3) + Duration::minutes(17),
            h0() + Duration::hours(7) + Duration::minutes(59),
        ),
    ]
}

/// Every (range, fold, grouping, filter) on both stores, asserting equality.
async fn assert_paths_agree(s: &Stores, when: &str) {
    let filters: Vec<Option<Expr>> = vec![
        None,
        Some(tenant_eq(TENANT_A)),
        Some(Expr::Or(
            Box::new(tenant_eq(TENANT_A)),
            Box::new(tenant_eq(TENANT_C)),
        )),
        Some(tenant_eq(TENANT_C)),
    ];
    for meter in [common::VCPU_METER, common::GB_METER] {
        for r in ranges() {
            for fold in [AggregationFold::Sum, AggregationFold::Count] {
                for group_by in [vec![], vec![AggregationDimension::TenantId]] {
                    for f in &filters {
                        let q = f
                            .clone()
                            .map_or_else(ODataQuery::new, |e| ODataQuery::new().with_filter(e));
                        let got = s
                            .rollup
                            .aggregate(common::meter(meter), r, fold, &q, &[], &group_by)
                            .await
                            .expect("rollup");
                        let want = s
                            .scan
                            .aggregate(common::meter(meter), r, fold, &q, &[], &group_by)
                            .await
                            .expect("scan");
                        assert_eq!(
                            canonical(&got),
                            canonical(&want),
                            "{when}: {meter} {r:?} {fold:?} {group_by:?} {f:?}"
                        );
                    }
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_rollup_path_equals_the_scan_before_and_after_a_refresh() {
    let s = stores().await;
    seed(&s.rollup).await;
    assert_paths_agree(&s, "before refresh (real-time half only)").await;
    common::refresh_rollup(&s.h.pool).await;
    assert_paths_agree(&s, "after refresh").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fully_withdrawn_tenant_has_no_group_and_folds_to_null_sum_and_zero_count() {
    let s = stores().await;
    seed(&s.rollup).await;
    common::refresh_rollup(&s.h.pool).await;
    let meter = common::meter(common::VCPU_METER);
    let day = range(h0(), h0() + Duration::hours(24));

    let grouped = s
        .rollup
        .aggregate(
            meter.clone(),
            day,
            AggregationFold::Sum,
            &ODataQuery::new(),
            &[],
            &[AggregationDimension::TenantId],
        )
        .await
        .expect("grouped");
    assert!(
        grouped
            .buckets
            .iter()
            .all(|b| b.key != vec![TENANT_C.to_string()]),
        "{grouped:?}"
    );

    let only_c = ODataQuery::new().with_filter(tenant_eq(TENANT_C));
    let sum = s
        .rollup
        .aggregate(meter.clone(), day, AggregationFold::Sum, &only_c, &[], &[])
        .await
        .expect("sum");
    assert_eq!(sum.buckets.len(), 1);
    assert_eq!(sum.buckets[0].value, None, "a SUM over nothing is null");
    let count = s
        .rollup
        .aggregate(meter, day, AggregationFold::Count, &only_c, &[], &[])
        .await
        .expect("count");
    assert_eq!(
        count.buckets[0].value.as_ref().map(BigDecimal::normalized),
        Some(BigDecimal::from(0).normalized())
    );
}

/// Routing by behaviour: after a refresh, a late write below the watermark is
/// invisible to an eligible query until the next refresh, and visible at once
/// to the same query with an `origin` filter, which takes the scan.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_write_is_stale_on_the_rollup_until_refreshed_and_fresh_on_the_scan() {
    let s = stores().await;
    seed(&s.rollup).await;
    common::refresh_rollup(&s.h.pool).await;
    let meter = common::meter(common::VCPU_METER);
    let day = range(h0(), h0() + Duration::hours(24));
    let a = ODataQuery::new().with_filter(tenant_eq(TENANT_A));
    let a_live = ODataQuery::new().with_filter(Expr::And(
        Box::new(tenant_eq(TENANT_A)),
        Box::new(origin_eq("live")),
    ));
    let sum = |r: &AggregationResult| r.buckets[0].value.clone().map(|v| v.normalized());

    let before = sum(&s
        .rollup
        .aggregate(meter.clone(), day, AggregationFold::Sum, &a, &[], &[])
        .await
        .unwrap());
    let before_live = sum(&s
        .rollup
        .aggregate(meter.clone(), day, AggregationFold::Sum, &a_live, &[], &[])
        .await
        .unwrap());

    let late_end = h0() + Duration::hours(6) + Duration::minutes(30);
    let late: UsageRecord = common::entry_over(
        &meter,
        TENANT_A,
        "late",
        Decimal::from(1000),
        late_end - Duration::minutes(5),
        late_end,
    );
    s.rollup.create(late).await.expect("late write");

    let stale = sum(&s
        .rollup
        .aggregate(meter.clone(), day, AggregationFold::Sum, &a, &[], &[])
        .await
        .unwrap());
    assert_eq!(
        stale, before,
        "the rollup path does not see a write below its watermark before a refresh"
    );
    let fresh_live = sum(&s
        .rollup
        .aggregate(meter.clone(), day, AggregationFold::Sum, &a_live, &[], &[])
        .await
        .unwrap());
    assert_eq!(
        fresh_live,
        before_live.map(|v| (v + BigDecimal::from(1000)).normalized()),
        "an origin filter takes the scan, which sees the write at once"
    );

    common::refresh_rollup(&s.h.pool).await;
    let refreshed = sum(&s
        .rollup
        .aggregate(meter, day, AggregationFold::Sum, &a, &[], &[])
        .await
        .unwrap());
    assert_eq!(
        refreshed,
        before.map(|v| (v + BigDecimal::from(1000)).normalized())
    );
}

/// A write inside the materialisation lag is served by the view's real-time
/// half without any refresh.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recent_write_is_visible_on_the_rollup_path_without_a_refresh() {
    let s = stores().await;
    seed(&s.rollup).await;
    common::refresh_rollup(&s.h.pool).await;
    let meter = common::meter(common::VCPU_METER);
    let end = OffsetDateTime::now_utc() - Duration::minutes(10);
    s.rollup
        .create(common::entry_over(
            &meter,
            TENANT_B,
            "recent",
            Decimal::from(42),
            end - Duration::minutes(5),
            end,
        ))
        .await
        .expect("recent write");
    let recent = range(end - Duration::hours(3), end + Duration::hours(2));
    let q = ODataQuery::new().with_filter(tenant_eq(TENANT_B));
    let got = s
        .rollup
        .aggregate(meter, recent, AggregationFold::Sum, &q, &[], &[])
        .await
        .expect("aggregate");
    assert_eq!(
        got.buckets[0].value.as_ref().map(BigDecimal::normalized),
        Some(BigDecimal::from(42).normalized())
    );
}

/// Re-applied policies run on creation, and the status query reads both back
/// with a success age once they have.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_refresh_policies_report_a_success_age_after_their_first_run() {
    let s = stores().await;
    apply_post_migration_setup(&s.h.pool, &s.h.cfg)
        .await
        .expect("re-apply policies");

    let mut statuses = Vec::new();
    for _ in 0..60 {
        statuses = refresh_job_statuses(&s.h.pool).await.expect("status query");
        if statuses.len() == 2 && statuses.iter().all(|st| st.secs_since_success.is_some()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let mut policies: Vec<_> = statuses.iter().map(|st| st.policy.as_label()).collect();
    policies.sort_unstable();
    assert_eq!(policies, ["history", "live"], "{statuses:?}");
    assert!(
        statuses
            .iter()
            .all(|st| !st.failing && st.secs_since_success.is_some()),
        "{statuses:?}"
    );
}
