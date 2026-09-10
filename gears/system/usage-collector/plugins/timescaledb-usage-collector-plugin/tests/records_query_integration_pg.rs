#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! `PgRecordStore::list`, `::get` and `::aggregate` against a live
//! `TimescaleDB`. Requires Docker.
//!
//! The behavioural home for the read paths, and for the obligations the DESIGN
//! section 3.3 contract suite structurally cannot reach: the keyset ones. The
//! reference backend that suite was validated against serves the canonical
//! order, ignores `query.order` and mints no `next_cursor`, and this plugin owes
//! all three — so `$orderby`, cursor round-tripping and a page boundary falling
//! between an invalidation and its target are covered here or nowhere.
//!
//! Selection reads the covered period's **end** alone, `from <= window_end < to`
//! (`cpt-cf-usage-collector-adr-window-end-selection`). The ledger paths return
//! entries as persisted, withdrawn pairs included; the fold is the one path that
//! excludes them, and it excludes **both halves**
//! (`cpt-cf-usage-collector-adr-append-only-invalidation`).

mod common;

use std::collections::BTreeMap;

use bigdecimal::BigDecimal;
use rust_decimal::Decimal;
use time::Duration;
use uuid::Uuid;

use toolkit_odata::ast::{CompareOperator, Expr, Value};
use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, SortDir};

use usage_collector_sdk::{
    AggregationDimension, AggregationFold, KEYSET_SAFE_RECORD_FIELDS, MetadataFilter, MetadataKey,
    RecordOrigin, ResourceRef, SubjectRef, TimeRange, UsageCollectorPluginError, UsageRecord,
};

use timescaledb_usage_collector_plugin::domain::ports::RecordStore;
use timescaledb_usage_collector_plugin::infra::storage::record_store::PgRecordStore;

/// The fingerprint the gateway computes for the query a page is read under. The
/// value is opaque to this plugin; what matters is that the same string comes
/// back in the minted cursor's `f`.
const FILTER_HASH: &str = "fp-7c1f9a2e";

/// A container plus a store over it.
async fn setup() -> (common::TsHarness, PgRecordStore) {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    (h, store)
}

/// An all-ascending order over `fields`.
fn asc(fields: &[&str]) -> ODataOrderBy {
    ODataOrderBy(
        fields
            .iter()
            .map(|f| OrderKey {
                field: (*f).to_owned(),
                dir: SortDir::Asc,
            })
            .collect(),
    )
}

/// An all-descending order over `fields`.
fn desc(fields: &[&str]) -> ODataOrderBy {
    ODataOrderBy(
        fields
            .iter()
            .map(|f| OrderKey {
                field: (*f).to_owned(),
                dir: SortDir::Desc,
            })
            .collect(),
    )
}

/// A dispatch the gateway would make: the given order, the fingerprint it
/// guarantees on every `list_usage_records` call, and an optional page size.
fn page_query(order: ODataOrderBy, limit: Option<u64>) -> ODataQuery {
    let q = ODataQuery::new()
        .with_order(order)
        .with_filter_hash(FILTER_HASH.to_owned());
    match limit {
        Some(n) => q.with_limit(n),
        None => q,
    }
}

/// The gateway-default keyset: `(window_end, id)`, ascending.
fn default_order() -> ODataOrderBy {
    asc(&["window_end", "id"])
}

fn eq_str(field: &str, value: &str) -> Expr {
    Expr::Compare(
        Box::new(Expr::Identifier(field.to_owned())),
        CompareOperator::Eq,
        Box::new(Expr::Value(Value::String(value.to_owned()))),
    )
}

fn eq_uuid(field: &str, value: Uuid) -> Expr {
    Expr::Compare(
        Box::new(Expr::Identifier(field.to_owned())),
        CompareOperator::Eq,
        Box::new(Expr::Value(Value::Uuid(value))),
    )
}

/// A range wide enough to select every fixture below.
fn wide_range() -> TimeRange {
    TimeRange::new(
        common::fixture_window_start() - Duration::days(1),
        common::fixture_window_end() + Duration::days(1),
    )
    .expect("ordered range")
}

/// `n` entries one hour apart, so `(window_end, id)` is strictly increasing
/// across them and a page boundary is observable.
async fn seed_hourly(store: &PgRecordStore, tenant: Uuid, n: i64) -> Vec<UsageRecord> {
    let meter = common::meter(common::VCPU_METER);
    let mut out = Vec::with_capacity(usize::try_from(n).expect("small n"));
    for i in 0..n {
        let rec = common::entry_over(
            &meter,
            tenant,
            &format!("idem-{i}"),
            Decimal::from(i + 1),
            common::fixture_window_start() + Duration::hours(i),
            common::fixture_window_end() + Duration::hours(i),
        );
        out.push(store.create(rec).await.expect("seed entry"));
    }
    out
}

/// One seeded entry for [`grouping_folds_by_column_by_metadata_and_drops_rows_missing_the_dimension`]:
/// key, quantity, resource id, optional subject id, and the value of its
/// `region` metadata key.
struct Seed {
    key: &'static str,
    value: i64,
    resource: &'static str,
    subject: Option<&'static str>,
    region: &'static str,
}

/// The bucket value of a bare (ungrouped) fold.
fn only_bucket_value(result: &usage_collector_sdk::AggregationResult) -> Option<BigDecimal> {
    assert_eq!(
        result.buckets.len(),
        1,
        "an empty group_by must yield exactly one bucket, never an empty list: {:?}",
        result.buckets
    );
    assert!(
        result.buckets[0].key.is_empty(),
        "the single ungrouped bucket carries an empty key"
    );
    result.buckets[0].value.clone()
}

// ---------------------------------------------------------------------------
// Selection: from <= window_end < to
// ---------------------------------------------------------------------------

/// Both boundaries of the range, and the bound they are read against.
///
/// The lower bound is inclusive and the upper exclusive, and both are tested
/// against a `window_end` sitting exactly on them — which is the only place the
/// two can be told apart from `>` / `<=`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selection_is_lower_inclusive_and_upper_exclusive_on_window_end() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x2001);

    let at_lower = common::fixture_window_end();
    let at_upper = at_lower + Duration::hours(1);
    let on_lower = common::entry_over(
        &meter,
        tenant,
        "idem-lower",
        Decimal::ONE,
        common::fixture_window_start(),
        at_lower,
    );
    let on_upper = common::entry_over(
        &meter,
        tenant,
        "idem-upper",
        Decimal::from(2),
        common::fixture_window_start(),
        at_upper,
    );
    let (lower_id, upper_id) = (on_lower.id, on_upper.id);
    store.create(on_lower).await.expect("seed lower");
    store.create(on_upper).await.expect("seed upper");

    let range = TimeRange::new(at_lower, at_upper).expect("ordered range");
    let page = store
        .list(
            meter.clone(),
            range,
            &page_query(default_order(), None),
            &[],
        )
        .await
        .expect("list");

    let ids: Vec<Uuid> = page.items.iter().map(|r| r.id).collect();
    assert_eq!(
        ids,
        vec![lower_id],
        "an entry whose window_end equals `from` is selected; one whose window_end \
         equals `to` is not"
    );
    assert!(!ids.contains(&upper_id));
}

/// A point event (`window_start == window_end`) needs no special case: it is
/// selected by the same predicate as everything else, because the predicate
/// never names `window_start`.
///
/// The companion assertion is the one that distinguishes end-selection from
/// overlap or containment: an entry whose `window_start` is **inside** the range
/// and whose `window_end` is **outside** it is not selected — a period longer
/// than the range is exactly the case those three rules disagree on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_point_event_is_selected_and_a_period_ending_outside_the_range_is_not() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x2002);

    let from = common::fixture_window_start();
    let to = from + Duration::hours(2);

    // A point event inside the range.
    let instant = from + Duration::hours(1);
    let point = common::entry_over(&meter, tenant, "idem-point", Decimal::ONE, instant, instant);
    let point_id = point.id;
    store.create(point).await.expect("seed point event");

    // A period that starts inside the range and ends after it. Overlap would
    // select it; containment would drop the point event; end-selection drops
    // this one and keeps that one.
    let straddling = common::entry_over(
        &meter,
        tenant,
        "idem-straddle",
        Decimal::from(2),
        from + Duration::minutes(30),
        to + Duration::hours(1),
    );
    let straddling_id = straddling.id;
    store
        .create(straddling)
        .await
        .expect("seed straddling entry");

    let page = store
        .list(
            meter.clone(),
            TimeRange::new(from, to).expect("ordered range"),
            &page_query(default_order(), None),
            &[],
        )
        .await
        .expect("list");

    let ids: Vec<Uuid> = page.items.iter().map(|r| r.id).collect();
    assert_eq!(
        ids,
        vec![point_id],
        "only the point event's end falls in the range"
    );
    assert!(
        !ids.contains(&straddling_id),
        "an entry whose window_start is inside the range but whose window_end is outside \
         it must not be selected: the rule reads the end alone"
    );

    let stored_point = page.items.first().expect("one item");
    assert_eq!(
        stored_point.window_start, stored_point.window_end,
        "the zero-length period round-trips as one"
    );
}

// ---------------------------------------------------------------------------
// Keyset pagination
// ---------------------------------------------------------------------------

/// A cursor round-trips, and its `f` is the fingerprint the call was dispatched
/// with — verbatim.
///
/// This is the one SPI requirement with no compiler backstop: a plugin written
/// before it recompiles clean and paginates exactly once. The gateway recomputes
/// the same string from the follow-up request and refuses a token carrying a
/// different one, so a plugin that mints `f: None` silently truncates every
/// result set to its first page.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_minted_cursor_carries_the_dispatched_filter_hash_verbatim() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x2003);
    seed_hourly(&store, tenant, 5).await;

    let page = store
        .list(
            meter,
            wide_range(),
            &page_query(default_order(), Some(2)),
            &[],
        )
        .await
        .expect("list");

    assert_eq!(
        page.items.len(),
        2,
        "the page carries $top items, not the look-ahead row"
    );
    let token = page
        .page_info
        .next_cursor
        .as_ref()
        .expect("a next_cursor is minted while entries remain");
    let cursor = CursorV1::decode(token).expect("the minted token decodes");
    assert_eq!(
        cursor.f.as_deref(),
        Some(FILTER_HASH),
        "next_cursor.f must be the dispatched query.filter_hash, carried verbatim"
    );
    assert_eq!(cursor.d, "fwd", "v1 mints forward cursors only");
    assert_eq!(
        cursor.o,
        SortDir::Asc,
        "the token records the order it was read in"
    );
    assert_eq!(
        cursor.s,
        default_order().to_signed_tokens(),
        "and the order's signed tokens, so a changed order is caught rather than silently \
         re-binding the keys to different columns"
    );
    assert_eq!(cursor.k.len(), 2, "one key per order field");
}

/// Walking the cursor covers every entry exactly once, in order, with no gap
/// and no overlap — ascending and descending.
///
/// Both directions, because the keyset predicate derives its comparison operator
/// from the sort direction: a descending walk emits `<` where an ascending one
/// emits `>`, and getting that backwards re-serves page one forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cursor_walk_covers_every_entry_exactly_once_in_both_directions() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x2004);
    let seeded = seed_hourly(&store, tenant, 7).await;
    let ascending: Vec<Uuid> = seeded.iter().map(|r| r.id).collect();

    for (label, order, expected) in [
        ("ascending", default_order(), ascending.clone()),
        ("descending", desc(&["window_end", "id"]), {
            let mut v = ascending.clone();
            v.reverse();
            v
        }),
    ] {
        let mut walked: Vec<Uuid> = Vec::new();
        let mut cursor: Option<CursorV1> = None;
        for step in 0..10 {
            let mut q = page_query(order.clone(), Some(2));
            if let Some(c) = cursor.clone() {
                q = q.with_cursor(c);
            }
            let page = store
                .list(meter.clone(), wide_range(), &q, &[])
                .await
                .unwrap_or_else(|e| panic!("{label} page {step}: {e:?}"));
            walked.extend(page.items.iter().map(|r| r.id));
            match page.page_info.next_cursor.as_ref() {
                Some(token) => {
                    cursor = Some(CursorV1::decode(token).expect("decode the continuation"));
                }
                None => break,
            }
        }
        assert_eq!(
            walked, expected,
            "{label}: the walk must cover every entry exactly once, in the order it asked for"
        );
    }
}

/// A page boundary falling **between an invalidation and its target**.
///
/// The SPI names this case explicitly: the pair shares a `window_end` but not an
/// `id`, and an admissible order names both, so the boundary can fall between
/// them whichever of the two the order leads with. What must hold is that the
/// walk still returns both, exactly once each — the ledger owes the pair as
/// persisted, and a consumer folding it out folds over a range it has read
/// whole rather than over a single page.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_boundary_between_an_invalidation_and_its_target_loses_neither() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x2005);

    let target = common::entry(&meter, tenant, "idem-target", Decimal::from(10));
    let target = store.create(target).await.expect("create the target");
    let withdrawal = common::withdrawal_of(&target, "idem-withdrawal");
    let withdrawal = store
        .create(withdrawal)
        .await
        .expect("create the withdrawal");
    assert_eq!(
        target.window_end, withdrawal.window_end,
        "a faithful withdrawal copies its target's covered period, so the two tie on the \
         leading order key and only `id` separates them"
    );
    assert_ne!(target.id, withdrawal.id);

    // `$top = 1` puts the boundary exactly between them.
    let mut walked: Vec<Uuid> = Vec::new();
    let mut cursor: Option<CursorV1> = None;
    for step in 0..5 {
        let mut q = page_query(default_order(), Some(1));
        if let Some(c) = cursor.clone() {
            q = q.with_cursor(c);
        }
        let page = store
            .list(meter.clone(), wide_range(), &q, &[])
            .await
            .unwrap_or_else(|e| panic!("page {step}: {e:?}"));
        assert!(page.items.len() <= 1);
        walked.extend(page.items.iter().map(|r| r.id));
        match page.page_info.next_cursor.as_ref() {
            Some(token) => cursor = Some(CursorV1::decode(token).expect("decode")),
            None => break,
        }
    }

    let mut expected = vec![target.id, withdrawal.id];
    expected.sort();
    let mut got = walked.clone();
    got.sort();
    assert_eq!(
        got, expected,
        "both halves of the pair must survive a boundary drawn between them, each once: \
         walked {walked:?}"
    );
}

/// `$orderby` on every field the SDK publishes as keyset-safe.
///
/// Driven from [`KEYSET_SAFE_RECORD_FIELDS`] itself, not from a copy of it, so a
/// field added to that list without a column behind it fails here rather than
/// being quietly untested. Each field is asserted by **its own values coming
/// back sorted**, because entries tying on the ordered field may come back in
/// any order among themselves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_keyset_safe_field_is_an_admissible_order() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    // Two tenants, so `tenant_id` discriminates; the list path is scoped by
    // meter and range, not by tenant, so both are visible to one call.
    let tenants = [Uuid::from_u128(0x2006_0001), Uuid::from_u128(0x2006_0002)];

    for (t, tenant) in tenants.into_iter().enumerate() {
        for i in 0..3_i64 {
            let mut rec = common::entry_over(
                &meter,
                tenant,
                &format!("idem-{t}-{i}"),
                Decimal::from(i + 1),
                common::fixture_window_start() + Duration::hours(i),
                common::fixture_window_end() + Duration::hours(i),
            );
            rec.resource_ref = ResourceRef::new(
                format!("res-{}", (b'a' + u8::try_from(i).expect("small")) as char),
                format!("type-{}", 2 - i),
            )
            .expect("valid resource ref");
            rec.origin = if i == 0 {
                RecordOrigin::Backfill
            } else {
                RecordOrigin::Live
            };
            store.create(rec).await.expect("seed");
        }
    }

    for field in KEYSET_SAFE_RECORD_FIELDS {
        for dir in [SortDir::Asc, SortDir::Desc] {
            let order = match dir {
                SortDir::Asc => asc(&[field]),
                SortDir::Desc => desc(&[field]),
            };
            let page = store
                .list(meter.clone(), wide_range(), &page_query(order, None), &[])
                .await
                .unwrap_or_else(|e| panic!("$orderby={field} {dir:?} must be admissible: {e:?}"));
            assert_eq!(
                page.items.len(),
                6,
                "$orderby={field}: every entry is still returned"
            );

            let keys: Vec<String> = page.items.iter().map(|r| order_key_of(r, field)).collect();
            let mut sorted = keys.clone();
            sorted.sort();
            if dir == SortDir::Desc {
                sorted.reverse();
            }
            assert_eq!(
                keys, sorted,
                "$orderby={field} {dir:?} must return that field's values in that order"
            );
        }
    }
}

/// The value `$orderby=<field>` sorts on, rendered so a string comparison
/// agrees with the column's own collation for these fixtures.
///
/// A `match` rather than a map so a new entry in [`KEYSET_SAFE_RECORD_FIELDS`]
/// fails to compile here, next to the decision it needs.
fn order_key_of(record: &UsageRecord, field: &str) -> String {
    match field {
        "id" => record.id.to_string(),
        "window_start" => record.window_start.unix_timestamp().to_string(),
        "window_end" => record.window_end.unix_timestamp().to_string(),
        "tenant_id" => record.tenant_id.to_string(),
        "resource_id" => record.resource_ref.resource_id().to_owned(),
        "resource_type" => record.resource_ref.resource_type().to_owned(),
        "origin" => record.origin.as_str().to_owned(),
        other => panic!("no order key for `{other}`; KEYSET_SAFE_RECORD_FIELDS grew"),
    }
}

// ---------------------------------------------------------------------------
// $filter
// ---------------------------------------------------------------------------

/// Each of the eight published `$filter` fields resolves to a column and
/// discriminates.
///
/// "Resolves" and "discriminates" are two claims and the second is the one worth
/// the fixtures: a filter that resolves but selects everything, or nothing, is
/// indistinguishable from a working one on a single-row table. So each case
/// names the exact id set it expects out of a seeded population that contains
/// counterexamples for every field.
///
/// `origin` and `entry_type` are the two the DESIGN section 3.3 contract suite's
/// fixtures cannot serve, and they are also the two whose columns are unlike the
/// rest: `entry_type` is a stored generated column over `invalidates`, and
/// `origin` is server-assigned rather than caller-supplied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_published_filter_field_resolves_and_discriminates() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant_a = Uuid::from_u128(0x2007_000A);
    let tenant_b = Uuid::from_u128(0x2007_000B);

    // Entry 0: tenant A, res-a/type-a, subject s-a/kind-a, live.
    let mut e0 = common::entry_over(
        &meter,
        tenant_a,
        "idem-0",
        Decimal::ONE,
        common::fixture_window_start(),
        common::fixture_window_end(),
    );
    e0.resource_ref = ResourceRef::new("res-a", "type-a").expect("valid");
    e0.subject_ref = Some(SubjectRef::new("s-a", Some("kind-a")).expect("valid"));
    let e0 = store.create(e0).await.expect("seed 0");

    // Entry 1: tenant B, res-b/type-b, subject s-b/kind-b, backfill, one hour on.
    let mut e1 = common::entry_over(
        &meter,
        tenant_b,
        "idem-1",
        Decimal::from(2),
        common::fixture_window_start() + Duration::hours(1),
        common::fixture_window_end() + Duration::hours(1),
    );
    e1.resource_ref = ResourceRef::new("res-b", "type-b").expect("valid");
    e1.subject_ref = Some(SubjectRef::new("s-b", Some("kind-b")).expect("valid"));
    e1.origin = RecordOrigin::Backfill;
    let e1 = store.create(e1).await.expect("seed 1");

    // Entry 2: a withdrawal of entry 0. Same tenant, period and attribution as
    // its target, so it is only `entry_type` and `invalidates` that separate it.
    let w = common::withdrawal_of(&e0, "idem-w");
    let w = store.create(w).await.expect("seed the withdrawal");

    let cases: Vec<(&str, Expr, Vec<Uuid>)> = vec![
        ("tenant_id", eq_uuid("tenant_id", tenant_b), vec![e1.id]),
        ("resource_id", eq_str("resource_id", "res-b"), vec![e1.id]),
        ("resource_type", eq_str("resource_type", "type-a"), {
            let mut v = vec![e0.id, w.id];
            v.sort();
            v
        }),
        ("subject_id", eq_str("subject_id", "s-b"), vec![e1.id]),
        ("subject_type", eq_str("subject_type", "kind-a"), {
            let mut v = vec![e0.id, w.id];
            v.sort();
            v
        }),
        (
            "entry_type",
            eq_str("entry_type", "invalidation"),
            vec![w.id],
        ),
        ("origin", eq_str("origin", "backfill"), vec![e1.id]),
        ("invalidates", eq_uuid("invalidates", e0.id), vec![w.id]),
    ];

    for (field, expr, expected) in cases {
        let q = page_query(default_order(), None).with_filter(expr);
        let page = store
            .list(meter.clone(), wide_range(), &q, &[])
            .await
            .unwrap_or_else(|e| panic!("$filter on `{field}` must resolve: {e:?}"));
        let mut got: Vec<Uuid> = page.items.iter().map(|r| r.id).collect();
        got.sort();
        assert_eq!(
            got, expected,
            "$filter on `{field}` must discriminate, not select everything or nothing"
        );
    }

    // The complement of the `entry_type` case: filtering for `record` returns
    // the two measurements and not the withdrawal. Written out because a
    // generated column that answered one spelling and not the other would pass
    // the case above.
    let q = page_query(default_order(), None).with_filter(eq_str("entry_type", "record"));
    let page = store
        .list(meter, wide_range(), &q, &[])
        .await
        .expect("list");
    let mut got: Vec<Uuid> = page.items.iter().map(|r| r.id).collect();
    got.sort();
    let mut expected = vec![e0.id, e1.id];
    expected.sort();
    assert_eq!(got, expected);
}

/// The metadata side channel: AND across filters, OR within one filter's values.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_metadata_side_channel_narrows_the_selection() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x2008);

    let mut ids = Vec::new();
    for (i, (region, tier)) in [("eu", "gold"), ("eu", "silver"), ("us", "gold")]
        .into_iter()
        .enumerate()
    {
        let i = i64::try_from(i).expect("small");
        let mut rec = common::entry_over(
            &meter,
            tenant,
            &format!("idem-{i}"),
            Decimal::from(i + 1),
            common::fixture_window_start() + Duration::hours(i),
            common::fixture_window_end() + Duration::hours(i),
        );
        rec.metadata.insert(
            MetadataKey::new("region").expect("valid key"),
            region.to_owned(),
        );
        rec.metadata.insert(
            MetadataKey::new("tier").expect("valid key"),
            tier.to_owned(),
        );
        ids.push(store.create(rec).await.expect("seed").id);
    }

    // One filter, two values: OR within.
    let page = store
        .list(
            meter.clone(),
            wide_range(),
            &page_query(default_order(), None),
            &[MetadataFilter::new("tier", ["gold", "platinum"]).expect("valid filter")],
        )
        .await
        .expect("list");
    let mut got: Vec<Uuid> = page.items.iter().map(|r| r.id).collect();
    got.sort();
    let mut expected = vec![ids[0], ids[2]];
    expected.sort();
    assert_eq!(got, expected, "OR within one filter's value set");

    // Two filters: AND across.
    let page = store
        .list(
            meter,
            wide_range(),
            &page_query(default_order(), None),
            &[
                MetadataFilter::new("region", ["eu"]).expect("valid filter"),
                MetadataFilter::new("tier", ["gold"]).expect("valid filter"),
            ],
        )
        .await
        .expect("list");
    let got: Vec<Uuid> = page.items.iter().map(|r| r.id).collect();
    assert_eq!(got, vec![ids[0]], "AND across filters");
}

// ---------------------------------------------------------------------------
// The point lookup
// ---------------------------------------------------------------------------

/// An entry outside the caller's compiled scope and an entry that never existed
/// are **one answer**.
///
/// The scope is part of the `WHERE`, so a withheld row is already
/// indistinguishable from an absent one by the time the store looks at the
/// result — and nothing may be logged, counted or timed that would tell them
/// apart, because that distinction *is* an existence oracle over every tenant's
/// entries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_outside_the_scope_and_a_row_that_does_not_exist_are_one_answer() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let mine = Uuid::from_u128(0x2009_0001);
    let theirs = Uuid::from_u128(0x2009_0002);

    let ours = common::entry(&meter, mine, "idem-mine", Decimal::ONE);
    let ours = store.create(ours).await.expect("create ours");
    let hidden = common::entry(&meter, theirs, "idem-theirs", Decimal::from(2));
    let hidden = store.create(hidden).await.expect("create theirs");

    let scope = common::tenant_scope(mine);

    // In scope: found.
    let got = store.get(ours.id, &scope).await.expect("our own entry");
    assert_eq!(got.id, ours.id);

    // Out of scope: not found, naming the id that was asked for.
    let withheld = store
        .get(hidden.id, &scope)
        .await
        .expect_err("an entry outside the scope must not be served");
    // Absent entirely: the same error, for an id nothing ever derived.
    let absent_id = Uuid::from_u128(0x2009_DEAD);
    let absent = store
        .get(absent_id, &scope)
        .await
        .expect_err("an id that was never stored is not found");

    match (&withheld, &absent) {
        (
            UsageCollectorPluginError::UsageRecordNotFound { id: a },
            UsageCollectorPluginError::UsageRecordNotFound { id: b },
        ) => {
            assert_eq!(*a, hidden.id);
            assert_eq!(*b, absent_id);
        }
        other => panic!("both must be UsageRecordNotFound, got {other:?}"),
    }
    assert_eq!(
        std::mem::discriminant(&withheld),
        std::mem::discriminant(&absent),
        "withheld and absent must be one variant: any difference is an existence oracle"
    );

    // And the row really is there, for the tenant that owns it.
    let theirs_scope = common::tenant_scope(theirs);
    assert_eq!(
        store
            .get(hidden.id, &theirs_scope)
            .await
            .expect("their entry")
            .id,
        hidden.id,
        "the withheld row exists; it was the scope that withheld it"
    );
}

/// The subject attribution survives a write and a point lookup, both halves of
/// it, including the untyped-subject shape the model allows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_subject_reference_round_trips_through_create_and_get() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x200A);
    let scope = common::tenant_scope(tenant);

    let mut typed = common::entry(&meter, tenant, "idem-typed", Decimal::ONE);
    typed.subject_ref = Some(SubjectRef::new("subj-1", Some("user")).expect("valid"));
    let typed_id = typed.id;
    store.create(typed).await.expect("create typed subject");

    let mut untyped = common::entry_over(
        &meter,
        tenant,
        "idem-untyped",
        Decimal::from(2),
        common::fixture_window_start() + Duration::hours(1),
        common::fixture_window_end() + Duration::hours(1),
    );
    untyped.subject_ref = Some(SubjectRef::new("subj-2", None::<String>).expect("valid"));
    let untyped_id = untyped.id;
    store.create(untyped).await.expect("create untyped subject");

    let got = store.get(typed_id, &scope).await.expect("get typed");
    let subject = got.subject_ref.expect("subject_ref round-trips");
    assert_eq!(subject.subject_id(), "subj-1");
    assert_eq!(subject.subject_type(), Some("user"));

    let got = store.get(untyped_id, &scope).await.expect("get untyped");
    let subject = got.subject_ref.expect("subject_ref round-trips");
    assert_eq!(subject.subject_id(), "subj-2");
    assert_eq!(
        subject.subject_type(),
        None,
        "a subject without a type is a subject, not an absent one"
    );
}

// ---------------------------------------------------------------------------
// The fold
// ---------------------------------------------------------------------------

/// A withdrawn pair is returned as persisted by both ledger paths, and
/// contributes nothing to any fold — **both halves**.
///
/// This is the rule that replaced the retired `corrects_id` model's, and it is
/// its opposite: an invalidation echoes the quantity it withdraws rather than
/// negating it, so netting the two would double-count. Every fold applies the
/// same exclusion; there is no per-fold rule.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_withdrawn_pair_is_returned_by_the_ledger_and_folded_by_nothing() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x200B);
    let scope = common::tenant_scope(tenant);

    let withdrawn = common::entry(&meter, tenant, "idem-withdrawn", Decimal::from(10));
    let withdrawn = store.create(withdrawn).await.expect("create the target");
    let withdrawal = common::withdrawal_of(&withdrawn, "idem-withdrawal");
    let withdrawal = store
        .create(withdrawal)
        .await
        .expect("create the withdrawal");
    let standing = common::entry_over(
        &meter,
        tenant,
        "idem-standing",
        Decimal::from(3),
        common::fixture_window_start() + Duration::hours(1),
        common::fixture_window_end() + Duration::hours(1),
    );
    let standing = store
        .create(standing)
        .await
        .expect("create the standing entry");

    // The ledger returns all three.
    let page = store
        .list(
            meter.clone(),
            wide_range(),
            &page_query(default_order(), None),
            &[],
        )
        .await
        .expect("list");
    let mut got: Vec<Uuid> = page.items.iter().map(|r| r.id).collect();
    got.sort();
    let mut all = vec![withdrawn.id, withdrawal.id, standing.id];
    all.sort();
    assert_eq!(
        got, all,
        "the raw list is the ledger: hiding either half destroys the audit trail"
    );
    // And so does the point lookup, for each half.
    assert_eq!(
        store
            .get(withdrawn.id, &scope)
            .await
            .expect("get target")
            .id,
        withdrawn.id
    );
    assert_eq!(
        store
            .get(withdrawal.id, &scope)
            .await
            .expect("get withdrawal")
            .id,
        withdrawal.id
    );

    // The fold sees only the standing entry, under every fold.
    for (fold, expected) in [
        (AggregationFold::Sum, BigDecimal::from(3)),
        (AggregationFold::Count, BigDecimal::from(1)),
        (AggregationFold::Min, BigDecimal::from(3)),
        (AggregationFold::Max, BigDecimal::from(3)),
        (AggregationFold::Latest, BigDecimal::from(3)),
    ] {
        let result = store
            .aggregate(
                meter.clone(),
                wide_range(),
                fold,
                &ODataQuery::new(),
                &[],
                &[],
            )
            .await
            .unwrap_or_else(|e| panic!("{fold}: {e:?}"));
        assert_eq!(
            only_bucket_value(&result).map(|v| v.normalized()),
            Some(expected.normalized()),
            "{fold} must exclude both halves of the withdrawn pair: the withdrawal itself \
             and the entry it names"
        );
    }
}

/// An orphan invalidation — one whose target has been purged by retention —
/// still contributes nothing.
///
/// Retention is plugin-owned, so a conforming deployment really can hold a
/// withdrawal whose target's chunk is gone. That orphan is the echoed quantity
/// with nothing left to pair it against, which is why the exclusion is two
/// obligations rather than one conditional: the `invalidates IS NULL` conjunct
/// stands on its own and is not an optimization of the `NOT EXISTS` one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_orphan_invalidation_contributes_nothing() {
    let (h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x200C);

    let target = common::entry(&meter, tenant, "idem-target", Decimal::from(10));
    let target = store.create(target).await.expect("create the target");
    let withdrawal = common::withdrawal_of(&target, "idem-withdrawal");
    store
        .create(withdrawal)
        .await
        .expect("create the withdrawal");
    let standing = common::entry_over(
        &meter,
        tenant,
        "idem-standing",
        Decimal::from(3),
        common::fixture_window_start() + Duration::hours(1),
        common::fixture_window_end() + Duration::hours(1),
    );
    store
        .create(standing)
        .await
        .expect("create the standing entry");

    // Purge the target the way retention would: the row goes, the withdrawal
    // that named it stays.
    let purged = sqlx::query("DELETE FROM usage_records WHERE id = $1")
        .bind(target.id)
        .execute(&h.pool)
        .await
        .expect("purge the target")
        .rows_affected();
    assert_eq!(purged, 1, "the target was purged");

    let result = store
        .aggregate(
            meter,
            wide_range(),
            AggregationFold::Sum,
            &ODataQuery::new(),
            &[],
            &[],
        )
        .await
        .expect("aggregate");
    assert_eq!(
        only_bucket_value(&result).map(|v| v.normalized()),
        Some(BigDecimal::from(3).normalized()),
        "the orphan's echoed quantity must not reappear in the total once its target is gone"
    );
}

/// The five folds over a known population, including `LATEST`'s declared
/// tie-break.
///
/// `LATEST` is greatest `window_end`, then greatest `acceptance_sequence`. The
/// tie-break needs two entries sharing a `window_end`, and the fixture below
/// has a pair at hour 1 — but sharing a `window_end` is only half of what the
/// fixture has to do.
///
/// **The other half is that the tie-break must disagree with the alternative.**
/// The SDK's reference backend has no `acceptance_sequence` and substitutes the
/// greatest `id`, which is why `latest-tie-break` sits in the contract suite's
/// `BLOCKED_CHECKS` and why this is the only place the declared rule is
/// exercised. A fixture whose acceptance-order winner *also* holds the greater
/// `id` passes identically under both rules and so demonstrates neither — and
/// the entry `id` is a `UUIDv5` over the 5-tuple, so which of two keys sorts
/// higher is not something a fixture author can predict. The pair is therefore
/// arranged so the later-accepted entry is the one with the **lower** `id`, and
/// `the_later_accepted_entry_must_hold_the_lower_id` below asserts exactly that
/// before the fold is asked anything. Without that assertion the discrimination
/// would be luck, and the plausible edit — making this plugin substitute
/// `MAX(id)` to unblock `latest-tie-break` — would leave the test green.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_five_folds_answer_over_a_known_population() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x200D);

    // Two entries at hour 0 (values 2 and 8), and two sharing hour 1's period
    // end. `idem-d` (5) is accepted first and `idem-c` (7) second, so 7 is the
    // acceptance-order winner — and `idem-c` is the one with the lower id, which
    // is what makes the greatest-id rule answer 5 here instead.
    let mut hour_one: Vec<UsageRecord> = Vec::new();
    for (i, value, key) in [
        (0_i64, 2_i64, "idem-a"),
        (0, 8, "idem-b"),
        (1, 5, "idem-d"),
        (1, 7, "idem-c"),
    ] {
        let rec = common::entry_over(
            &meter,
            tenant,
            key,
            Decimal::from(value),
            common::fixture_window_start() + Duration::hours(i),
            common::fixture_window_end() + Duration::hours(i),
        );
        let stored = store.create(rec).await.expect("seed");
        if i == 1 {
            hour_one.push(stored);
        }
    }

    let (earlier, later) = (&hour_one[0], &hour_one[1]);
    assert_eq!(
        earlier.window_end, later.window_end,
        "the pair ties on window_end"
    );
    assert!(
        later.id < earlier.id,
        "the fixture must pin acceptance order against id order, or this test cannot tell \
         the declared rule (greatest window_end, then greatest acceptance_sequence) from \
         the reference backend's greatest-id substitute. later={} earlier={}",
        later.id,
        earlier.id
    );

    for (fold, expected) in [
        (AggregationFold::Sum, 22_i64),
        (AggregationFold::Count, 4),
        (AggregationFold::Min, 2),
        (AggregationFold::Max, 8),
        // Greatest window_end is hour 1; of the two there, `idem-c` (7) was
        // accepted second and so carries the greater acceptance_sequence. Under
        // greatest-id the answer would be 5, which is the point of the
        // assertion above.
        (AggregationFold::Latest, 7),
    ] {
        let result = store
            .aggregate(
                meter.clone(),
                wide_range(),
                fold,
                &ODataQuery::new(),
                &[],
                &[],
            )
            .await
            .unwrap_or_else(|e| panic!("{fold}: {e:?}"));
        assert_eq!(
            only_bucket_value(&result).map(|v| v.normalized()),
            Some(BigDecimal::from(expected).normalized()),
            "{fold} over {{2, 8}} at hour 0 and {{5, 7}} at hour 1"
        );
    }
}

/// A bare fold over an **empty** selection still answers one bucket, and `COUNT`
/// answers `Some(0)` where the others answer `None`.
///
/// That split is `SELECT COUNT(*)`'s own answer rather than anything the plugin
/// does, which is exactly why only a real query demonstrates it: a unit test
/// reaches the statement that leads here and no further.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bare_fold_over_an_empty_selection_is_one_bucket_and_count_is_zero() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x200E);

    // A populated ledger, and a range that selects none of it — so the empty
    // selection is the range's doing rather than an empty table's.
    seed_hourly(&store, tenant, 3).await;
    let empty = TimeRange::new(
        common::fixture_window_end() + Duration::days(30),
        common::fixture_window_end() + Duration::days(31),
    )
    .expect("ordered range");

    let count = store
        .aggregate(
            meter.clone(),
            empty,
            AggregationFold::Count,
            &ODataQuery::new(),
            &[],
            &[],
        )
        .await
        .expect("count over an empty selection");
    assert_eq!(
        only_bucket_value(&count).map(|v| v.normalized()),
        Some(BigDecimal::from(0).normalized()),
        "COUNT over an empty selection is Some(0), not None: counting nothing is zero"
    );

    for fold in [
        AggregationFold::Sum,
        AggregationFold::Min,
        AggregationFold::Max,
        AggregationFold::Latest,
    ] {
        let result = store
            .aggregate(meter.clone(), empty, fold, &ODataQuery::new(), &[], &[])
            .await
            .unwrap_or_else(|e| panic!("{fold}: {e:?}"));
        assert_eq!(
            only_bucket_value(&result),
            None,
            "{fold} over an empty selection has no value to answer with"
        );
    }
}

/// Grouping: by a record column, by a metadata key, and by a dimension some rows
/// do not carry.
///
/// The last is the one with a rule behind it: a row missing the grouped
/// dimension is dropped rather than folded into a `NULL` bucket, so grouped
/// buckets need not sum to the ungrouped total.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grouping_folds_by_column_by_metadata_and_drops_rows_missing_the_dimension() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x200F);

    // res-a: 4 + 6; res-b: 5; and a fourth row with no subject at all.
    let rows = [
        Seed {
            key: "idem-1",
            value: 4,
            resource: "res-a",
            subject: Some("subj-a"),
            region: "eu",
        },
        Seed {
            key: "idem-2",
            value: 6,
            resource: "res-a",
            subject: Some("subj-a"),
            region: "eu",
        },
        Seed {
            key: "idem-3",
            value: 5,
            resource: "res-b",
            subject: Some("subj-b"),
            region: "us",
        },
        Seed {
            key: "idem-4",
            value: 7,
            resource: "res-b",
            subject: None,
            region: "us",
        },
    ];
    for (i, seed) in rows.into_iter().enumerate() {
        let Seed {
            key,
            value,
            resource,
            subject,
            region,
        } = seed;
        let i = i64::try_from(i).expect("small");
        let mut rec = common::entry_over(
            &meter,
            tenant,
            key,
            Decimal::from(value),
            common::fixture_window_start() + Duration::hours(i),
            common::fixture_window_end() + Duration::hours(i),
        );
        rec.resource_ref = ResourceRef::new(resource, "compute.vm").expect("valid");
        rec.subject_ref = subject.map(|s| SubjectRef::new(s, Some("user")).expect("valid"));
        rec.metadata.insert(
            MetadataKey::new("region").expect("valid key"),
            region.to_owned(),
        );
        store.create(rec).await.expect("seed");
    }

    let by = |dim: AggregationDimension| {
        let meter = meter.clone();
        let store = &store;
        async move {
            let result = store
                .aggregate(
                    meter,
                    wide_range(),
                    AggregationFold::Sum,
                    &ODataQuery::new(),
                    &[],
                    &[dim],
                )
                .await
                .expect("grouped aggregate");
            result
                .buckets
                .into_iter()
                .map(|b| {
                    (
                        b.key.join("|"),
                        b.value
                            .map(|v| v.normalized())
                            .expect("a grouped bucket has a value"),
                    )
                })
                .collect::<BTreeMap<String, BigDecimal>>()
        }
    };

    assert_eq!(
        by(AggregationDimension::ResourceId).await,
        BTreeMap::from([
            ("res-a".to_owned(), BigDecimal::from(10).normalized()),
            ("res-b".to_owned(), BigDecimal::from(12).normalized()),
        ]),
        "grouped by a record column"
    );

    assert_eq!(
        by(AggregationDimension::Metadata(
            MetadataKey::new("region").expect("valid key")
        ))
        .await,
        BTreeMap::from([
            ("eu".to_owned(), BigDecimal::from(10).normalized()),
            ("us".to_owned(), BigDecimal::from(12).normalized()),
        ]),
        "grouped by a metadata key"
    );

    assert_eq!(
        by(AggregationDimension::SubjectId).await,
        BTreeMap::from([
            ("subj-a".to_owned(), BigDecimal::from(10).normalized()),
            ("subj-b".to_owned(), BigDecimal::from(5).normalized()),
        ]),
        "the subject-less row joins no bucket rather than forming a NULL one, so the \
         grouped buckets (15) do not sum to the ungrouped total (22)"
    );
}

/// A composed `$filter` and the metadata side channel both narrow the fold, not
/// only the list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fold_honours_the_filter_and_the_metadata_side_channel() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant_a = Uuid::from_u128(0x2010_000A);
    let tenant_b = Uuid::from_u128(0x2010_000B);

    for (i, (tenant, value, region)) in [
        (tenant_a, 4_i64, "eu"),
        (tenant_a, 6, "us"),
        (tenant_b, 100, "eu"),
    ]
    .into_iter()
    .enumerate()
    {
        let i = i64::try_from(i).expect("small");
        let mut rec = common::entry_over(
            &meter,
            tenant,
            &format!("idem-{i}"),
            Decimal::from(value),
            common::fixture_window_start() + Duration::hours(i),
            common::fixture_window_end() + Duration::hours(i),
        );
        rec.metadata.insert(
            MetadataKey::new("region").expect("valid key"),
            region.to_owned(),
        );
        store.create(rec).await.expect("seed");
    }

    let scoped = ODataQuery::new().with_filter(eq_uuid("tenant_id", tenant_a));
    let result = store
        .aggregate(
            meter.clone(),
            wide_range(),
            AggregationFold::Sum,
            &scoped,
            &[],
            &[],
        )
        .await
        .expect("aggregate");
    assert_eq!(
        only_bucket_value(&result).map(|v| v.normalized()),
        Some(BigDecimal::from(10).normalized()),
        "the composed filter bounds the fold, not only the list"
    );

    let result = store
        .aggregate(
            meter,
            wide_range(),
            AggregationFold::Sum,
            &scoped,
            &[MetadataFilter::new("region", ["eu"]).expect("valid filter")],
            &[],
        )
        .await
        .expect("aggregate");
    assert_eq!(
        only_bucket_value(&result).map(|v| v.normalized()),
        Some(BigDecimal::from(4).normalized()),
        "the side channel narrows inside the filter"
    );
}
