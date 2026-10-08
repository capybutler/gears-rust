//! `MeterRef` threading tests for the three parameter-carrying read methods.
//!
//! One module rather than three (`service_tests.rs`'s own convention is one
//! test module per concern, and this concern — the reference a resolved
//! declaration hands the plugin — is shared by `list_usage_records`,
//! `query_aggregated_usage_records` and `get_reconciliation_metadata`
//! alike). `service_tests.rs`'s own fixtures (`SAMPLE_METER_TYPE_ID`,
//! `sample_create_record`, `sample_withdrawal`) are private to
//! `service_metrics_tests.rs` and unreachable from here, so this module
//! copies what it needs rather than reaching across the boundary. Tasks 3
//! and 4 append their own tests to this module.

use std::collections::BTreeMap;
use std::sync::Arc;

use toolkit_odata::ODataQuery;
use usage_collector_sdk::{
    CreateUsageRecord, EntryType, FeedStart, FeedSubscription, IdempotencyKey, MeterRef,
    MeterTypeId, ReasonCode, ResourceRef, UsageCollectorError, UsageCollectorPluginV1,
};
use uuid::Uuid;

use crate::domain::test_support::{
    ConflictingPlugin, ForeignMeterPlugin, PanickingPlugin, RecordingPlugin, SingleRecordPlugin,
    TargetHoldingPlugin, authenticated_ctx, qty, recent_window_end, recent_window_start,
    service_denying_with_panicking_resolver, service_resolving_only, service_with_declaration_uuid,
    service_with_panicking_reverse_resolver, service_with_reverse_mapping,
    service_with_unreachable_registry,
};

/// The meter every test in this module dispatches against.
///
/// Copied from `service_metrics_tests.rs`'s own fixture rather than reached
/// for across the module boundary — see the module doc.
const SAMPLE_METER_TYPE_ID: &str =
    toolkit_gts::gts_id!("cf.core.uc.usage_record.v1~example.usage._.bytes_in.v1~");

/// A second, distinct valid meter identifier — Task 3's feed tests need two
/// meters to tell a subscription's resolvable member from its unresolvable
/// one.
const OTHER_SAMPLE_METER_TYPE_ID: &str =
    toolkit_gts::gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_cached.v1~");

/// An arbitrary valid read-path range. No test in this module asserts on the
/// range itself, so its only obligation is to be well formed.
fn any_time_range() -> usage_collector_sdk::TimeRange {
    usage_collector_sdk::TimeRange::new(
        time::OffsetDateTime::UNIX_EPOCH,
        time::OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
    )
    .expect("a one-hour range from the epoch is a valid TimeRange")
}

/// The three parameter-carrying read methods must hand the plugin the
/// reference `types-registry` issued for the queried meter, taken from the
/// resolved declaration rather than derived. Asserted on `list`, which is
/// the one of the three whose fake records what it was dispatched.
#[tokio::test]
async fn list_dispatches_the_reference_from_the_resolved_declaration() {
    let meter = MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid meter id");
    let type_uuid = Uuid::from_u128(0xabc1_2345);
    let plugin = RecordingPlugin::new();
    let svc = service_with_declaration_uuid(
        Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
        &meter,
        type_uuid,
    );

    // The outcome is not the assertion here — `RecordingPlugin`'s
    // `list_usage_records_response` is left unprogrammed, so the call
    // answers `not_programmed` regardless. `drop` discards the `Result`
    // explicitly rather than `let _ = …`, which `clippy::let_underscore_must_use`
    // denies for a `#[must_use]` return type.
    drop(
        svc.list_usage_records(
            &authenticated_ctx(),
            meter.clone(),
            any_time_range(),
            &ODataQuery::default(),
            &[],
        )
        .await,
    );

    let dispatched = plugin.last_list_meter().expect("the plugin was dispatched");
    assert_eq!(
        dispatched.uuid, type_uuid,
        "the reference must reach the plugin"
    );
    assert_eq!(
        dispatched.id, meter,
        "the identifier must ride alongside it for the plugin's own diagnostics"
    );
}

/// Review Focus 3. A subscription names N meters and the page is one page
/// across all of them, so resolving some and dispatching those would serve
/// a page silently missing a subscribed meter's entries — indistinguishable
/// from that meter having had no traffic. The whole read must fail instead.
#[tokio::test]
async fn one_unresolvable_subscribed_meter_fails_the_whole_feed_read() {
    let good = MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid meter id");
    let absent = MeterTypeId::new(OTHER_SAMPLE_METER_TYPE_ID).expect("valid meter id");
    let plugin = Arc::new(PanickingPlugin::new(
        "read_feed_page must not be dispatched",
    ));
    let svc = service_resolving_only(plugin, &[(&good, Uuid::from_u128(1))]);

    let result = svc
        .read_usage_feed(
            &authenticated_ctx(),
            &FeedSubscription::new(vec![good, absent]).expect("non-empty subscription"),
            FeedStart::Oldest,
            None,
            Some(10),
        )
        .await;

    assert!(
        matches!(result, Err(UsageCollectorError::NotFound { .. })),
        "an undeclared meter in the subscription must fail closed, got {result:?}"
    );
}

/// The resolved reference, never the bare identifier, is what the feed read
/// path hands the plugin.
#[tokio::test]
async fn a_resolvable_subscription_reaches_the_plugin_as_references() {
    let meter = MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid meter id");
    let type_uuid = Uuid::from_u128(0xfeed_0001);
    let plugin = RecordingPlugin::new();
    let svc = service_with_declaration_uuid(
        Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
        &meter,
        type_uuid,
    );

    // The outcome is not the assertion here, for the reason the first test
    // in this module gives: `drop` discards the `Result` explicitly rather
    // than `let _ = …`, which `clippy::let_underscore_must_use` denies for a
    // `#[must_use]` return type.
    drop(
        svc.read_usage_feed(
            &authenticated_ctx(),
            &FeedSubscription::new(vec![meter.clone()]).expect("non-empty subscription"),
            FeedStart::Oldest,
            None,
            Some(10),
        )
        .await,
    );

    let dispatched = plugin
        .last_subscription()
        .expect("the plugin was dispatched");
    assert_eq!(dispatched, vec![MeterRef::new(type_uuid, meter)]);
}

/// The identity-free submission every ingestion test in this module sends.
///
/// Copied from `service_metrics_tests.rs` for the module doc's reason: that
/// module's own fixtures are private to it.
fn sample_create_record() -> CreateUsageRecord {
    CreateUsageRecord {
        entry_type: EntryType::Record,
        gts_type_id: MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid gts_type_id"),
        tenant_id: Uuid::from_u128(2),
        resource_ref: ResourceRef::new("rsc-1", "compute.vm").expect("valid resource ref"),
        subject_ref: None,
        metadata: BTreeMap::new(),
        quantity: qty("1"),
        idempotency_key: Some(IdempotencyKey::new("idem-1").expect("valid idempotency key")),
        invalidation: None,
        window_start: recent_window_start(),
        window_end: recent_window_end(),
    }
}

/// A faithful withdrawal of [`sample_create_record`]'s projection: the same
/// submission, departing only in `entry_type` and the reason it states.
fn sample_withdrawal() -> CreateUsageRecord {
    CreateUsageRecord {
        entry_type: EntryType::Invalidation,
        invalidation: Some(ReasonCode::new("emitter_defect").expect("valid reason code")),
        ..sample_create_record()
    }
}

/// Review Focus 1. Re-attachment staples an identifier the *gear* holds
/// onto a record the *plugin* returned. A plugin answering with a row for
/// another meter would otherwise have it relabelled with the queried
/// meter's identifier and served to a caller as fact. The same class of
/// store breach as `target.id != invalidation.target`, refused the same way.
#[tokio::test]
async fn a_listed_record_under_a_foreign_reference_is_an_invariant_breach() {
    let meter = MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid meter id");
    let type_uuid = Uuid::from_u128(0x0001);
    let plugin = ForeignMeterPlugin::new(Uuid::from_u128(0x9999));
    let svc = service_with_declaration_uuid(
        Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
        &meter,
        type_uuid,
    );

    let result = svc
        .list_usage_records(
            &authenticated_ctx(),
            meter,
            any_time_range(),
            &ODataQuery::default(),
            &[],
        )
        .await;

    assert!(
        matches!(result, Err(UsageCollectorError::Internal { .. })),
        "a row under another meter's reference must be refused, got {result:?}"
    );
}

/// Review Focus 4. The error arm reaches re-attachment by a different path
/// than the `Ok` arm, and is just as able to misreport a foreign row.
#[tokio::test]
async fn an_idempotency_conflict_naming_a_foreign_reference_is_an_invariant_breach() {
    let meter = MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid meter id");
    let plugin = ConflictingPlugin::with_existing_reference(Uuid::from_u128(0x9999));
    let svc = service_with_declaration_uuid(
        Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
        &meter,
        Uuid::from_u128(0x0001),
    );

    let result = svc
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await
        .expect("the batch call itself succeeds; a conflict is a per-record rejection")
        .into_iter()
        .next()
        .expect("one entry in, one slot out");

    assert!(
        matches!(result, Err(UsageCollectorError::Internal { .. })),
        "a conflict naming another meter's entry must be refused, got {result:?}"
    );
}

/// Review Focus 2. A feed page is bounded by its subscription, so an entry
/// carrying a reference the subscription never named has no identifier to
/// re-attach. It must be a typed breach, not a panic on a missing map key.
#[tokio::test]
async fn a_feed_entry_outside_the_subscription_is_an_invariant_breach() {
    let meter = MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid meter id");
    let plugin = ForeignMeterPlugin::new(Uuid::from_u128(0x9999));
    let svc = service_with_declaration_uuid(
        Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
        &meter,
        Uuid::from_u128(0x0001),
    );

    let result = svc
        .read_usage_feed(
            &authenticated_ctx(),
            &FeedSubscription::new(vec![meter]).expect("non-empty subscription"),
            FeedStart::Oldest,
            None,
            Some(10),
        )
        .await;

    assert!(
        matches!(result, Err(UsageCollectorError::Internal { .. })),
        "an entry outside the subscription must be refused, got {result:?}"
    );
}

/// The point read's reverse resolution, per spec §3.4.
#[tokio::test]
async fn a_point_read_names_the_meter_the_reverse_resolver_answers_with() {
    let meter = MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid meter id");
    let type_uuid = Uuid::from_u128(0x0001);
    let plugin = SingleRecordPlugin::new(type_uuid);
    let svc = service_with_reverse_mapping(
        Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
        &[(type_uuid, &meter)],
    );

    let record = svc
        .get_usage_record(&authenticated_ctx(), SingleRecordPlugin::ID)
        .await
        .expect("the point read resolves");

    assert_eq!(record.gts_type_id, meter);
}

/// Spec §3.4, first arm. The row exists and is the caller's; the registry
/// cannot name its meter. That is gear-side corruption, not a caller fault,
/// and in particular not a 404 — a 404 here would report a record that
/// demonstrably exists as absent.
#[tokio::test]
async fn a_point_read_whose_reference_no_longer_names_a_meter_is_internal() {
    let plugin = SingleRecordPlugin::new(Uuid::from_u128(0x0001));
    let svc =
        service_with_reverse_mapping(Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>, &[]);

    let result = svc
        .get_usage_record(&authenticated_ctx(), SingleRecordPlugin::ID)
        .await;

    assert!(
        matches!(result, Err(UsageCollectorError::Internal { .. })),
        "a reference the registry cannot name is gear-side corruption, got {result:?}"
    );
}

/// Spec §3.4, second arm.
#[tokio::test]
async fn a_point_read_against_an_unreachable_registry_is_service_unavailable() {
    let plugin = SingleRecordPlugin::new(Uuid::from_u128(0x0001));
    let svc =
        service_with_unreachable_registry(Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>);

    let result = svc
        .get_usage_record(&authenticated_ctx(), SingleRecordPlugin::ID)
        .await;

    assert!(
        matches!(result, Err(UsageCollectorError::ServiceUnavailable { .. })),
        "an unreachable registry is transient, got {result:?}"
    );
}

/// Review Focus 5. `collapse_deny_to_not_found` runs on the PDP decision,
/// before dispatch; the resolver runs after a row comes back. A regression
/// that reordered them would turn this surface into the existence oracle
/// the collapse exists to prevent.
#[tokio::test]
async fn a_denied_point_read_is_not_found_and_reaches_no_resolver() {
    let plugin = Arc::new(PanickingPlugin::new("a denied read must not dispatch"));
    let svc = service_denying_with_panicking_resolver(plugin);

    let result = svc
        .get_usage_record(&authenticated_ctx(), Uuid::from_u128(0x1234))
        .await;

    assert!(
        matches!(result, Err(UsageCollectorError::NotFound { .. })),
        "a PDP deny must still collapse to not-found, got {result:?}"
    );
}

/// The invalidation-target path compares references and must never reach
/// the reverse resolver — a regression here would put a types-registry
/// dependency on the ingestion path, the worst outcome this slice could
/// produce.
#[tokio::test]
async fn resolving_an_invalidation_target_reaches_no_reverse_resolver() {
    let meter = MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid meter id");
    let type_uuid = Uuid::from_u128(0x0001);
    let plugin = TargetHoldingPlugin::new(type_uuid);
    let svc = service_with_panicking_reverse_resolver(
        Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
        &meter,
        type_uuid,
    );

    let result = svc
        .create_usage_records(&authenticated_ctx(), vec![sample_withdrawal()])
        .await
        .expect("the batch call itself succeeds")
        .into_iter()
        .next()
        .expect("one entry in, one slot out");

    assert!(
        result.is_ok(),
        "the withdrawal resolves without reverse resolution: {result:?}"
    );
}

/// Whole-branch review Fix 2. `settle_dispatched`'s `Ok` arm
/// (`service.rs:331`) is the one guard where relabelling a record is also a
/// persistence claim: a plugin that *accepts* a write and answers with a
/// persisted entry under a different meter's reference must still be
/// refused, because the caller is told "this was stored, under this meter."
///
/// No existing double reached this arm before this test: `HappyPathPlugin`
/// routes every programmed outcome through `stored_outcome`, which
/// forcibly re-stamps `meter.uuid`, so no programmed outcome can be
/// foreign; `ForeignMeterPlugin` answers a foreign entry unconditionally,
/// but was previously only ever wired into the three read-path tests above.
/// This wires it into the write path instead.
#[tokio::test]
async fn a_created_record_under_a_foreign_reference_is_an_invariant_breach() {
    let meter = MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid meter id");
    let plugin = ForeignMeterPlugin::new(Uuid::from_u128(0x9999));
    let svc = service_with_declaration_uuid(
        Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
        &meter,
        Uuid::from_u128(0x0001),
    );

    let result = svc
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await
        .expect("the batch call itself succeeds; the breach is per-entry")
        .into_iter()
        .next()
        .expect("one entry in, one slot out");

    assert!(
        matches!(result, Err(UsageCollectorError::Internal { .. })),
        "a record stored under another meter's reference must be refused, got {result:?}"
    );
}

/// The batch twin of the test above. `create_usage_records` reaches the
/// same `settle_dispatched` arm through `dispatch_eligible_entries`, and
/// reports the breach per-entry rather than failing the call as a whole.
#[tokio::test]
async fn a_batch_created_record_under_a_foreign_reference_is_an_invariant_breach() {
    let meter = MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid meter id");
    let plugin = ForeignMeterPlugin::new(Uuid::from_u128(0x9999));
    let svc = service_with_declaration_uuid(
        Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
        &meter,
        Uuid::from_u128(0x0001),
    );

    let result = svc
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await
        .expect("the batch call itself succeeds; the breach is per-entry");

    assert_eq!(result.len(), 1, "one submitted record, one outcome slot");
    assert!(
        matches!(&result[0], Err(UsageCollectorError::Internal { .. })),
        "a record stored under another meter's reference must be refused, got {result:?}"
    );
}

/// Task 3's own addition. The deleted single-emit path resolved a
/// withdrawal's target with an inline per-record lookup
/// (`create_usage_record_inner`); the batch path resolves it instead through
/// `resolve_invalidation_targets`. Nothing before this test exercised that
/// the two agree for a batch of exactly one, and once the single-emit path
/// is deleted (a later task in this plan), the batch path is the only one
/// left to get this right.
///
/// The measurement is persisted first, through the same one-entry-batch
/// surface the withdrawal itself goes through, so the expected target is an
/// `id` the batch path assigned — not one this test derives independently
/// and could get wrong in the same way a regression in the gear would.
#[tokio::test]
async fn a_batch_withdrawal_resolves_to_its_measurements_persisted_id() {
    let meter = MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid meter id");
    let type_uuid = Uuid::from_u128(0x0001);
    let plugin = TargetHoldingPlugin::new(type_uuid);
    let svc = service_with_panicking_reverse_resolver(
        Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
        &meter,
        type_uuid,
    );
    let ctx = authenticated_ctx();

    let measurement = svc
        .create_usage_records(&ctx, vec![sample_create_record()])
        .await
        .expect("the batch call itself succeeds")
        .into_iter()
        .next()
        .expect("one entry in, one slot out")
        .expect("the measurement is accepted and persisted");

    let withdrawal = svc
        .create_usage_records(&ctx, vec![sample_withdrawal()])
        .await
        .expect("the batch call itself succeeds")
        .into_iter()
        .next()
        .expect("one entry in, one slot out")
        .expect("the withdrawal resolves its target and is accepted");

    let resolved_target = withdrawal
        .invalidation
        .expect("a persisted withdrawal carries the target it resolved")
        .target;

    assert_eq!(
        resolved_target, measurement.id,
        "a one-entry batch's withdrawal must resolve to the same target the \
         measurement it withdraws was persisted under -- the single-emit path's \
         inline lookup and the batch path's `resolve_invalidation_targets` must \
         agree on this identifier, got {resolved_target} vs measurement id {}",
        measurement.id
    );
}
