//! Tests for the no-op storage backend's [`UsageCollectorPluginV1`] surface.

use std::collections::BTreeMap;

use toolkit_odata::ast;
use usage_collector_sdk::{
    AggregationFold, FeedPosition, FeedStart, IdempotencyKey, MeterRef, MeterTypeId,
    ReconciliationMetadata, RecordOrigin, ResourceRef, StoredUsageRecord, TimeRange,
    UsageCollectorPluginError, UsageCollectorPluginV1, UsageQuantity, UsageRecord,
};
use uuid::Uuid;

use super::NoopBackend;

/// The meter the fixtures below are written under.
///
/// The reference is arbitrary: this backend persists nothing and keys on
/// nothing, so the only obligation on the pair is that it is well formed.
fn sample_meter() -> MeterRef {
    MeterRef::new(
        Uuid::from_u128(0x5a_11_00_01),
        MeterTypeId::new("gts.cf.core.uc.usage_record.v1~test.uc.batch.order.v1~")
            .expect("meter type id fixture"),
    )
}

/// One fixture entry, in the storage shape the SPI carries.
///
/// Built as a [`UsageRecord`] and converted, rather than assembled field by
/// field, so the entry identity still goes on through the projection the
/// gear's own derivation uses.
fn sample_record(id: &str, idempotency_key: &str) -> StoredUsageRecord {
    UsageRecord {
        id: Uuid::parse_str(id).expect("valid record id fixture"),
        gts_type_id: MeterTypeId::new("gts.cf.core.uc.usage_record.v1~test.uc.batch.order.v1~")
            .expect("meter type id fixture"),
        tenant_id: Uuid::parse_str("22222222-2222-2222-2222-222222222222")
            .expect("valid tenant uuid fixture"),
        resource_ref: ResourceRef::new("vm-1", "compute.vm").expect("valid resource ref fixture"),
        subject_ref: None,
        metadata: BTreeMap::new(),
        quantity: UsageQuantity::parse("1").expect("fixture quantity"),
        idempotency_key: IdempotencyKey::new(idempotency_key)
            .expect("valid idempotency key fixture"),
        accepted_at: time::OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        origin: RecordOrigin::Live,
        invalidation: None,
        window_start: time::OffsetDateTime::UNIX_EPOCH,
        window_end: time::OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
    }
    .into_stored(sample_meter().uuid)
}

#[tokio::test]
async fn create_usage_records_preserves_input_order() {
    let backend = NoopBackend::new();
    let inputs = [
        sample_record("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", "k-1"),
        sample_record("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb", "k-2"),
        sample_record("cccccccc-cccc-cccc-cccc-cccccccccccc", "k-3"),
    ];

    let outputs = backend
        .create_usage_records(
            inputs
                .iter()
                .map(|record| (sample_meter(), record.clone()))
                .collect(),
        )
        .await
        .expect("noop create_usage_records must succeed on echo");

    assert_eq!(
        outputs.len(),
        inputs.len(),
        "result vec length MUST match input vec length",
    );
    for (i, (input, output)) in inputs.iter().zip(outputs.into_iter()).enumerate() {
        let echoed = output.unwrap_or_else(|e| {
            panic!("noop must Ok-echo every input record, got error at index {i}: {e:?}")
        });
        assert_eq!(
            &echoed, input,
            "echoed record at index {i} MUST equal the input at the same index",
        );
    }
}

#[tokio::test]
async fn create_usage_records_rejects_empty_batch_as_internal() {
    let backend = NoopBackend::new();

    let err = backend
        .create_usage_records(Vec::new())
        .await
        .expect_err("an empty batch is a host-contract breach, not a success");

    assert!(
        matches!(err, UsageCollectorPluginError::Internal(_)),
        "empty batch MUST surface as Internal (non-retryable host-contract breach), got {err:?}",
    );
}

/// The single one-byte position this backend issues, spelled out rather than
/// read back from the backend's own helper.
///
/// A test that asked the subject for its expected value would agree with
/// whatever value the subject chose, which is no assertion at all. The
/// encoding is this plugin's own, so a deliberate change to it lands here —
/// what must not change silently is that a live read carries *this* cursor
/// and a bounded one carries none.
fn noop_head() -> FeedPosition {
    FeedPosition::new(vec![0]).expect("one byte is an admissible feed position")
}

/// The meter every feed read below subscribes to.
///
/// This backend never reads the subscription — it retains nothing — so the
/// value only has to be well formed. `MeterTypeId::new` validates at
/// runtime, not at compile time, so a malformed literal here would panic
/// the test rather than fail the build.
fn feed_meter() -> MeterTypeId {
    MeterTypeId::new("gts.cf.core.uc.usage_record.v1~test.uc.noop.feed.v1~")
        .expect("the fixture meter id is well formed")
}

/// [`feed_meter`]'s reference, for the SPI methods below that take a
/// [`MeterRef`] rather than the bare identifier. This backend reads neither
/// field, so the `uuid` half is an arbitrary well-formed value.
fn feed_meter_ref() -> MeterRef {
    MeterRef::new(Uuid::nil(), feed_meter())
}

/// A compiled single-tenant grant: `tenant_id eq <tenant>`.
///
/// `ast::Expr` has no boolean literal, so the shortest well-formed scope is
/// a real comparison. This backend never evaluates it.
fn feed_scope() -> ast::Expr {
    ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("tenant_id".to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::Uuid(
            Uuid::parse_str("22222222-2222-2222-2222-222222222222")
                .expect("valid tenant uuid fixture"),
        ))),
    )
}

/// Both [`FeedStart`] variants, each with the spelling a failure message
/// names it by.
///
/// Every feed read below runs over both. `read_feed_page` ignores `start`
/// deliberately rather than by omission — with nothing retained,
/// `FeedStart::Oldest` and `FeedStart::After(head)` name the same position —
/// and that is a claim better stated by a test that exercises both than by a
/// comment saying so.
fn both_starts() -> Vec<(&'static str, FeedStart<FeedPosition>)> {
    vec![
        ("FeedStart::Oldest", FeedStart::Oldest),
        ("FeedStart::After(head)", FeedStart::After(noop_head())),
    ]
}

#[tokio::test]
async fn a_live_feed_read_is_an_empty_page_carrying_the_head_cursor() {
    let backend = NoopBackend::new();

    for (start_name, start) in both_starts() {
        let page = backend
            .read_feed_page(&[feed_meter_ref()], &feed_scope(), start, None, 16)
            .await
            .expect("the noop feed read encodes one constant byte and cannot fail");

        assert!(
            page.entries.is_empty(),
            "under {start_name}: this backend retains nothing, so every page it serves is \
             empty",
        );
        assert_eq!(
            page.next,
            Some(noop_head()),
            "under {start_name}: a live read MUST carry the head cursor. An *absent* cursor \
             means a completed bounded replay, which a live read has not done: a gateway \
             handed one here would stop following a feed that is still open, and DESIGN \
             section 3.3's `feed-bootstrap-position` requires an empty page with a head cursor \
             of a subscription retaining no entries",
        );
    }
}

#[tokio::test]
async fn a_bounded_feed_replay_closes_because_the_head_is_already_reached() {
    let backend = NoopBackend::new();

    for (start_name, start) in both_starts() {
        let page = backend
            .read_feed_page(
                &[feed_meter_ref()],
                &feed_scope(),
                start,
                Some(noop_head()),
                16,
            )
            .await
            .expect("the noop feed read encodes one constant byte and cannot fail");

        assert!(
            page.entries.is_empty(),
            "under {start_name}: this backend retains nothing, so every page it serves is \
             empty",
        );
        assert!(
            page.next.is_none(),
            "under {start_name}: this is the anti-hang guard. The only position this backend \
             issues is the head, so any `until` against it is already reached, and an absent \
             cursor is what says a bounded replay has reached its `until`. A head cursor here \
             is not a wrong value a caller reads and moves on from; it is a page whose \
             continuation is itself, so a gateway following the cursor spins forever. Got: \
             {:?}",
            page.next,
        );
    }
}

/// Every fold, and the answer that fold has over an empty selection.
///
/// The loop over all five is the assertion, and it is the shape that made the
/// split visible in the first place: a single call would leave open whether
/// the backend reads `fold` at all.
///
/// The two halves differ because they are asking different things. `SUM` and
/// `COUNT` are totals and the total of nothing is a defined zero; `MAX`,
/// `MIN` and `LATEST` select an entry and there is none to select, so there
/// is nothing to report. Everything else is the same under all five: this
/// backend stores nothing, so the count is zero and both watermarks are
/// absent whatever the caller asked for.
#[tokio::test]
async fn reconciliation_metadata_answers_per_fold_over_an_empty_selection() {
    let backend = NoopBackend::new();
    let tenant_id =
        Uuid::parse_str("22222222-2222-2222-2222-222222222222").expect("valid tenant uuid fixture");
    let range = TimeRange::new(
        time::OffsetDateTime::UNIX_EPOCH,
        time::OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
    )
    .expect("an ordered probe range");

    for fold in [
        AggregationFold::Sum,
        AggregationFold::Count,
        AggregationFold::Max,
        AggregationFold::Min,
        AggregationFold::Latest,
    ] {
        let metadata = backend
            .get_reconciliation_metadata(tenant_id, &feed_meter_ref(), range, fold, &feed_scope())
            .await
            .expect("the noop reconciliation read has nothing to fail on");

        // Whole-struct, not field by field: a backend that filled in a count
        // and both watermarks and forgot `quantity_summary` would pass a
        // field-by-field comparison of the three it did fill.
        assert_eq!(
            metadata,
            ReconciliationMetadata::empty_for(fold),
            "this backend stores nothing, so every scope holds no entries under every fold, \
             and `empty_for({fold})` is the whole of what a scope holding nothing reports",
        );
    }
}
