#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! `TimescaleDB`-backed integration tests for `PgRecordStore::list` and
//! `PgRecordStore::aggregate`: keyset pagination (first page + cursor follow
//! with no overlap/gap), metadata side-channel filtering, `$filter` by tenant,
//! and pushed-down aggregation (SUM nets compensation, COUNT, GROUP BY
//! resource/metadata, active-only). Requires Docker.

mod common;

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use time::OffsetDateTime;
use uuid::Uuid;

use toolkit_odata::ast::{CompareOperator, Expr, Value};
use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, SortDir};

use usage_collector_sdk::{
    AggregationDimension, AggregationOp, AggregationSpec, MetadataFilter, MetadataKey, UsageRecord,
};

use timescaledb_usage_collector_plugin::domain::ports::{CatalogStore, RecordStore};
use timescaledb_usage_collector_plugin::infra::storage::record_store::PgRecordStore;

const VCPU_GTS: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1";

/// A fixed base instant so each record's `created_at = base + i` is distinct and
/// the `(created_at, uuid)` order is observable across a page.
const BASE_TS: i64 = 1_700_000_000;

/// Bring up a container and register `VCPU_GTS` so the `usage_records.gts_id`
/// FK is satisfied. Returns the harness and a record store.
async fn setup_with_type(gts: &str, fields: &[&str]) -> (common::TsHarness, PgRecordStore) {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let catalog = common::catalog_store(&h.pool);
    catalog
        .create(common::fixture_usage_type(gts, "counter", fields))
        .await
        .expect("register usage type for FK");
    let store = common::record_store(&h.pool);
    (h, store)
}

/// Build a record for `tenant` whose `created_at` is `BASE_TS + i` (so the
/// `(created_at, uuid)` order is strictly increasing across the inserted set),
/// with a distinct uuid/idempotency key derived from `seq`.
fn record_at(gts: &str, tenant: Uuid, seq: u128, i: i64) -> UsageRecord {
    let mut rec = common::fixture_usage_record(
        gts,
        tenant,
        &format!("idem-{seq}"),
        Decimal::new(i + 1, 0),
        seq,
    );
    rec.created_at =
        OffsetDateTime::from_unix_timestamp(BASE_TS + i).expect("valid created_at instant");
    rec
}

/// The gateway-default record order: `(created_at asc, uuid asc)`.
fn created_at_uuid_asc() -> ODataOrderBy {
    ODataOrderBy(vec![
        OrderKey {
            field: "created_at".to_owned(),
            dir: SortDir::Asc,
        },
        OrderKey {
            field: "uuid".to_owned(),
            dir: SortDir::Asc,
        },
    ])
}

/// The descending counterpart `(created_at desc, uuid desc)`. The plugin's
/// keyset translation supports any uniform-direction order (DESC emits the
/// `<` seek predicate); this exercises that path end-to-end against real rows.
fn created_at_uuid_desc() -> ODataOrderBy {
    ODataOrderBy(vec![
        OrderKey {
            field: "created_at".to_owned(),
            dir: SortDir::Desc,
        },
        OrderKey {
            field: "uuid".to_owned(),
            dir: SortDir::Desc,
        },
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_list_first_page_returns_limit_and_next_cursor() {
    let (_h, store) = setup_with_type(VCPU_GTS, &[]).await;
    let tenant = Uuid::from_u128(0x2001);

    for i in 0..5 {
        let seq = 0x2001_0000 + u128::try_from(i).unwrap();
        store
            .create(record_at(VCPU_GTS, tenant, seq, i))
            .await
            .expect("create record");
    }

    let query = ODataQuery::new()
        .with_limit(2)
        .with_order(created_at_uuid_asc());

    let page = store
        .list(common::fixture_gts_id(VCPU_GTS), &query, &[])
        .await
        .expect("list first page");

    assert_eq!(page.items.len(), 2, "first page is capped at the limit");
    assert!(
        page.page_info.next_cursor.is_some(),
        "a 5-record set over limit 2 must yield a next cursor"
    );
    assert_eq!(page.page_info.limit, 2, "page echoes the request limit");
    // Ascending by created_at: first two are the earliest two instants.
    assert!(
        page.items[0].created_at < page.items[1].created_at,
        "page items are in ascending created_at order"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_list_following_cursor_has_no_overlap_or_gap() {
    let (_h, store) = setup_with_type(VCPU_GTS, &[]).await;
    let tenant = Uuid::from_u128(0x2002);

    let mut expected: Vec<Uuid> = Vec::new();
    for i in 0..5 {
        let seq = 0x2002_0000 + u128::try_from(i).unwrap();
        let rec = record_at(VCPU_GTS, tenant, seq, i);
        expected.push(rec.uuid);
        store.create(rec).await.expect("create record");
    }
    // The gts_id is shared, so iteration order is the SQL order (created_at,uuid).
    expected.sort();

    let order = created_at_uuid_asc();
    let mut seen: Vec<Uuid> = Vec::new();
    let mut cursor: Option<CursorV1> = None;

    // Walk every page (limit 2) following the cursor each time.
    loop {
        let mut query = ODataQuery::new().with_limit(2).with_order(order.clone());
        if let Some(c) = cursor.take() {
            query = query.with_cursor(c);
        }
        let page = store
            .list(common::fixture_gts_id(VCPU_GTS), &query, &[])
            .await
            .expect("list page");

        for item in &page.items {
            assert!(
                !seen.contains(&item.uuid),
                "no record appears on two pages (overlap)"
            );
            seen.push(item.uuid);
        }

        match page.page_info.next_cursor {
            Some(token) => {
                cursor = Some(CursorV1::decode(&token).expect("decode next cursor"));
            }
            None => break,
        }
    }

    let mut seen_sorted = seen.clone();
    seen_sorted.sort();
    assert_eq!(
        seen_sorted, expected,
        "walking all pages yields every record exactly once (no gap, no overlap)"
    );
    assert_eq!(seen.len(), 5, "exactly the five inserted records");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_list_descending_cursor_walk_is_ordered_with_no_overlap_or_gap() {
    let (_h, store) = setup_with_type(VCPU_GTS, &[]).await;
    let tenant = Uuid::from_u128(0x200D);

    let mut expected: Vec<Uuid> = Vec::new();
    for i in 0..5 {
        let seq = 0x200D_0000 + u128::try_from(i).unwrap();
        let rec = record_at(VCPU_GTS, tenant, seq, i);
        expected.push(rec.uuid);
        store.create(rec).await.expect("create record");
    }
    expected.sort();

    let order = created_at_uuid_desc();
    let mut seen: Vec<Uuid> = Vec::new();
    let mut prev_created_at: Option<OffsetDateTime> = None;
    let mut cursor: Option<CursorV1> = None;

    // Walk every page (limit 2) following the DESC cursor each time.
    loop {
        let mut query = ODataQuery::new().with_limit(2).with_order(order.clone());
        if let Some(c) = cursor.take() {
            query = query.with_cursor(c);
        }
        let page = store
            .list(common::fixture_gts_id(VCPU_GTS), &query, &[])
            .await
            .expect("list page (desc)");

        for item in &page.items {
            // Strictly descending by created_at across the whole walk: the DESC
            // seek predicate must never revisit or skip the order boundary.
            if let Some(prev) = prev_created_at {
                assert!(
                    item.created_at < prev,
                    "rows are strictly descending by created_at across pages"
                );
            }
            prev_created_at = Some(item.created_at);
            assert!(
                !seen.contains(&item.uuid),
                "no record appears on two pages (overlap)"
            );
            seen.push(item.uuid);
        }

        match page.page_info.next_cursor {
            Some(token) => {
                cursor = Some(CursorV1::decode(&token).expect("decode next cursor"));
            }
            None => break,
        }
    }

    let mut seen_sorted = seen.clone();
    seen_sorted.sort();
    assert_eq!(
        seen_sorted, expected,
        "walking all pages (desc) yields every record exactly once (no gap, no overlap)"
    );
    assert_eq!(seen.len(), 5, "exactly the five inserted records");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_list_metadata_filter_narrows_results() {
    // Register a type declaring the `region` metadata field.
    let (_h, store) = setup_with_type(VCPU_GTS, &["region"]).await;
    let tenant = Uuid::from_u128(0x2003);

    // 3 records: two in us-east-1, one in eu-west-1.
    let regions = ["us-east-1", "us-east-1", "eu-west-1"];
    for (i, region) in regions.iter().enumerate() {
        let idx = i64::try_from(i).unwrap();
        let seq = 0x2003_0000 + u128::try_from(i).unwrap();
        let mut rec = record_at(VCPU_GTS, tenant, seq, idx);
        let mut meta = BTreeMap::new();
        meta.insert(
            MetadataKey::new("region").expect("valid metadata key"),
            (*region).to_owned(),
        );
        rec.metadata = meta;
        store.create(rec).await.expect("create record");
    }

    let filter = MetadataFilter::new("region", ["us-east-1"]).expect("valid metadata filter");
    let query = ODataQuery::new().with_order(created_at_uuid_asc());

    let page = store
        .list(
            common::fixture_gts_id(VCPU_GTS),
            &query,
            std::slice::from_ref(&filter),
        )
        .await
        .expect("list with metadata filter");

    assert_eq!(
        page.items.len(),
        2,
        "only the two us-east-1 records match the metadata filter"
    );
    for item in &page.items {
        assert_eq!(
            item.metadata.get(&MetadataKey::new("region").unwrap()),
            Some(&"us-east-1".to_owned()),
            "every returned record carries the filtered metadata value"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_list_filter_by_tenant() {
    let (_h, store) = setup_with_type(VCPU_GTS, &[]).await;
    let tenant_a = Uuid::from_u128(0x2004_000A);
    let tenant_b = Uuid::from_u128(0x2004_000B);

    // Two records for tenant A, one for tenant B.
    store
        .create(record_at(VCPU_GTS, tenant_a, 0x2004_0001, 0))
        .await
        .expect("create A1");
    store
        .create(record_at(VCPU_GTS, tenant_a, 0x2004_0002, 1))
        .await
        .expect("create A2");
    store
        .create(record_at(VCPU_GTS, tenant_b, 0x2004_0003, 2))
        .await
        .expect("create B1");

    // Build `tenant_id eq <tenant_a>` directly as an AST (the Uuid value type
    // matches the `tenant_id` filter field's declared `kind = "Uuid"`).
    let filter = Expr::Compare(
        Box::new(Expr::Identifier("tenant_id".to_owned())),
        CompareOperator::Eq,
        Box::new(Expr::Value(Value::Uuid(tenant_a))),
    );
    let query = ODataQuery::new()
        .with_order(created_at_uuid_asc())
        .with_filter(filter);

    let page = store
        .list(common::fixture_gts_id(VCPU_GTS), &query, &[])
        .await
        .expect("list filtered by tenant");

    assert_eq!(page.items.len(), 2, "only tenant A's two records match");
    for item in &page.items {
        assert_eq!(
            item.tenant_id, tenant_a,
            "every returned record is tenant A"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_aggregate_sum_nets_compensation() {
    let (_h, store) = setup_with_type(VCPU_GTS, &[]).await;
    let tenant = Uuid::from_u128(0x3001);

    // Original +10 row, then an active compensation of -3 that corrects it.
    let mut original = record_at(VCPU_GTS, tenant, 0x3001_0001, 0);
    original.value = Decimal::new(10, 0);
    let original_uuid = original.uuid;
    store.create(original).await.expect("create original");

    let mut compensation = record_at(VCPU_GTS, tenant, 0x3001_0002, 1);
    compensation.value = Decimal::new(-3, 0);
    compensation.corrects_id = Some(original_uuid);
    store
        .create(compensation)
        .await
        .expect("create compensation");

    let spec = AggregationSpec {
        op: AggregationOp::Sum,
        group_by: Vec::new(),
    };
    let result = store
        .aggregate(
            common::fixture_gts_id(VCPU_GTS),
            &ODataQuery::new(),
            &[],
            spec,
        )
        .await
        .expect("aggregate sum");

    assert_eq!(
        result.buckets.len(),
        1,
        "empty group_by yields exactly one bucket"
    );
    let bucket = &result.buckets[0];
    assert!(bucket.key.is_empty(), "no grouping -> empty bucket key");
    assert_eq!(
        bucket.value,
        Some(Decimal::new(7, 0)),
        "SUM nets the active compensation: 10 + (-3) = 7"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_aggregate_count_counts_active_rows() {
    let (_h, store) = setup_with_type(VCPU_GTS, &[]).await;
    let tenant = Uuid::from_u128(0x3002);

    for i in 0..3 {
        let seq = 0x3002_0000 + u128::try_from(i).unwrap();
        store
            .create(record_at(VCPU_GTS, tenant, seq, i))
            .await
            .expect("create record");
    }

    let spec = AggregationSpec {
        op: AggregationOp::Count,
        group_by: Vec::new(),
    };
    let result = store
        .aggregate(
            common::fixture_gts_id(VCPU_GTS),
            &ODataQuery::new(),
            &[],
            spec,
        )
        .await
        .expect("aggregate count");

    assert_eq!(result.buckets.len(), 1, "empty group_by -> one bucket");
    assert!(result.buckets[0].key.is_empty(), "no grouping -> empty key");
    assert_eq!(
        result.buckets[0].value,
        Some(Decimal::new(3, 0)),
        "COUNT(*)::numeric reads back as 3 over three active rows"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_aggregate_group_by_resource_id() {
    let (_h, store) = setup_with_type(VCPU_GTS, &[]).await;
    let tenant = Uuid::from_u128(0x3003);

    // res-a: 4 + 6 = 10; res-b: 5. Distinct created_at per row so the
    // (…, created_at) unique tuple never collides.
    let rows = [
        ("idem-3003-1", 4_i64, 0x3003_0001_u128, "res-a", 0_i64),
        ("idem-3003-2", 6, 0x3003_0002, "res-a", 1),
        ("idem-3003-3", 5, 0x3003_0003, "res-b", 2),
    ];
    for (idem, value, seq, resource_id, ts) in rows {
        let mut rec = common::fixture_usage_record_with_resource(
            VCPU_GTS,
            tenant,
            idem,
            Decimal::new(value, 0),
            seq,
            resource_id,
        );
        rec.created_at = OffsetDateTime::from_unix_timestamp(BASE_TS + ts).unwrap();
        store.create(rec).await.expect("create record");
    }

    let spec = AggregationSpec {
        op: AggregationOp::Sum,
        group_by: vec![AggregationDimension::ResourceId],
    };
    let result = store
        .aggregate(
            common::fixture_gts_id(VCPU_GTS),
            &ODataQuery::new(),
            &[],
            spec,
        )
        .await
        .expect("aggregate group by resource_id");

    assert_eq!(
        result.buckets.len(),
        2,
        "one bucket per distinct resource_id"
    );
    let mut got: Vec<(String, Option<Decimal>)> = result
        .buckets
        .iter()
        .map(|b| {
            assert_eq!(b.key.len(), 1, "single grouped dimension -> one key entry");
            (b.key[0].clone(), b.value)
        })
        .collect();
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        got,
        vec![
            ("res-a".to_owned(), Some(Decimal::new(10, 0))),
            ("res-b".to_owned(), Some(Decimal::new(5, 0))),
        ],
        "each resource_id bucket carries its summed value"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_aggregate_group_by_metadata() {
    // Register a type declaring the `region` metadata field.
    let (_h, store) = setup_with_type(VCPU_GTS, &["region"]).await;
    let tenant = Uuid::from_u128(0x3004);

    // us-east-1: 2 + 3 = 5; eu-west-1: 7.
    let rows = [
        ("us-east-1", 2_i64, 0_i64),
        ("us-east-1", 3, 1),
        ("eu-west-1", 7, 2),
    ];
    for (i, (region, value, ts)) in rows.iter().enumerate() {
        let seq = 0x3004_0000 + u128::try_from(i).unwrap();
        let mut rec = record_at(VCPU_GTS, tenant, seq, *ts);
        rec.value = Decimal::new(*value, 0);
        let mut meta = BTreeMap::new();
        meta.insert(
            MetadataKey::new("region").expect("valid metadata key"),
            (*region).to_owned(),
        );
        rec.metadata = meta;
        store.create(rec).await.expect("create record");
    }

    let spec = AggregationSpec {
        op: AggregationOp::Sum,
        group_by: vec![AggregationDimension::Metadata(
            MetadataKey::new("region").expect("valid metadata key"),
        )],
    };
    let result = store
        .aggregate(
            common::fixture_gts_id(VCPU_GTS),
            &ODataQuery::new(),
            &[],
            spec,
        )
        .await
        .expect("aggregate group by metadata");

    assert_eq!(result.buckets.len(), 2, "one bucket per distinct region");
    let mut got: Vec<(String, Option<Decimal>)> = result
        .buckets
        .iter()
        .map(|b| {
            assert_eq!(b.key.len(), 1, "single grouped dimension -> one key entry");
            (b.key[0].clone(), b.value)
        })
        .collect();
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        got,
        vec![
            ("eu-west-1".to_owned(), Some(Decimal::new(7, 0))),
            ("us-east-1".to_owned(), Some(Decimal::new(5, 0))),
        ],
        "each region bucket carries its summed value"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_aggregate_min_max_avg_over_active_rows() {
    let (_h, store) = setup_with_type(VCPU_GTS, &[]).await;
    let tenant = Uuid::from_u128(0x3008);

    // Values {2, 8}: min 2, max 8, avg 5 -- exact, so no fractional precision.
    for (i, v) in [2_i64, 8].into_iter().enumerate() {
        let seq = 0x3008_0000 + u128::try_from(i).unwrap();
        let mut rec = record_at(VCPU_GTS, tenant, seq, i64::try_from(i).unwrap());
        rec.value = Decimal::new(v, 0);
        store.create(rec).await.expect("create record");
    }

    for (op, expected) in [
        (AggregationOp::Min, Decimal::new(2, 0)),
        (AggregationOp::Max, Decimal::new(8, 0)),
        (AggregationOp::Avg, Decimal::new(5, 0)),
    ] {
        let spec = AggregationSpec {
            op,
            group_by: Vec::new(),
        };
        let result = store
            .aggregate(
                common::fixture_gts_id(VCPU_GTS),
                &ODataQuery::new(),
                &[],
                spec,
            )
            .await
            .unwrap_or_else(|e| panic!("aggregate {op:?}: {e:?}"));

        assert_eq!(
            result.buckets.len(),
            1,
            "{op:?}: empty group_by -> one bucket"
        );
        // normalize() strips trailing-zero scale so AVG's numeric `5.0000...`
        // compares equal to `5`.
        assert_eq!(
            result.buckets[0].value.map(|v| v.normalize()),
            Some(expected.normalize()),
            "{op:?} over {{2, 8}}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_aggregate_excludes_inactive() {
    let (_h, store) = setup_with_type(VCPU_GTS, &[]).await;
    let tenant = Uuid::from_u128(0x3005);

    // Two active rows summing 10 + 5; deactivate the 10 row.
    let mut keep = record_at(VCPU_GTS, tenant, 0x3005_0001, 0);
    keep.value = Decimal::new(5, 0);
    let mut drop_row = record_at(VCPU_GTS, tenant, 0x3005_0002, 1);
    drop_row.value = Decimal::new(10, 0);
    let drop_uuid = drop_row.uuid;
    store.create(keep).await.expect("create keep");
    store.create(drop_row).await.expect("create drop");
    store.deactivate(drop_uuid).await.expect("deactivate drop");

    let spec = AggregationSpec {
        op: AggregationOp::Sum,
        group_by: Vec::new(),
    };
    let result = store
        .aggregate(
            common::fixture_gts_id(VCPU_GTS),
            &ODataQuery::new(),
            &[],
            spec,
        )
        .await
        .expect("aggregate excludes inactive");

    assert_eq!(result.buckets.len(), 1, "empty group_by -> one bucket");
    assert_eq!(
        result.buckets[0].value,
        Some(Decimal::new(5, 0)),
        "SUM counts only the active row (5); the deactivated 10 is excluded"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_subject_ref_round_trips_through_create_and_get() {
    let (_h, store) = setup_with_type(VCPU_GTS, &[]).await;
    let tenant = Uuid::from_u128(0x3006);

    let rec = common::fixture_usage_record_with_subject(
        VCPU_GTS,
        tenant,
        "idem-3006-1",
        Decimal::new(1, 0),
        0x3006_0001,
        "subj-1",
        Some("user"),
    );
    let uuid = rec.uuid;
    store.create(rec).await.expect("create with subject");

    let got = store
        .get(uuid)
        .await
        .expect("get the subject-bearing record");
    let subject = got.subject_ref.expect("subject_ref must round-trip");
    assert_eq!(subject.subject_id(), "subj-1", "subject_id round-trips");
    assert_eq!(
        subject.subject_type(),
        Some("user"),
        "subject_type round-trips"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_aggregate_group_by_subject_id_excludes_subjectless() {
    let (_h, store) = setup_with_type(VCPU_GTS, &[]).await;
    let tenant = Uuid::from_u128(0x3007);

    // subj-a: 4 + 6 = 10; subj-b: 5; plus one subject-less row (7) that must be
    // excluded from a group-by-subject aggregation per the SDK contract.
    let subject_rows = [
        ("idem-3007-1", 4_i64, 0x3007_0001_u128, "subj-a", 0_i64),
        ("idem-3007-2", 6, 0x3007_0002, "subj-a", 1),
        ("idem-3007-3", 5, 0x3007_0003, "subj-b", 2),
    ];
    for (idem, value, seq, subject_id, ts) in subject_rows {
        let mut rec = common::fixture_usage_record_with_subject(
            VCPU_GTS,
            tenant,
            idem,
            Decimal::new(value, 0),
            seq,
            subject_id,
            None,
        );
        rec.created_at =
            OffsetDateTime::from_unix_timestamp(BASE_TS + ts).expect("valid created_at instant");
        store.create(rec).await.expect("create subject row");
    }
    // A subject-less row (the IS NOT NULL guard must exclude it from grouping).
    let mut subjectless = record_at(VCPU_GTS, tenant, 0x3007_0004, 3);
    subjectless.value = Decimal::new(7, 0);
    store
        .create(subjectless)
        .await
        .expect("create subjectless row");

    let spec = AggregationSpec {
        op: AggregationOp::Sum,
        group_by: vec![AggregationDimension::SubjectId],
    };
    let result = store
        .aggregate(
            common::fixture_gts_id(VCPU_GTS),
            &ODataQuery::new(),
            &[],
            spec,
        )
        .await
        .expect("aggregate group by subject_id");

    assert_eq!(
        result.buckets.len(),
        2,
        "subject-less row is excluded -> two subject buckets"
    );
    let mut got: Vec<(String, Option<Decimal>)> = result
        .buckets
        .iter()
        .map(|b| {
            assert_eq!(b.key.len(), 1, "single grouped dimension -> one key entry");
            (b.key[0].clone(), b.value)
        })
        .collect();
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        got,
        vec![
            ("subj-a".to_owned(), Some(Decimal::new(10, 0))),
            ("subj-b".to_owned(), Some(Decimal::new(5, 0))),
        ],
        "each subject_id bucket carries its summed value"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_aggregate_filter_by_tenant_isolates_sum() {
    // The aggregate path builds its WHERE clause independently of `list`; this
    // pins that the PDP-injected `tenant_id eq …` `$filter` actually scopes the
    // aggregation, so a regression that dropped the filter (summing across all
    // tenants) is caught.
    let (_h, store) = setup_with_type(VCPU_GTS, &[]).await;
    let tenant_a = Uuid::from_u128(0x3009_000A);
    let tenant_b = Uuid::from_u128(0x3009_000B);

    // Tenant A: 4 + 6 = 10. Tenant B: 100 (must be excluded by the filter).
    let mut a1 = record_at(VCPU_GTS, tenant_a, 0x3009_0001, 0);
    a1.value = Decimal::new(4, 0);
    let mut a2 = record_at(VCPU_GTS, tenant_a, 0x3009_0002, 1);
    a2.value = Decimal::new(6, 0);
    let mut b1 = record_at(VCPU_GTS, tenant_b, 0x3009_0003, 2);
    b1.value = Decimal::new(100, 0);
    store.create(a1).await.expect("create A1");
    store.create(a2).await.expect("create A2");
    store.create(b1).await.expect("create B1");

    let filter = Expr::Compare(
        Box::new(Expr::Identifier("tenant_id".to_owned())),
        CompareOperator::Eq,
        Box::new(Expr::Value(Value::Uuid(tenant_a))),
    );
    let query = ODataQuery::new().with_filter(filter);

    let spec = AggregationSpec {
        op: AggregationOp::Sum,
        group_by: Vec::new(),
    };
    let result = store
        .aggregate(common::fixture_gts_id(VCPU_GTS), &query, &[], spec)
        .await
        .expect("aggregate sum filtered by tenant");

    assert_eq!(result.buckets.len(), 1, "empty group_by -> one bucket");
    assert_eq!(
        result.buckets[0].value,
        Some(Decimal::new(10, 0)),
        "SUM includes only tenant A's rows (4 + 6); tenant B's 100 is excluded by the $filter"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_aggregate_metadata_filter_narrows_sum() {
    // The metadata side-channel filter is shared by `list` and `aggregate` but
    // only `list` exercised it with a real filter; this covers the real billing
    // query shape ("sum where region = us-east-1") through the aggregate path.
    let (_h, store) = setup_with_type(VCPU_GTS, &["region"]).await;
    let tenant = Uuid::from_u128(0x300A);

    // us-east-1: 2 + 3 = 5; eu-west-1: 7 (must be excluded by the metadata filter).
    let rows = [
        ("us-east-1", 2_i64, 0_i64),
        ("us-east-1", 3, 1),
        ("eu-west-1", 7, 2),
    ];
    for (i, (region, value, ts)) in rows.iter().enumerate() {
        let seq = 0x300A_0000 + u128::try_from(i).unwrap();
        let mut rec = record_at(VCPU_GTS, tenant, seq, *ts);
        rec.value = Decimal::new(*value, 0);
        let mut meta = BTreeMap::new();
        meta.insert(
            MetadataKey::new("region").expect("valid metadata key"),
            (*region).to_owned(),
        );
        rec.metadata = meta;
        store.create(rec).await.expect("create record");
    }

    let filter = MetadataFilter::new("region", ["us-east-1"]).expect("valid metadata filter");
    let spec = AggregationSpec {
        op: AggregationOp::Sum,
        group_by: Vec::new(),
    };
    let result = store
        .aggregate(
            common::fixture_gts_id(VCPU_GTS),
            &ODataQuery::new(),
            std::slice::from_ref(&filter),
            spec,
        )
        .await
        .expect("aggregate sum with metadata filter");

    assert_eq!(result.buckets.len(), 1, "empty group_by -> one bucket");
    assert_eq!(
        result.buckets[0].value,
        Some(Decimal::new(5, 0)),
        "SUM includes only the us-east-1 rows (2 + 3); eu-west-1's 7 is excluded by the metadata filter"
    );
}
