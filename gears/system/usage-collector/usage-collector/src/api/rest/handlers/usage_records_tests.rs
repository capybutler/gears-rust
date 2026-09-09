//! Handler-level unit tests for the foundation
//! `/usage-collector/v1/records` create and read surface.
//!
//! Scope: pin handler-shaped concerns the SDK error-mapping and service
//! tests cannot reach. Specifically, the create handler lifts per-record
//! `gts_type_id` validation failures into the canonical `InvalidArgument`
//! `Problem` envelope (`field_violations[0].reason="INVALID_BASE_GTS_ID"`)
//! WITHOUT failing the surrounding batch, and the point-lookup handler
//! lifts a malformed `uuid` path segment into the canonical
//! `InvalidArgument` envelope before reaching the service.
//!
//! Out of scope here:
//!
//! * Wire-shape / DTO conversions — pinned in
//!   [`crate::api::rest::dto::tests`].
//! * Service-layer create / read — pinned in
//!   [`crate::domain::service::service_tests`].

use std::sync::Arc;
use toolkit_gts::gts_id;

use axum::Json;
use axum::extract::{Extension, Path};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use time::OffsetDateTime;
use toolkit::client_hub::ClientHub;
use toolkit_security::{SecurityContext, pep_properties};
use uuid::Uuid;

use super::{handle_create_usage_records, handle_get_usage_record};
use crate::api::rest::dto::{CreateUsageRecordRequest, CreateUsageRecordsRequest, ResourceRefDto};
use crate::domain::Service;
use crate::domain::test_support::{
    CountingPermitResolver, CountingUnreachableResolver, HappyPathPlugin, ServiceFixture,
    authenticated_ctx, enforcer_for, fake_declaration_source_with_fold, recent_window_end,
    recent_window_start, recording_plugin_resolver,
};

/// Wire a `Service` against a counting unreachable-PDP resolver and an
/// empty `ClientHub` (no plugin / no registry). Any handler path that
/// reaches the service surfaces 503 — but `CountingUnreachableResolver`
/// also records *that* it was reached, so short-circuit tests below can
/// assert `resolver.calls() == 0` as direct evidence the service path was
/// not entered.
fn service_with_sentinel_pdp() -> (Arc<Service>, Arc<CountingUnreachableResolver>) {
    let hub = Arc::new(ClientHub::new());
    let resolver = CountingUnreachableResolver::new();
    let enforcer = enforcer_for(Arc::clone(&resolver) as _);
    let service = Arc::new(Service::new(hub, "cyberfabric".to_owned(), enforcer));
    (service, resolver)
}

#[tokio::test]
async fn create_with_only_bad_gts_type_id_records_short_circuits_to_207_without_calling_service() {
    // Every record carries a bad-prefix `gts_type_id`, so every record is
    // rejected at the handler boundary BEFORE the service is invoked. We
    // pair the service with a `CountingUnreachableResolver` so the test
    // can pin the short-circuit two ways: the response status is 207
    // (not 503), AND the resolver was never invoked (`calls() == 0`).
    let (service, resolver) = service_with_sentinel_pdp();

    let req = CreateUsageRecordsRequest {
        records: vec![CreateUsageRecordRequest {
            gts_type_id: "not-a-valid-prefix".to_owned(),
            tenant_id: Uuid::new_v4(),
            resource_ref: ResourceRefDto {
                resource_id: "rsc-1".to_owned(),
                resource_type: "compute.vm".to_owned(),
            },
            subject_ref: None,
            metadata: std::collections::BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: "idem-bad-prefix-1".to_owned(),
            invalidates: None,
            reason_code: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }],
    };

    let response = handle_create_usage_records(
        Extension(SecurityContext::anonymous()),
        Extension(service),
        Json(req),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::MULTI_STATUS,
        "all-rejected batch MUST surface as 207 Multi-Status",
    );
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.contains("json"),
        "207 envelope MUST be JSON (got `{content_type}`)",
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");
    let results = body
        .get("results")
        .and_then(serde_json::Value::as_array)
        .expect("response carries a `results` array");
    assert_eq!(results.len(), 1);
    let item = &results[0];
    assert_eq!(
        item.get("outcome").and_then(serde_json::Value::as_str),
        Some("rejected"),
        "bad-prefix record MUST surface as `outcome: rejected`",
    );
    let problem = item.get("error").expect("rejected item carries `error`");
    let violation = problem
        .get("context")
        .and_then(|c| c.get("field_violations"))
        .and_then(|fv| fv.as_array())
        .and_then(|arr| arr.first())
        .expect("rejected error carries field_violations[0]");
    assert_eq!(
        violation.get("field").and_then(serde_json::Value::as_str),
        Some("gts_type_id"),
        "per-record bad-prefix error MUST carry field_violations[0].field = gts_type_id",
    );
    assert_eq!(
        violation.get("reason").and_then(serde_json::Value::as_str),
        Some("INVALID_BASE_GTS_ID"),
        "per-record bad-prefix error MUST carry field_violations[0].reason = INVALID_BASE_GTS_ID",
    );
    assert_eq!(
        resolver.calls(),
        0,
        "handler MUST short-circuit before dispatching to the service \
         (resolver MUST NOT be touched on the all-rejected path)",
    );
}

/// Closes the id-length window `MeterTypeId` replaces `UsageTypeGtsId` to
/// fix: `gts-id` (the old boundary type, deleted with this task) capped a
/// whole identifier at 1024 bytes, but `MeterTypeId` (the new, sole boundary
/// type) caps at 512 — so an identifier between those two bounds used to
/// pass the record DTO's conversion and only fail deep inside the aggregate
/// path's `meter_type_id_of` bridge, surfacing as a host-invariant `Internal`
/// (500) rather than the `InvalidArgument` (400) an over-long identifier
/// should be. `MeterTypeId` is now the parameter type at construction, so
/// the same identifier is rejected once, at the wire boundary, before the
/// service (and therefore the PDP) is ever reached.
///
/// The identifier below is otherwise grammatically valid — a genuine
/// `vendor.package.namespace.type.v1` derivation segment, just with a long
/// `type` token — so this test is falsifiable: pin `MAX_METER_TYPE_ID_LEN`
/// to something larger than 520 and `MeterTypeId::new` accepts it, the
/// handler dispatches to the service, and every assertion below (status,
/// `field_violations`, and `resolver.calls() == 0`) breaks.
#[tokio::test]
async fn create_with_an_over_long_gts_type_id_is_rejected_as_invalid_argument_not_500() {
    let (service, resolver) = service_with_sentinel_pdp();

    // 520 bytes: over MeterTypeId's 512-byte cap, comfortably under the old
    // gts-id boundary's 1024-byte cap.
    let over_long_gts_type_id = format!(
        "{}cf.mini_chat._.{}.v1~",
        gts_id!("cf.core.uc.usage_record.v1~"),
        "a".repeat(470),
    );
    assert!(
        over_long_gts_type_id.len() > 512,
        "test premise: the identifier must exceed MeterTypeId's cap"
    );
    assert!(
        over_long_gts_type_id.len() < 1024,
        "test premise: the identifier must stay under gts-id's old cap"
    );

    let req = CreateUsageRecordsRequest {
        records: vec![CreateUsageRecordRequest {
            gts_type_id: over_long_gts_type_id,
            tenant_id: Uuid::new_v4(),
            resource_ref: ResourceRefDto {
                resource_id: "rsc-1".to_owned(),
                resource_type: "compute.vm".to_owned(),
            },
            subject_ref: None,
            metadata: std::collections::BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: "idem-over-long-1".to_owned(),
            invalidates: None,
            reason_code: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }],
    };

    let response = handle_create_usage_records(
        Extension(SecurityContext::anonymous()),
        Extension(service),
        Json(req),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::MULTI_STATUS,
        "an over-long gts_type_id MUST reject as a per-record 400, not a 500 \
         (all-rejected single-record batch surfaces as 207)",
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");
    let results = body
        .get("results")
        .and_then(serde_json::Value::as_array)
        .expect("response carries a `results` array");
    assert_eq!(results.len(), 1);
    let problem = results[0]
        .get("error")
        .expect("rejected item carries `error`");
    assert_eq!(
        problem.get("status").and_then(serde_json::Value::as_u64),
        Some(400),
        "over-long gts_type_id MUST lift to InvalidArgument (400), never Internal (500)",
    );
    let violation = problem
        .get("context")
        .and_then(|c| c.get("field_violations"))
        .and_then(|fv| fv.as_array())
        .and_then(|arr| arr.first())
        .expect("rejected error carries field_violations[0]");
    assert_eq!(
        violation.get("field").and_then(serde_json::Value::as_str),
        Some("gts_type_id"),
    );
    assert_eq!(
        violation.get("reason").and_then(serde_json::Value::as_str),
        Some("INVALID_BASE_GTS_ID"),
    );
    let description = violation
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        description.contains("512 bytes"),
        "the rejection detail MUST name the length cap that was breached \
         (got `{description}`)",
    );

    assert_eq!(
        resolver.calls(),
        0,
        "the over-long identifier MUST be rejected before the service (and \
         therefore the PDP) is ever reached - closing the window by \
         construction, not by a deep bridge-conversion check",
    );
}

// ---------------------------------------------------------------------------
// Happy-path coverage.
//
// The short-circuit tests above pin the rejection wiring; these tests pin the
// success-side composition (request → service → DTO conversion → wire body).
// Specifically they guard against a regression that swaps the persisted
// record with the input record inside [`super::UsageRecordDto::from`]: the
// service-returned record carries a UUID DIFFERENT from the submitted one,
// and the test asserts the wire body echoes the service-returned UUID.
// ---------------------------------------------------------------------------

use std::collections::BTreeMap;
use usage_collector_sdk::{
    IdempotencyKey, Invalidation, MeterTypeId, ReasonCode, RecordOrigin, ResourceRef, UsageRecord,
    derive_usage_record_id,
};

const HAPPY_RECORD_GTS_ID: &str =
    gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

/// The reason every withdrawal fixture states. Open vocabulary — the gear
/// records the emitter's stated intent and infers nothing from it.
const HAPPY_REASON_CODE: &str = "emitter_duplicate";

/// The exclusive end of the fixture covered period, one hour after the
/// epoch start. Distinct from the start so a test that confused the two
/// bounds fails rather than passing by symmetry.
const EPOCH_PLUS_ONE_HOUR: OffsetDateTime =
    OffsetDateTime::UNIX_EPOCH.saturating_add(time::Duration::hours(1));

fn sample_persisted_record(id: Uuid, tenant_id: Uuid) -> UsageRecord {
    sample_persisted_entry(id, tenant_id, None)
}

/// Wire spelling of the mandatory raw-path range's inclusive lower bound.
const RANGE_FROM: &str = "1970-01-01T00:00:00Z";
/// Wire spelling of its exclusive upper bound. One hour later, so a
/// handler that swapped or reused a bound fails rather than passing by
/// symmetry.
const RANGE_TO: &str = "1970-01-01T01:00:00Z";

/// The mandatory `from` / `to` query parameters every raw-path read
/// carries, plus whatever `extra` the call site needs.
///
/// The range is mandatory on every list request, so every call site would
/// otherwise repeat the pair — and a call site that assembled the two
/// halves itself would be longer than the literals it replaced. Tests
/// whose subject *is* a missing, duplicated or malformed bound build their
/// parameter list by hand instead.
fn list_params(extra: &[(&str, &str)]) -> Vec<(String, String)> {
    extra
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .chain([
            ("from".to_owned(), RANGE_FROM.to_owned()),
            ("to".to_owned(), RANGE_TO.to_owned()),
        ])
        .collect()
}

/// The typed range `list_params` spells on the wire.
fn list_range() -> usage_collector_sdk::TimeRange {
    usage_collector_sdk::TimeRange::new(OffsetDateTime::UNIX_EPOCH, EPOCH_PLUS_ONE_HOUR)
        .expect("a one-hour range from the epoch is valid")
}

/// The fingerprint the service binds a continuation to for a request
/// carrying `query`'s `$filter` over the standard [`list_params`] range,
/// against the happy-path meter and with no metadata filter.
///
/// A continuation whose `f` is anything else — absent included — is
/// refused, so a full-stack cursor test has to mint the real value. It
/// comes from the domain rather than being restated here: a second
/// spelling at the edge is the very defect the fingerprint's single owner
/// exists to prevent.
fn list_fingerprint(query: &toolkit_odata::ODataQuery) -> String {
    let meter = usage_collector_sdk::MeterTypeId::new(HAPPY_RECORD_GTS_ID)
        .expect("the happy-path gts_type_id is valid");
    crate::domain::query::read_fingerprint(&meter, list_range(), query, &[])
}

/// The same range in the aggregate path's carrier — its request body.
fn range_body() -> crate::api::rest::dto::TimeRangeDto {
    crate::api::rest::dto::TimeRangeDto {
        from: OffsetDateTime::UNIX_EPOCH,
        to: EPOCH_PLUS_ONE_HOUR,
    }
}

/// The `field` of the first `field_violations[]` entry in a canonical
/// `Problem` response body, or `None` when the body carries none.
///
/// Read off the wire rather than off a `CanonicalError`, because a handler
/// test's subject is what a caller receives: a 400 that names no parameter
/// leaves the caller diffing their request against the docs.
async fn first_violation_field(response: axum::response::Response) -> Option<String> {
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("Problem is JSON");
    body.get("context")?
        .get("field_violations")?
        .as_array()?
        .first()?
        .get("field")?
        .as_str()
        .map(str::to_owned)
}

/// The same payload carrying a withdrawal of `target`, so its derived
/// entry type is `invalidation`. A faithful copy of
/// [`sample_persisted_record`] in every compared field, which is what lets
/// a full-stack test submit it against that record as its target.
fn sample_persisted_invalidation(id: Uuid, tenant_id: Uuid, target: Uuid) -> UsageRecord {
    sample_persisted_entry(
        id,
        tenant_id,
        Some(Invalidation {
            target,
            reason: ReasonCode::new(HAPPY_REASON_CODE).expect("valid reason code"),
        }),
    )
}

fn sample_persisted_entry(
    id: Uuid,
    tenant_id: Uuid,
    invalidation: Option<Invalidation>,
) -> UsageRecord {
    UsageRecord {
        id,
        gts_type_id: MeterTypeId::new(HAPPY_RECORD_GTS_ID).expect("valid gts_type_id"),
        tenant_id,
        resource_ref: ResourceRef::new("rsc-happy", "compute.vm").expect("valid resource ref"),
        subject_ref: None,
        metadata: BTreeMap::new(),
        value: rust_decimal::Decimal::from(1),
        idempotency_key: IdempotencyKey::new("idem-happy").expect("valid idempotency key"),
        origin: RecordOrigin::Live,
        invalidation,
        window_start: recent_window_start(),
        window_end: recent_window_end(),
    }
}

#[tokio::test]
async fn create_records_happy_path_wire_body_reflects_service_returned_record() {
    // Wire the service against a permit-by-default PDP and a
    // `HappyPathPlugin` that returns one `Ok(persisted_record)` from
    // `create_usage_records` where `persisted_record.id` is DIFFERENT from
    // the gateway-derived dispatched record's id (the meter's declaration
    // resolves via the fixture's fake source, so semantics validation
    // passes without a plugin-owned catalog row).
    // The handler then emits 200 OK; the wire body's `records[0].record.id`
    // MUST be the persisted id — proving the handler composes the
    // response from the SERVICE-RETURNED record, not from the dispatched one.
    let plugin = HappyPathPlugin::new();
    let tenant_id = Uuid::from_u128(2);
    let gts_id = MeterTypeId::new(HAPPY_RECORD_GTS_ID).expect("valid gts_type_id");
    let idempotency_key = IdempotencyKey::new("idem-happy").expect("valid idempotency key");
    let derived_id = derive_usage_record_id(
        tenant_id,
        &gts_id,
        &idempotency_key,
        recent_window_start(),
        recent_window_end(),
    );
    let persisted_uuid = Uuid::new_v4();
    assert_ne!(derived_id, persisted_uuid, "test premise");
    plugin.set_create_records(vec![Ok(sample_persisted_record(persisted_uuid, tenant_id))]);

    let service = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build(
            Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
            "test.handler.create_records.happy.v1",
        );

    let req = CreateUsageRecordsRequest {
        records: vec![CreateUsageRecordRequest {
            gts_type_id: HAPPY_RECORD_GTS_ID.to_owned(),
            tenant_id,
            resource_ref: ResourceRefDto {
                resource_id: "rsc-happy".to_owned(),
                resource_type: "compute.vm".to_owned(),
            },
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: "idem-happy".to_owned(),
            invalidates: None,
            reason_code: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }],
    };

    let response = handle_create_usage_records(
        Extension(authenticated_ctx()),
        Extension(service),
        Json(req),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "all-accepted happy path MUST surface as 200 OK, not 207",
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");
    let results = body
        .get("results")
        .and_then(serde_json::Value::as_array)
        .expect("response carries a `results` array");
    assert_eq!(results.len(), 1);
    let item = &results[0];
    assert_eq!(
        item.get("outcome").and_then(serde_json::Value::as_str),
        Some("accepted"),
    );
    let record = item.get("record").expect("accepted item carries `record`");
    assert_eq!(
        record.get("id").and_then(serde_json::Value::as_str),
        Some(persisted_uuid.to_string().as_str()),
        "wire body MUST echo the service-returned (persisted) UUID, NOT the \
         gateway-derived dispatched UUID",
    );
    assert_eq!(
        record.get("entry_type").and_then(serde_json::Value::as_str),
        Some("record"),
        "wire body MUST project the derived entry type to lowercase string \
         `record` (a regression that flipped this to e.g. `\"RECORD\"` or the \
         empty string would silently break OAS-typed clients)",
    );

    // Sanity: the plugin was actually invoked with the gateway-derived id.
    let forwarded = plugin
        .last_create_records_input()
        .expect("plugin received the eligible batch");
    assert_eq!(forwarded.len(), 1);
    assert_eq!(forwarded[0].id, derived_id);
}

#[tokio::test]
async fn create_stamps_derived_id() {
    // The gateway MUST derive the dispatched record's id from the 5-tuple
    // dedup identity
    // `(tenant_id, gts_type_id, idempotency_key, window_start, window_end)`
    // rather than accept a
    // caller-chosen value — pin both that the dispatched id matches
    // `derive_usage_record_id` AND that a same-key resubmit derives the
    // identical id (determinism).
    let plugin = HappyPathPlugin::new();
    let tenant_id = Uuid::from_u128(2);
    let gts_id = MeterTypeId::new(HAPPY_RECORD_GTS_ID).expect("valid gts_type_id");
    let idempotency_key = IdempotencyKey::new("idem-derive-1").expect("valid idempotency key");
    let expected = derive_usage_record_id(
        tenant_id,
        &gts_id,
        &idempotency_key,
        recent_window_start(),
        recent_window_end(),
    );

    let service = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build(
            Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
            "test.handler.create_records.derive_id.v1",
        );

    let build_req = || CreateUsageRecordsRequest {
        records: vec![CreateUsageRecordRequest {
            gts_type_id: HAPPY_RECORD_GTS_ID.to_owned(),
            tenant_id,
            resource_ref: ResourceRefDto {
                resource_id: "rsc-happy".to_owned(),
                resource_type: "compute.vm".to_owned(),
            },
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: "idem-derive-1".to_owned(),
            invalidates: None,
            reason_code: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }],
    };

    // First submission.
    plugin.set_create_records(vec![Ok(sample_persisted_record(Uuid::new_v4(), tenant_id))]);
    let response = handle_create_usage_records(
        Extension(authenticated_ctx()),
        Extension(Arc::clone(&service)),
        Json(build_req()),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    let forwarded = plugin
        .last_create_records_input()
        .expect("plugin received the eligible batch");
    assert_eq!(forwarded.len(), 1);
    assert_eq!(
        forwarded[0].id, expected,
        "gateway MUST stamp the dispatched record's id with \
         derive_usage_record_id(tenant_id, gts_type_id, idempotency_key, \
         window_start, window_end)",
    );

    // Same-key resubmit: the derived id MUST be identical.
    plugin.set_create_records(vec![Ok(sample_persisted_record(Uuid::new_v4(), tenant_id))]);
    let response = handle_create_usage_records(
        Extension(authenticated_ctx()),
        Extension(service),
        Json(build_req()),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    let forwarded_again = plugin
        .last_create_records_input()
        .expect("plugin received the eligible batch");
    assert_eq!(forwarded_again.len(), 1);
    assert_eq!(
        forwarded_again[0].id, expected,
        "a same dedup-key resubmit MUST derive the identical id",
    );
}

#[tokio::test]
async fn create_same_key_different_covered_periods_derives_distinct_ids() {
    // `cpt-cf-usage-collector-adr-record-identity-derivation`: both
    // covered-period bounds are part of the identity. Three
    // submissions sharing `(tenant_id, gts_type_id, idempotency_key)` but
    // covering different periods MUST be dispatched with DISTINCT ids — that
    // is what lets one stable per-meter key cover many periods instead of
    // collapsing them onto one entry. The third submission moves only
    // `window_end`, so a derivation that read the start alone would collide
    // it with the first.
    let plugin = HappyPathPlugin::new();
    let tenant_id = Uuid::from_u128(2);
    let service = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build(
            Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
            "test.handler.create_records.distinct_periods.v1",
        );

    let build_req =
        |window_start: OffsetDateTime, window_end: OffsetDateTime| CreateUsageRecordsRequest {
            records: vec![CreateUsageRecordRequest {
                gts_type_id: HAPPY_RECORD_GTS_ID.to_owned(),
                tenant_id,
                resource_ref: ResourceRefDto {
                    resource_id: "rsc-happy".to_owned(),
                    resource_type: "compute.vm".to_owned(),
                },
                subject_ref: None,
                metadata: BTreeMap::new(),
                value: rust_decimal::Decimal::from(1),
                idempotency_key: "idem-distinct".to_owned(),
                invalidates: None,
                reason_code: None,
                window_start,
                window_end,
            }],
        };

    let mut dispatched_ids = Vec::new();
    for (window_start, window_end) in [
        (recent_window_start(), recent_window_end()),
        (
            recent_window_start() + time::Duration::seconds(1),
            recent_window_end(),
        ),
        (
            recent_window_start(),
            recent_window_end() + time::Duration::seconds(1),
        ),
    ] {
        plugin.set_create_records(vec![Ok(sample_persisted_record(Uuid::new_v4(), tenant_id))]);
        let response = handle_create_usage_records(
            Extension(authenticated_ctx()),
            Extension(Arc::clone(&service)),
            Json(build_req(window_start, window_end)),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        dispatched_ids.push(
            plugin
                .last_create_records_input()
                .expect("batch dispatched")[0]
                .id,
        );
    }

    let distinct: std::collections::HashSet<Uuid> = dispatched_ids.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        dispatched_ids.len(),
        "one idempotency key + three distinct covered periods MUST derive three \
         distinct ids; got {dispatched_ids:?}",
    );
}

/// A covered-period bound in its wire spelling.
///
/// The wire-built fixtures format [`recent_window`]'s bounds through this
/// rather than carrying a date literal: a literal sits inside the live
/// path's 48-hour past tolerance only until it doesn't, and a fixture
/// period that quietly stopped being plausible is the exact defect this
/// re-base cleared.
fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .expect("a UTC instant formats as RFC 3339")
}

/// A one-record batch request built from the wire, so the RFC 3339 codec
/// (not a Rust `OffsetDateTime` literal) decides what the period bounds
/// become. `time::Time` cannot represent second 60 at all, so a leap-second
/// bound is only expressible this way.
fn create_request_json(window_start: &str, window_end: &str) -> serde_json::Value {
    serde_json::json!({
        "records": [{
            "gts_type_id": HAPPY_RECORD_GTS_ID,
            "tenant_id": Uuid::from_u128(2).to_string(),
            "resource_ref": { "resource_id": "rsc-happy", "resource_type": "compute.vm" },
            "value": "1",
            "idempotency_key": "idem-period",
            "window_start": window_start,
            "window_end": window_end,
        }],
    })
}

/// Dispatches a wire-built one-record batch and returns the response's
/// status plus the single `results[0]` entry.
async fn dispatch_one_record_batch(
    suffix: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req: CreateUsageRecordsRequest =
        serde_json::from_value(body).expect("the request body deserializes");
    let plugin = HappyPathPlugin::new();
    // Programmed unconditionally: a record whose covered period is rejected
    // never reaches the SPI, so the response goes unused there — but an
    // unprogrammed plugin would fail the whole batch with a 503 and hide
    // whichever per-record outcome the test is actually about.
    let tenant_id = Uuid::from_u128(2);
    plugin.set_create_records(vec![Ok(sample_persisted_record(Uuid::new_v4(), tenant_id))]);
    let service = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build(
            Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
            suffix,
        );
    let response = handle_create_usage_records(
        Extension(authenticated_ctx()),
        Extension(service),
        Json(req),
    )
    .await
    .into_response();
    let status = response.status();
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");
    let item = body
        .get("results")
        .and_then(serde_json::Value::as_array)
        .and_then(|arr| arr.first())
        .cloned()
        .expect("response carries results[0]");
    (status, item)
}

/// Reads `field_violations[0].field` off a `rejected` batch entry.
fn rejected_violation_field(item: &serde_json::Value) -> String {
    assert_eq!(
        item.get("outcome").and_then(serde_json::Value::as_str),
        Some("rejected"),
        "expected a rejected entry; got {item:?}",
    );
    item.get("error")
        .and_then(|problem| problem.get("context"))
        .and_then(|c| c.get("field_violations"))
        .and_then(serde_json::Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|v| v.get("field"))
        .and_then(serde_json::Value::as_str)
        .expect("rejected entry carries field_violations[0].field")
        .to_owned()
}

/// Reads `field_violations[0].reason` off a `rejected` batch entry.
///
/// The field alone says which half of the request is at fault; the reason
/// is what a caller dispatches on, so a test that pins only the field
/// stays green when the condition is reclassified.
fn rejected_violation_reason(item: &serde_json::Value) -> String {
    item.get("error")
        .and_then(|problem| problem.get("context"))
        .and_then(|c| c.get("field_violations"))
        .and_then(serde_json::Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|v| v.get("reason"))
        .and_then(serde_json::Value::as_str)
        .expect("rejected entry carries field_violations[0].reason")
        .to_owned()
}

/// A one-record batch body carrying whatever correction fields `extra`
/// names, so a half-shape is expressible on the wire. It is not
/// expressible in the domain — `CreateUsageRecord` carries the pair as one
/// field — which is exactly why the fold point is the only place that can
/// refuse it.
fn create_request_json_with(extra: &serde_json::Value) -> serde_json::Value {
    let mut record = serde_json::json!({
        "gts_type_id": HAPPY_RECORD_GTS_ID,
        "tenant_id": Uuid::from_u128(2).to_string(),
        "resource_ref": { "resource_id": "rsc-happy", "resource_type": "compute.vm" },
        "value": "1",
        "idempotency_key": "idem-withdrawal",
        "window_start": rfc3339(recent_window_start()),
        "window_end": rfc3339(recent_window_end()),
    });
    let obj = record.as_object_mut().expect("object");
    for (k, v) in extra.as_object().expect("extra is an object") {
        obj.insert(k.clone(), v.clone());
    }
    serde_json::json!({ "records": [record] })
}

#[tokio::test]
async fn a_reference_without_a_reason_is_rejected_naming_the_missing_half() {
    // The wire contract states this as `dependentRequired`, and
    // `record_request_into_domain` is the only place on this path that can
    // enforce it: past the fold the pair is one `Invalidation`, so a
    // half-shape does not exist to be re-checked. The diagnostic names the
    // half the caller has to add, not the half they sent.
    let (status, item) = dispatch_one_record_batch(
        "test.handler.create_records.reference_without_reason.v1",
        create_request_json_with(&serde_json::json!({
            "invalidates": Uuid::from_u128(77).to_string(),
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::MULTI_STATUS,
        "an all-rejected batch MUST surface as 207 Multi-Status",
    );
    assert_eq!(rejected_violation_field(&item), "reason_code");
    assert_eq!(
        rejected_violation_reason(&item),
        "INVALIDATION_REFERENCE_INCOMPLETE",
    );
}

#[tokio::test]
async fn a_reason_without_a_reference_is_rejected_naming_the_missing_half() {
    // The other half of the same rule. Both arms are pinned because the
    // fold is a four-arm match and an implementation that folded only one
    // direction would pass the sibling above.
    let (status, item) = dispatch_one_record_batch(
        "test.handler.create_records.reason_without_reference.v1",
        create_request_json_with(&serde_json::json!({ "reason_code": HAPPY_REASON_CODE })),
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert_eq!(rejected_violation_field(&item), "invalidates");
    assert_eq!(
        rejected_violation_reason(&item),
        "INVALIDATION_REFERENCE_INCOMPLETE",
    );
}

#[tokio::test]
async fn neither_correction_field_is_an_ordinary_record() {
    // The complement the two rejections above need: a submission carrying
    // neither half folds to `None` and is accepted. Without it, a fold that
    // rejected every submission would pass both of them.
    let (status, item) = dispatch_one_record_batch(
        "test.handler.create_records.no_correction_fields.v1",
        create_request_json_with(&serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        item.get("outcome").and_then(serde_json::Value::as_str),
        Some("accepted"),
        "a submission carrying neither correction field is an ordinary \
         record; got {item:?}",
    );
}

#[tokio::test]
async fn a_malformed_reason_code_is_rejected_per_record() {
    // The fold builds the reason through `ReasonCode::new`, the same way
    // the sibling newtype conversions in `record_request_into_domain`
    // build theirs, so an unvalidated code cannot reach a storage plugin.
    // An empty code is the cheapest witness: it is refused by the newtype
    // and never by the domain, which sees only the validated form.
    let (status, item) = dispatch_one_record_batch(
        "test.handler.create_records.malformed_reason_code.v1",
        create_request_json_with(&serde_json::json!({
            "invalidates": Uuid::from_u128(78).to_string(),
            "reason_code": "",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert_eq!(
        rejected_violation_field(&item),
        "reason_code",
        "the rejection MUST attribute to the offending field, not to the \
         reference beside it",
    );
    assert_eq!(
        rejected_violation_reason(&item),
        "VALIDATION",
        "a malformed code is a newtype validation failure, NOT the \
         both-or-neither rule: the pair is complete here and only its \
         content is bad",
    );
}

#[tokio::test]
async fn an_unfaithful_copy_is_rejected_naming_the_field_that_differs() {
    // End to end, past the fold: both halves are present, so the fold
    // builds an `Invalidation` and the gateway resolves the target. The
    // submission departs from it in `value`, and the diagnostic is the
    // deliverable — a rejection saying only "mismatch" leaves the emitter
    // diffing two payloads by hand.
    let tenant_id = Uuid::from_u128(2);
    let target_uuid = Uuid::from_u128(0x4242);
    let plugin = HappyPathPlugin::new();
    plugin.set_get_record_for(target_uuid, sample_persisted_record(target_uuid, tenant_id));
    plugin.set_create_records(vec![Ok(sample_persisted_record(Uuid::new_v4(), tenant_id))]);
    let service = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build(
            Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
            "test.handler.create_records.unfaithful_copy.v1",
        );

    // The fixture target carries `value: 1`; this submission says 2. Every
    // other compared field matches, so the rejection can only be about the
    // quantity.
    let body = create_request_json_with(&serde_json::json!({
        "invalidates": target_uuid.to_string(),
        "reason_code": HAPPY_REASON_CODE,
        "value": "2",
    }));
    let req: CreateUsageRecordsRequest =
        serde_json::from_value(body).expect("the request body deserializes");

    let response = handle_create_usage_records(
        Extension(authenticated_ctx()),
        Extension(service),
        Json(req),
    )
    .await
    .into_response();
    let status = response.status();
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");
    let item = body
        .get("results")
        .and_then(serde_json::Value::as_array)
        .and_then(|arr| arr.first())
        .cloned()
        .expect("response carries results[0]");

    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert_eq!(
        rejected_violation_field(&item),
        "value",
        "the rejection MUST name the field that differs, not the reference",
    );
    assert_eq!(
        rejected_violation_reason(&item),
        "INVALIDATION_FIELD_MISMATCH",
    );
}

#[tokio::test]
async fn a_faithful_copy_reaches_the_plugin() {
    // The complement of the mismatch above, and the pin that the fold
    // actually builds an `Invalidation` rather than dropping the pair: the
    // record the gateway dispatches carries the caller's target and reason.
    // A fold that silently discarded both halves would still be accepted
    // here, and only this assertion catches it.
    let tenant_id = Uuid::from_u128(2);
    let target_uuid = Uuid::from_u128(0x4243);
    let plugin = HappyPathPlugin::new();
    plugin.set_get_record_for(target_uuid, sample_persisted_record(target_uuid, tenant_id));
    plugin.set_create_records(vec![Ok(sample_persisted_invalidation(
        Uuid::new_v4(),
        tenant_id,
        target_uuid,
    ))]);
    let service = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build(
            Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
            "test.handler.create_records.faithful_copy.v1",
        );

    let body = create_request_json_with(&serde_json::json!({
        "invalidates": target_uuid.to_string(),
        "reason_code": HAPPY_REASON_CODE,
    }));
    let req: CreateUsageRecordsRequest =
        serde_json::from_value(body).expect("the request body deserializes");

    let response = handle_create_usage_records(
        Extension(authenticated_ctx()),
        Extension(service),
        Json(req),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    let forwarded = plugin
        .last_create_records_input()
        .expect("plugin received the eligible batch");
    assert_eq!(forwarded.len(), 1);
    let invalidation = forwarded[0]
        .invalidation
        .as_ref()
        .expect("the fold MUST carry the correction reference through to the SPI");
    assert_eq!(invalidation.target, target_uuid);
    assert_eq!(invalidation.reason.as_str(), HAPPY_REASON_CODE);
}

#[tokio::test]
async fn a_leap_second_period_bound_is_rejected() {
    // `time`'s RFC 3339 parser renders 23:59:60 at a valid stand-in
    // position as 23:59:59.999999999, so a leap second reaches the gear as
    // a sub-microsecond bound and the precision precondition rejects it
    // (`cpt-cf-usage-collector-adr-record-identity-derivation`: "A second
    // value of 60 is rejected on the same path, so no leap second enters the
    // derivation"). `time::Time` cannot represent
    // second 60 at all, so this is the only place the rule can be observed.
    // A `:60` anywhere else fails at the parser instead — see the sibling
    // below.
    let (status, item) = dispatch_one_record_batch(
        "test.handler.create_records.leap_second.v1",
        create_request_json("2016-12-31T22:00:00Z", "2016-12-31T23:59:60Z"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::MULTI_STATUS,
        "an all-rejected batch MUST surface as 207 Multi-Status",
    );
    assert_eq!(
        rejected_violation_field(&item),
        "window_end",
        "the rejection MUST attribute to the offending period bound",
    );
}

#[test]
fn a_second_sixty_outside_a_leap_position_is_rejected_at_deserialization() {
    // The other half of the rule: `time` admits `:60` only at a valid
    // leap-second stand-in position and rejects it everywhere else, so a
    // request carrying one never reaches the gear's own precondition at
    // all. Pinning both halves is what keeps "no leap second enters the
    // derivation" true regardless of which of the two paths a caller trips.
    let err = serde_json::from_value::<CreateUsageRecordsRequest>(create_request_json(
        "2026-05-29T11:00:00Z",
        "2026-05-29T12:00:60Z",
    ))
    .expect_err("a :60 second outside a leap position MUST be rejected");
    assert!(
        err.to_string().contains("second"),
        "the deserialization error MUST name the out-of-range second; got {err}",
    );
}

#[tokio::test]
async fn an_inverted_covered_period_is_rejected_per_record() {
    // The second period precondition, end to end: `window_end` before
    // `window_start` is a validation error at the ingestion choke point,
    // lifted into the same per-record `Problem` envelope every other
    // per-record rejection uses.
    let (status, item) = dispatch_one_record_batch(
        "test.handler.create_records.inverted_period.v1",
        create_request_json(
            &rfc3339(recent_window_end()),
            &rfc3339(recent_window_start()),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert_eq!(rejected_violation_field(&item), "window_end");
}

#[tokio::test]
async fn a_point_event_is_accepted_per_record() {
    // The boundary on the other side of the ordering check: equal bounds
    // are a point event, not an inverted period. Without this, tightening
    // the check to `window_end <= window_start` would pass every other
    // handler test.
    let (status, item) = dispatch_one_record_batch(
        "test.handler.create_records.point_event.v1",
        create_request_json(&rfc3339(recent_window_end()), &rfc3339(recent_window_end())),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        item.get("outcome").and_then(serde_json::Value::as_str),
        Some("accepted"),
        "a zero-length covered period MUST be accepted; got {item:?}",
    );
}

#[tokio::test]
async fn create_records_happy_path_wire_body_projects_the_invalidation_entry_type() {
    // Sibling to `create_records_happy_path_wire_body_reflects_service_returned_record`:
    // pin the other side of the derived-`entry_type` projection. The
    // projection reads the record's own reference, so a plugin returning an
    // entry that carries one MUST surface as `entry_type: "invalidation"`
    // with the correction pair beside it. A regression that hard-coded the
    // discriminator, or dropped either half of the pair out of
    // `From<UsageRecord>`, would not surface in any other test.
    //
    // The submission itself is an ordinary record: what is under test is
    // the response projection of whatever the plugin returned, not the
    // ingestion path's own target resolution.
    let plugin = HappyPathPlugin::new();
    let tenant_id = Uuid::from_u128(2);
    let persisted_uuid = Uuid::new_v4();
    let target_uuid = Uuid::new_v4();
    assert_ne!(persisted_uuid, target_uuid, "test premise");
    plugin.set_create_records(vec![Ok(sample_persisted_invalidation(
        persisted_uuid,
        tenant_id,
        target_uuid,
    ))]);

    let service = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build(
            Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
            "test.handler.create_records.invalidation_projection.v1",
        );

    let req = CreateUsageRecordsRequest {
        records: vec![CreateUsageRecordRequest {
            gts_type_id: HAPPY_RECORD_GTS_ID.to_owned(),
            tenant_id,
            resource_ref: ResourceRefDto {
                resource_id: "rsc-happy".to_owned(),
                resource_type: "compute.vm".to_owned(),
            },
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: "idem-happy".to_owned(),
            invalidates: None,
            reason_code: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }],
    };

    let response = handle_create_usage_records(
        Extension(authenticated_ctx()),
        Extension(service),
        Json(req),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");
    let record = body
        .get("results")
        .and_then(serde_json::Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|item| item.get("record"))
        .expect("accepted item carries `record`");
    assert_eq!(
        record.get("entry_type").and_then(serde_json::Value::as_str),
        Some("invalidation"),
        "an entry carrying a withdrawal MUST project as `invalidation`",
    );
    assert_eq!(
        record
            .get("invalidates")
            .and_then(serde_json::Value::as_str),
        Some(target_uuid.to_string().as_str()),
        "the wire body MUST carry the withdrawn entry's id",
    );
    assert_eq!(
        record
            .get("reason_code")
            .and_then(serde_json::Value::as_str),
        Some(HAPPY_REASON_CODE),
        "the wire body MUST carry the stated reason beside the reference",
    );
    assert!(
        record.get("status").is_none(),
        "no lifecycle flag survives on the wire; got {record:?}",
    );
}

#[tokio::test]
async fn create_records_mixed_batch_preserves_input_order_across_accept_and_reject() {
    // Submit a 3-record batch as [valid, bad-prefix, valid]. The bad-prefix
    // record is rejected at the handler boundary (never reaches the service);
    // the two valid records flow through the service and the plugin returns
    // Ok for both. The wire body's `results` array MUST come out ordered by
    // input index — [Accepted(0), Rejected(1), Accepted(2)] — to pin the
    // handler's sort-by-input-index step against a regression that would
    // append accepted entries after rejected ones.
    let plugin = HappyPathPlugin::new();
    let tenant_id = Uuid::from_u128(2);
    let gts_id = MeterTypeId::new(HAPPY_RECORD_GTS_ID).expect("valid gts_type_id");
    let derived_id_0 = derive_usage_record_id(
        tenant_id,
        &gts_id,
        &IdempotencyKey::new("idem-mixed-0").expect("valid idempotency key"),
        recent_window_start(),
        recent_window_end(),
    );
    let derived_id_2 = derive_usage_record_id(
        tenant_id,
        &gts_id,
        &IdempotencyKey::new("idem-mixed-2").expect("valid idempotency key"),
        recent_window_start(),
        recent_window_end(),
    );
    let persisted_uuid_0 = Uuid::new_v4();
    let persisted_uuid_2 = Uuid::new_v4();
    plugin.set_create_records(vec![
        Ok(sample_persisted_record(persisted_uuid_0, tenant_id)),
        Ok(sample_persisted_record(persisted_uuid_2, tenant_id)),
    ]);

    let service = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build(
            Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
            "test.handler.create_records.mixed.v1",
        );

    let valid_record = |idem: &str| CreateUsageRecordRequest {
        gts_type_id: HAPPY_RECORD_GTS_ID.to_owned(),
        tenant_id,
        resource_ref: ResourceRefDto {
            resource_id: "rsc-mixed".to_owned(),
            resource_type: "compute.vm".to_owned(),
        },
        subject_ref: None,
        metadata: BTreeMap::new(),
        value: rust_decimal::Decimal::from(1),
        idempotency_key: idem.to_owned(),
        invalidates: None,
        reason_code: None,
        window_start: recent_window_start(),
        window_end: recent_window_end(),
    };

    let req = CreateUsageRecordsRequest {
        records: vec![
            valid_record("idem-mixed-0"),
            CreateUsageRecordRequest {
                gts_type_id: "not-a-valid-prefix".to_owned(),
                tenant_id,
                resource_ref: ResourceRefDto {
                    resource_id: "rsc-mixed".to_owned(),
                    resource_type: "compute.vm".to_owned(),
                },
                subject_ref: None,
                metadata: BTreeMap::new(),
                value: rust_decimal::Decimal::from(1),
                idempotency_key: "idem-mixed-1".to_owned(),
                invalidates: None,
                reason_code: None,
                window_start: recent_window_start(),
                window_end: recent_window_end(),
            },
            valid_record("idem-mixed-2"),
        ],
    };

    let response = handle_create_usage_records(
        Extension(authenticated_ctx()),
        Extension(service),
        Json(req),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::MULTI_STATUS,
        "any rejection in the batch MUST surface as 207 Multi-Status",
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");
    let results = body
        .get("results")
        .and_then(serde_json::Value::as_array)
        .expect("response carries a `results` array");
    assert_eq!(results.len(), 3);

    let outcome = |i: usize| {
        results[i]
            .get("outcome")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let index_field = |i: usize| results[i].get("index").and_then(serde_json::Value::as_u64);

    assert_eq!(outcome(0), "accepted", "input index 0 MUST be accepted");
    assert_eq!(index_field(0), Some(0));
    assert_eq!(
        results[0]
            .get("record")
            .and_then(|r| r.get("id"))
            .and_then(serde_json::Value::as_str),
        Some(persisted_uuid_0.to_string().as_str()),
    );

    assert_eq!(outcome(1), "rejected", "input index 1 MUST be rejected");
    assert_eq!(index_field(1), Some(1));

    assert_eq!(outcome(2), "accepted", "input index 2 MUST be accepted");
    assert_eq!(index_field(2), Some(2));
    assert_eq!(
        results[2]
            .get("record")
            .and_then(|r| r.get("id"))
            .and_then(serde_json::Value::as_str),
        Some(persisted_uuid_2.to_string().as_str()),
    );

    let forwarded = plugin
        .last_create_records_input()
        .expect("plugin received the eligible batch");
    assert_eq!(
        forwarded.len(),
        2,
        "plugin MUST receive only the eligible (handler-validated) records",
    );
    assert_eq!(forwarded[0].id, derived_id_0);
    assert_eq!(forwarded[1].id, derived_id_2);
}

// ---------------------------------------------------------------------------
// `handle_get_usage_record`: GET /usage-collector/v1/records/{id}
//
// Handler-shaped concerns the service tests cannot reach:
//   - Malformed UUID path segment lifts to InvalidArgument (HTTP 400)
//     BEFORE the service is invoked.
//   - `get_usage_record` authorizes via a pre-row compiled-scope PDP
//     request BEFORE resolving the plugin (DESIGN §3.3) — so a missing
//     plugin surfaces 503 AFTER the PDP is reached (not before), and an
//     unreachable PDP surfaces 503 before any plugin dispatch.
//   - Happy path: 200 OK with the persisted record body, wire-projected
//     through `UsageRecordDto`.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_with_malformed_uuid_returns_400_before_reaching_service() {
    let (service, resolver) = service_with_sentinel_pdp();

    let raw_uuid = "not-a-uuid".to_owned();
    let response = handle_get_usage_record(
        Extension(SecurityContext::anonymous()),
        Extension(service),
        Path(raw_uuid.clone()),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "malformed UUID MUST lift to 400",
    );
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.contains("problem+json"),
        "malformed-UUID response MUST be application/problem+json (got `{content_type}`)",
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("Problem body collected");
    let body: serde_json::Value =
        serde_json::from_slice(&body_bytes).expect("Problem body is JSON");
    let violation = body
        .get("context")
        .and_then(|c| c.get("field_violations"))
        .and_then(|fv| fv.as_array())
        .and_then(|arr| arr.first())
        .expect("InvalidArgument envelope carries field_violations[0]");
    assert_eq!(
        violation.get("field").and_then(serde_json::Value::as_str),
        Some("id"),
    );
    assert_eq!(
        violation.get("reason").and_then(serde_json::Value::as_str),
        Some("VALIDATION"),
    );
    // The envelope echoes the rejected raw segment back. A caller whose id
    // came out of a template or a copy-paste has nothing else to debug
    // against: `field` and `reason` say the shape is wrong but not which
    // value was wrong, and the request never reached the service, so no
    // server-side log ties the 400 to a row. A constant message would pass
    // every other assertion here.
    let description = violation
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        description.contains(raw_uuid.as_str()),
        "field_violations[0].description MUST echo the rejected raw value \
         (got `{description}`)",
    );
    assert_eq!(
        resolver.calls(),
        0,
        "handler MUST reject the malformed UUID before reaching the service",
    );
}

#[tokio::test]
async fn get_without_plugin_surfaces_503() {
    // No usage-collector storage plugin registered. `get_usage_record` now
    // authorizes via a pre-row compiled-scope PDP request BEFORE resolving
    // the plugin (the point lookup shares the posture of list/aggregate),
    // so a *permitting* resolver is required to reach
    // `Service::get_plugin`'s failure at all — a `CountingUnreachableResolver`
    // sentinel would instead surface 503 from the PDP step itself, proving
    // nothing about plugin resolution. `CountingPermitResolver` grants a
    // real scope and lets the test pin that the PDP WAS reached exactly
    // once before the plugin-resolution failure.
    let hub = Arc::new(ClientHub::new());
    let resolver = CountingPermitResolver::new(
        pep_properties::OWNER_TENANT_ID,
        Uuid::from_u128(2).to_string(),
    );
    let enforcer = enforcer_for(Arc::clone(&resolver) as _);
    let service = Arc::new(Service::new(hub, "cyberfabric".to_owned(), enforcer));

    let response = handle_get_usage_record(
        Extension(authenticated_ctx()),
        Extension(service),
        Path(Uuid::new_v4().to_string()),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "missing storage plugin MUST surface 503",
    );
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.contains("problem+json"),
        "503 envelope MUST be application/problem+json (got `{content_type}`)",
    );
    assert_eq!(
        resolver.calls(),
        1,
        "authorization now runs BEFORE plugin resolution - the PDP IS \
         reached once, and it is the subsequent plugin-resolution failure \
         that produces the 503",
    );
}

#[tokio::test]
async fn get_with_unreachable_pdp_surfaces_503() {
    // A real plugin is wired (unused here — authorization now runs BEFORE
    // any plugin dispatch) plus an unreachable PDP resolver. The resolver
    // fails with transport `ServiceUnavailable`, lifted to the canonical
    // 503 `Problem`. The counting resolver pins the real PDP path:
    // `calls() >= 1` is direct evidence the handler reached the PDP call —
    // the very first thing `Service::get_usage_record` does.
    let plugin = HappyPathPlugin::new();
    let target_uuid = Uuid::new_v4();
    let tenant_id = Uuid::from_u128(2);
    plugin.set_get_record(sample_persisted_record(target_uuid, tenant_id));

    let hub = crate::domain::test_support::hub_with_plugin(
        Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
        "test.handler.get_record.unreachable_pdp.v1",
        "cyberfabric",
    );
    let resolver = CountingUnreachableResolver::new();
    let enforcer = enforcer_for(Arc::clone(&resolver) as _);
    let service = Arc::new(Service::new(hub, "cyberfabric".to_owned(), enforcer));

    let response = handle_get_usage_record(
        Extension(authenticated_ctx()),
        Extension(service),
        Path(target_uuid.to_string()),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "unreachable PDP MUST surface 503",
    );
    assert!(
        resolver.calls() >= 1,
        "handler MUST reach the PDP step before failing (resolver.calls() == {})",
        resolver.calls(),
    );
}

#[tokio::test]
async fn get_happy_path_returns_200_with_record_body() {
    let plugin = HappyPathPlugin::new();
    let target_uuid = Uuid::new_v4();
    let tenant_id = Uuid::from_u128(2);
    plugin.set_get_record(sample_persisted_record(target_uuid, tenant_id));

    // `get_usage_record` now authorizes via a pre-row compiled-scope PDP
    // request; the fixture's default `CountingTenantPermitResolver`
    // reads the constraint back out of the caller's own request, which a
    // pre-row request carries none of, so it would fall back to an
    // allow-all permit and fail closed under `require_constraints(true)`.
    // `recording_plugin_resolver` grants a fixed tenant-narrowing scope
    // regardless of request shape instead.
    let service = ServiceFixture::default()
        .with_resolver(recording_plugin_resolver())
        .build(
            Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
            "test.handler.get_record.happy.v1",
        );

    let response = handle_get_usage_record(
        Extension(authenticated_ctx()),
        Extension(service),
        Path(target_uuid.to_string()),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "happy-path get MUST surface 200 OK",
    );
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.contains("json"),
        "200 envelope MUST be JSON (got `{content_type}`)",
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");
    assert_eq!(
        body.get("id").and_then(serde_json::Value::as_str),
        Some(target_uuid.to_string().as_str()),
        "wire body MUST echo the loaded record's UUID",
    );
    assert_eq!(
        body.get("entry_type").and_then(serde_json::Value::as_str),
        Some("record"),
        "the point lookup MUST project the derived entry type too",
    );
}

// ---------------------------------------------------------------------------
// prepare_list_query — $top cap, $orderby default, cursor validate
// ---------------------------------------------------------------------------

mod prepare_list_query_tests {
    use toolkit_canonical_errors::CanonicalError;
    use toolkit_canonical_errors::context::InvalidArgumentV1;
    use toolkit_odata::ast::{CompareOperator, Expr, Value};
    use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, SortDir};

    use super::super::{MAX_PAGE_SIZE, prepare_list_query};

    fn assert_order_keys(order: &ODataOrderBy, expected: &[(&str, SortDir)]) {
        assert_eq!(order.0.len(), expected.len(), "order arity mismatch");
        for (i, (field, dir)) in expected.iter().enumerate() {
            assert_eq!(order.0[i].field, *field, "order key #{i} field");
            assert_eq!(order.0[i].dir, *dir, "order key #{i} dir");
        }
    }

    fn extract_first_field_violation_reason(err: &CanonicalError) -> Option<String> {
        let CanonicalError::InvalidArgument { ctx, .. } = err else {
            return None;
        };
        match ctx {
            InvalidArgumentV1::FieldViolations { field_violations } => {
                field_violations.first().map(|v| v.reason.clone())
            }
            _ => None,
        }
    }

    fn extract_first_field_violation_field(err: &CanonicalError) -> Option<String> {
        let CanonicalError::InvalidArgument { ctx, .. } = err else {
            return None;
        };
        match ctx {
            InvalidArgumentV1::FieldViolations { field_violations } => {
                field_violations.first().map(|v| v.field.clone())
            }
            _ => None,
        }
    }

    #[test]
    fn limit_above_max_page_size_is_rejected_as_invalid_argument() {
        // A silent clamp would hand the caller a partial page that
        // looks complete; reject so paginators surface the cap.
        let mut q = ODataQuery::new();
        q.limit = Some(5_000);
        let err = prepare_list_query(q).expect_err("limit > cap must be rejected");
        assert!(
            matches!(err, CanonicalError::InvalidArgument { .. }),
            "limit > MAX_PAGE_SIZE must surface as InvalidArgument, got {err:?}",
        );
        assert_eq!(
            extract_first_field_violation_reason(&err).as_deref(),
            Some("VALIDATION"),
        );
    }

    #[test]
    fn limit_above_max_page_size_names_top_as_the_violating_field() {
        // `$top` and `limit` both fold onto `ODataQuery.limit`, so the
        // parsed query cannot say which spelling arrived. The violation
        // names the canonical one and the detail names the alias —
        // matching `toolkit_odata`'s own `InvalidLimit` mapping, so a
        // client dispatching on `field` sees one spelling platform-wide.
        let mut q = ODataQuery::new();
        q.limit = Some(5_000);
        let err = prepare_list_query(q).expect_err("limit > cap must be rejected");
        assert_eq!(
            extract_first_field_violation_field(&err).as_deref(),
            Some("$top"),
        );
    }

    #[test]
    fn limit_equal_to_max_page_size_is_preserved() {
        let mut q = ODataQuery::new();
        q.limit = Some(MAX_PAGE_SIZE);
        let out = prepare_list_query(q).expect("limit == cap is the boundary");
        assert_eq!(out.limit, Some(MAX_PAGE_SIZE));
    }

    #[test]
    fn limit_below_cap_is_preserved() {
        let mut q = ODataQuery::new();
        q.limit = Some(42);
        let out = prepare_list_query(q).expect("ok");
        assert_eq!(out.limit, Some(42));
    }

    #[test]
    fn missing_limit_defaults_to_max_page_size() {
        let out = prepare_list_query(ODataQuery::new()).expect("ok");
        assert_eq!(
            out.limit,
            Some(MAX_PAGE_SIZE),
            "absent limit defaults to MAX_PAGE_SIZE so the plugin never sees an unbounded read",
        );
    }

    #[test]
    fn empty_orderby_and_no_cursor_defaults_to_canonical_keyset() {
        // `(window_end asc, id asc)`: the column the mandatory range
        // selects on is the column the page orders by, so one index serves
        // both. The floor itself is the domain's — this pins that the wire
        // path reaches it.
        let out = prepare_list_query(ODataQuery::new()).expect("ok");
        assert_order_keys(
            &out.order,
            &[("window_end", SortDir::Asc), ("id", SortDir::Asc)],
        );
    }

    #[test]
    fn supplied_orderby_gets_unique_tiebreaker_appended() {
        // The caller's explicit `$orderby` is preserved as the leading
        // sort key, and because it names neither canonical field the
        // gateway appends both — so this fixture does end in
        // `(window_end, id)`. What is guaranteed in general is only that
        // both names are present; the domain's floor tests carry the
        // orders where they land elsewhere. Without both, the plugin keys
        // against a non-unique boundary and silently drops the tied rows
        // that did not fit on the previous page.
        let mut q = ODataQuery::new();
        q.order = ODataOrderBy(vec![OrderKey {
            field: "resource_id".into(),
            dir: SortDir::Desc,
        }]);
        let out = prepare_list_query(q).expect("ok");
        // Direction-aware: the tiebreaker is appended in the order's existing
        // direction so the plugin (uniform-direction keyset only) never sees
        // a mixed-direction tuple.
        assert_order_keys(
            &out.order,
            &[
                ("resource_id", SortDir::Desc),
                ("window_end", SortDir::Desc),
                ("id", SortDir::Desc),
            ],
        );
    }

    #[test]
    fn an_order_on_the_covered_period_end_is_accepted() {
        // `$orderby=window_end` names the leading suffix key but no unique
        // final one, so the gateway must append `id` — a page boundary at a
        // run of entries sharing a `window_end` would otherwise drop the
        // tied rows that did not fit on the previous page. `window_end` is
        // already named, so only `id` is appended.
        let mut q = ODataQuery::new();
        q.order = ODataOrderBy(vec![OrderKey {
            field: "window_end".into(),
            dir: SortDir::Asc,
        }]);
        let out = prepare_list_query(q).expect("ok");
        assert_order_keys(
            &out.order,
            &[("window_end", SortDir::Asc), ("id", SortDir::Asc)],
        );
    }

    #[test]
    fn an_order_on_created_at_is_rejected() {
        // `created_at` is no longer a record attribute, so a caller order
        // naming it must fail closed with a `400` rather than resolve to
        // the covered period or to nothing. The message is deliberately
        // not asserted: the rejection may come from the keyset-safety
        // classification or from the OData layer as an unknown field, and
        // either is a correct fail-closed outcome.
        let mut q = ODataQuery::new();
        q.order = ODataOrderBy(vec![OrderKey {
            field: "created_at".into(),
            dir: SortDir::Asc,
        }]);
        let err = prepare_list_query(q).expect_err("a retired order key must be rejected");
        assert!(
            matches!(err, CanonicalError::InvalidArgument { .. }),
            "$orderby=created_at must surface as InvalidArgument, got {err:?}",
        );
    }

    #[test]
    fn descending_orderby_appends_tiebreaker_in_same_direction() {
        // Direction handling: this plugin's keyset only supports
        // uniform-direction tuples, so the appended tiebreaker must follow
        // the caller's direction. A `window_end desc` order must normalize
        // to `(window_end desc, id desc)` — never `(window_end desc, id
        // asc)`, which the plugin would reject as a mixed-direction keyset.
        let mut q = ODataQuery::new();
        q.order = ODataOrderBy(vec![OrderKey {
            field: "window_end".into(),
            dir: SortDir::Desc,
        }]);
        let out = prepare_list_query(q).expect("ok");
        assert_order_keys(
            &out.order,
            &[("window_end", SortDir::Desc), ("id", SortDir::Desc)],
        );
    }

    #[test]
    fn mixed_direction_orderby_is_rejected_as_invalid_argument() {
        // The storage plugin's keyset supports only uniform-direction
        // tuples. A caller order that mixes ascending and descending keys
        // (e.g. `$orderby=window_end asc,tenant_id desc`) can only ever
        // compose into a mixed-direction keyset the plugin rejects
        // downstream with a late, non-specific error. Reject it up front
        // with a typed 400 that names the real cause (mixed sort
        // directions) instead of leaking a plugin-internal keyset error to
        // the caller.
        //
        // Both keys are deliberately keyset-safe. With a non-mandatory
        // second key the order would be refused anyway, for the other
        // reason, and this test would stay green with the direction rule
        // deleted. `tenant_id` is on the keyset allowlist, so the fixture
        // rests on the same closed set admissibility is decided against.
        let mut q = ODataQuery::new();
        q.order = ODataOrderBy(vec![
            OrderKey {
                field: "window_end".into(),
                dir: SortDir::Asc,
            },
            OrderKey {
                field: "tenant_id".into(),
                dir: SortDir::Desc,
            },
        ]);
        let err =
            prepare_list_query(q).expect_err("mixed-direction $orderby must be rejected up front");
        assert!(
            matches!(err, CanonicalError::InvalidArgument { .. }),
            "mixed-direction $orderby must surface as InvalidArgument, got {err:?}",
        );
        assert_eq!(
            extract_first_field_violation_reason(&err).as_deref(),
            Some("VALIDATION"),
        );
    }

    #[test]
    fn orderby_on_a_nullable_field_is_rejected_as_invalid_argument() {
        // The storage plugin's keyset continuation is a row-value tuple
        // comparison that is only sound over NOT NULL columns. `subject_id`,
        // `subject_type`, and `invalidates` are domain-optional (nullable), so
        // a `$orderby` leading on one of them would silently drop NULL-keyed
        // rows from the page (and 500 on a page ending at a NULL row). Reject
        // it up front with a typed 400 that names the real cause instead of
        // leaking a plugin-internal keyset error — or, worse, an incomplete
        // page — to the caller.
        for field in ["subject_id", "subject_type", "invalidates"] {
            let mut q = ODataQuery::new();
            q.order = ODataOrderBy(vec![OrderKey {
                field: field.into(),
                dir: SortDir::Asc,
            }]);
            let err =
                prepare_list_query(q).expect_err("$orderby on a nullable field must be rejected");
            assert!(
                matches!(err, CanonicalError::InvalidArgument { .. }),
                "$orderby on nullable `{field}` must surface as InvalidArgument, got {err:?}",
            );
            assert_eq!(
                extract_first_field_violation_reason(&err).as_deref(),
                Some("VALIDATION"),
            );
        }
    }

    #[test]
    fn orderby_on_a_mandatory_field_other_than_the_tiebreaker_is_accepted() {
        // Guard against over-rejection: `tenant_id` and `resource_id` are
        // attributes every entry carries in its own right, so ordering by
        // either is a valid keyset and must still gain the canonical
        // `(window_end, id)` suffix. Both are on
        // `KEYSET_SAFE_RECORD_FIELDS`, which is what admissibility is
        // decided against — so this fixture moves only if that allowlist
        // does, and loudly.
        for field in ["tenant_id", "resource_id"] {
            let mut q = ODataQuery::new();
            q.order = ODataOrderBy(vec![OrderKey {
                field: field.into(),
                dir: SortDir::Asc,
            }]);
            let out =
                prepare_list_query(q).expect("ordering by a mandatory field is a valid keyset");
            assert_order_keys(
                &out.order,
                &[
                    (field, SortDir::Asc),
                    ("window_end", SortDir::Asc),
                    ("id", SortDir::Asc),
                ],
            );
        }
    }

    #[test]
    fn uniform_multi_key_orderby_is_accepted_and_tiebroken() {
        // A uniform-direction multi-key order (all `desc` here) is valid: it
        // is preserved and gains the canonical `id` suffix in the same
        // direction. Guards the mixed-direction rejection against
        // over-rejecting legitimate multi-key orders.
        let mut q = ODataQuery::new();
        q.order = ODataOrderBy(vec![
            OrderKey {
                field: "resource_id".into(),
                dir: SortDir::Desc,
            },
            OrderKey {
                field: "window_end".into(),
                dir: SortDir::Desc,
            },
        ]);
        let out = prepare_list_query(q).expect("uniform multi-key order is valid");
        assert_order_keys(
            &out.order,
            &[
                ("resource_id", SortDir::Desc),
                ("window_end", SortDir::Desc),
                ("id", SortDir::Desc),
            ],
        );
    }

    #[test]
    fn cursor_present_materializes_keyset_order_from_signed_tokens() {
        // Regression (cursor-continuation 500): the toolkit OData extractor
        // leaves `order` empty on a cursor request — the effective keyset
        // order lives in the cursor's signed-token payload (`cursor.s`).
        // The storage plugin reads `query.order` *directly* to build both
        // the `ORDER BY` and the keyset continuation predicate; it has no
        // access to the cursor's token derivation. So `prepare_list_query`
        // MUST materialize the cursor-derived order back into `query.order`.
        // Before the fix it was derived "for comparison purposes only" and
        // the empty order propagated to the plugin, which 500'd with
        // "keyset order must not be empty" on every cursor follow-up.
        let mut q = ODataQuery::new();
        q.cursor = Some(CursorV1 {
            k: vec!["2026-06-12T00:00:00Z".into(), uuid::Uuid::nil().to_string()],
            o: SortDir::Asc,
            s: "+window_end,+id".to_owned(),
            f: None,
            d: "fwd".to_owned(),
        });
        // Note: q.order intentionally empty (mirrors the toolkit extractor).
        let out = prepare_list_query(q).expect("cursor-driven request validates");
        assert_order_keys(
            &out.order,
            &[("window_end", SortDir::Asc), ("id", SortDir::Asc)],
        );
    }

    // `validate_cursor_against` derives the effective order from
    // `cursor.s` itself in our wrapper, so an `OrderMismatch` between
    // a caller-supplied `$orderby` and the cursor's bound order can
    // never originate inside `prepare_list_query` directly — the
    // toolkit's OData extractor already rejects "cursor + $orderby"
    // combinations as `Error::OrderWithCursor` upstream. The
    // architectural decision is documented in
    // `prepare_list_query`'s docstring; no dedicated regression test
    // is needed for an unreachable branch.

    #[test]
    fn the_edge_leaves_the_query_fingerprint_comparison_to_the_service() {
        // The extractor's `filter_hash` is a hash of `$filter` alone,
        // while the fingerprint a continuation is bound to also covers
        // `gts_type_id`, the read range and `metadata_filter` — computed
        // and compared behind the service, which is the only layer an
        // in-process caller passes through too. An edge that kept comparing its own
        // narrower value would therefore reject every legitimate page
        // two, so it MUST pass `None` and keep only the signed-token
        // order check. Two fingerprints for one property is the defect,
        // not the redundancy.
        //
        // Deliberately divergent values here: this passes only because
        // the comparison is gone, so restoring the argument fails it.
        let mut q = ODataQuery::new();
        q.filter = Some(Box::new(Expr::Compare(
            Box::new(Expr::Identifier("resource_type".into())),
            CompareOperator::Eq,
            Box::new(Expr::Value(Value::String("compute.vm".into()))),
        )));
        q.filter_hash = Some("hash_current".into());
        q.cursor = Some(CursorV1 {
            k: vec!["x".into()],
            o: SortDir::Asc,
            s: "+window_end,+id".to_owned(),
            f: Some("hash_DIFFERENT".into()),
            d: "fwd".to_owned(),
        });
        let out =
            prepare_list_query(q).expect("the edge no longer owns the fingerprint comparison");
        assert!(
            out.cursor.is_some(),
            "the decoded cursor MUST reach the service, which is where the \
             fingerprint is compared",
        );
        assert_order_keys(
            &out.order,
            &[("window_end", SortDir::Asc), ("id", SortDir::Asc)],
        );
    }

    #[test]
    fn cursor_with_malformed_signed_tokens_surfaces_invalid_orderby_field() {
        // `from_signed_tokens` rejects an empty `s` as InvalidOrderByField,
        // which lifts to canonical InvalidArgument carrying reason
        // "INVALID_ORDERBY_FIELD" — NOT one of the cursor categories,
        // but the same fail-closed posture.
        let mut q = ODataQuery::new();
        q.cursor = Some(CursorV1 {
            k: vec!["x".into()],
            o: SortDir::Asc,
            s: String::new(), // malformed
            f: None,
            d: "fwd".to_owned(),
        });
        let err = prepare_list_query(q).expect_err("empty signed tokens reject");
        assert!(
            matches!(err, CanonicalError::InvalidArgument { .. }),
            "malformed cursor signed tokens MUST lift to canonical InvalidArgument",
        );
    }
}

// ---------------------------------------------------------------------------
// parse_metadata_filters — repeated query-param grouping
// ---------------------------------------------------------------------------

mod parse_metadata_filters_tests {
    use toolkit_canonical_errors::CanonicalError;

    use super::super::parse_metadata_filters;

    fn p(key: &str, value: &str) -> (String, String) {
        (key.to_owned(), value.to_owned())
    }

    #[test]
    fn no_metadata_entries_yields_empty_vec() {
        let out = parse_metadata_filters(&[p("gts_type_id", "x")]).expect("ok");
        assert!(out.is_empty());
    }

    #[test]
    fn single_key_with_one_value_makes_one_filter() {
        let out = parse_metadata_filters(&[p("metadata.user_id", "u1")]).expect("ok");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key().as_str(), "user_id");
        assert_eq!(out[0].values(), &["u1".to_owned()]);
    }

    #[test]
    fn multiple_values_for_same_key_collapse_into_one_filter() {
        let out = parse_metadata_filters(&[
            p("metadata.user_id", "u1"),
            p("metadata.user_id", "u2"),
            p("metadata.user_id", "u3"),
        ])
        .expect("ok");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key().as_str(), "user_id");
        assert_eq!(
            out[0].values(),
            &["u1".to_owned(), "u2".to_owned(), "u3".to_owned()],
        );
    }

    #[test]
    fn distinct_keys_make_distinct_filters_in_sorted_order() {
        let out = parse_metadata_filters(&[
            p("metadata.region", "eu"),
            p("metadata.user_id", "u1"),
            p("metadata.account", "acme"),
        ])
        .expect("ok");
        // BTreeMap inside the helper gives deterministic key order.
        let keys: Vec<_> = out.iter().map(|f| f.key().as_str().to_owned()).collect();
        assert_eq!(keys, vec!["account", "region", "user_id"]);
    }

    #[test]
    fn empty_key_is_rejected_as_invalid_argument() {
        let err = parse_metadata_filters(&[p("metadata.", "x")])
            .expect_err("metadata. with no key MUST fail");
        assert!(matches!(err, CanonicalError::InvalidArgument { .. }));
    }

    #[test]
    fn non_metadata_params_are_ignored() {
        let out = parse_metadata_filters(&[
            p("gts_type_id", "g"),
            p("from", "f"),
            p("metadata.k", "v"),
            p("$filter", "x"),
        ])
        .expect("ok");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key().as_str(), "k");
    }

    #[test]
    fn distinct_key_count_at_cap_is_admitted_but_one_over_is_rejected() {
        // Boundary check on `MAX_METADATA_FILTERS`. At the cap the helper
        // returns Ok; one over the cap MUST surface as `InvalidArgument`
        // with the violation field pinned to `metadata` (NOT to a
        // specific key) so the caller learns it is the cardinality, not
        // any single key, that breached the cap.
        use super::super::MAX_METADATA_FILTERS;

        let at_cap: Vec<(String, String)> = (0..MAX_METADATA_FILTERS)
            .map(|i| p(&format!("metadata.k{i}"), "v"))
            .collect();
        let out = parse_metadata_filters(&at_cap).expect("at-cap distinct keys must be admitted");
        assert_eq!(out.len(), MAX_METADATA_FILTERS);

        let over_cap: Vec<(String, String)> = (0..=MAX_METADATA_FILTERS)
            .map(|i| p(&format!("metadata.k{i}"), "v"))
            .collect();
        let err = parse_metadata_filters(&over_cap)
            .expect_err("MAX_METADATA_FILTERS + 1 distinct keys MUST reject");
        let field = match err {
            CanonicalError::InvalidArgument { ctx, .. } => match ctx {
                toolkit_canonical_errors::context::InvalidArgumentV1::FieldViolations {
                    field_violations,
                } => field_violations
                    .first()
                    .map(|v| v.field.clone())
                    .unwrap_or_default(),
                _ => String::new(),
            },
            _ => panic!("over-cap distinct keys MUST surface as InvalidArgument"),
        };
        assert_eq!(
            field, "metadata",
            "over-cap distinct-key rejection MUST blame the bag (`metadata`), \
             not any specific `metadata.<key>`",
        );
    }

    #[test]
    fn per_key_value_count_at_cap_is_admitted_but_one_over_is_rejected() {
        // Boundary check on `MAX_METADATA_FILTER_VALUES` — the per-key
        // value-list cap that bounds the OR-within-key expansion at the
        // plugin. The over-cap violation MUST scope its `field` to the
        // offending `metadata.<key>` so the caller can correct the
        // specific repeated query parameter.
        use super::super::MAX_METADATA_FILTER_VALUES;

        let at_cap: Vec<(String, String)> = (0..MAX_METADATA_FILTER_VALUES)
            .map(|i| p("metadata.k", &format!("v{i}")))
            .collect();
        let out = parse_metadata_filters(&at_cap).expect("at-cap per-key values must be admitted");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].values().len(), MAX_METADATA_FILTER_VALUES);

        let over_cap: Vec<(String, String)> = (0..=MAX_METADATA_FILTER_VALUES)
            .map(|i| p("metadata.k", &format!("v{i}")))
            .collect();
        let err = parse_metadata_filters(&over_cap)
            .expect_err("MAX_METADATA_FILTER_VALUES + 1 values on one key MUST reject");
        let field = match err {
            CanonicalError::InvalidArgument { ctx, .. } => match ctx {
                toolkit_canonical_errors::context::InvalidArgumentV1::FieldViolations {
                    field_violations,
                } => field_violations
                    .first()
                    .map(|v| v.field.clone())
                    .unwrap_or_default(),
                _ => String::new(),
            },
            _ => panic!("over-cap per-key values MUST surface as InvalidArgument"),
        };
        assert_eq!(
            field, "metadata.k",
            "over-cap per-key-value rejection MUST blame the specific \
             `metadata.<key>` whose value list breached the cap",
        );
    }
}

// ---------------------------------------------------------------------------
// parse_required_gts_type_id — typed mandatory query parameter
// ---------------------------------------------------------------------------
//
// `parse_required_gts_type_id` is the only place the gateway lifts
// `gts_type_id` query-string presence + validity into the typed
// `MeterTypeId`. The service tests cannot reach this surface (they
// construct the typed value directly), and the wire-level
// `handle_list_usage_records` tests would conflate three failure modes
// into the same 400. Pin each failure mode separately here so a
// regression in one path doesn't get masked by another.

mod parse_required_gts_type_id_tests {
    use toolkit_canonical_errors::CanonicalError;
    use toolkit_canonical_errors::context::InvalidArgumentV1;

    use super::super::parse_required_gts_type_id;
    use super::HAPPY_RECORD_GTS_ID;

    fn p(key: &str, value: &str) -> (String, String) {
        (key.to_owned(), value.to_owned())
    }

    fn first_violation(err: &CanonicalError) -> (String, String, String) {
        match err {
            CanonicalError::InvalidArgument {
                ctx: InvalidArgumentV1::FieldViolations { field_violations },
                ..
            } => {
                let v = field_violations.first().expect("at least one violation");
                (v.field.clone(), v.reason.clone(), v.description.clone())
            }
            _ => panic!("expected InvalidArgument with field_violations"),
        }
    }

    #[test]
    fn missing_gts_type_id_param_rejects_as_missing_required() {
        let err = parse_required_gts_type_id(&[p("$filter", "x")])
            .expect_err("missing gts_type_id MUST reject");
        let (field, reason, description) = first_violation(&err);
        assert_eq!(field, "gts_type_id");
        assert_eq!(
            reason, "VALIDATION",
            "host-private wire-shape rejection uses the gateway-side \
             `VALIDATION` reason, NOT the SDK's `INVALID_BASE_GTS_ID`",
        );
        assert!(
            description.contains("missing required query parameter"),
            "description MUST cite the missing-required idiom (got `{description}`)",
        );
    }

    #[test]
    fn duplicate_gts_type_id_param_rejects_instead_of_silently_last_winning() {
        // Two `gts_type_id=…` entries with DIFFERENT values: last-wins
        // would silently mask the caller bug. The helper MUST reject
        // outright. Pin BOTH the field/reason and that the rejection
        // happens regardless of whether either value is well-formed.
        let valid_gts = HAPPY_RECORD_GTS_ID.to_owned();
        let err = parse_required_gts_type_id(&[
            p("gts_type_id", &valid_gts),
            p("gts_type_id", "even.something.else"),
        ])
        .expect_err("duplicate gts_type_id MUST reject");
        let (field, reason, description) = first_violation(&err);
        assert_eq!(field, "gts_type_id");
        assert_eq!(reason, "VALIDATION");
        assert!(
            description.contains("at most once"),
            "duplicate-rejection description MUST cite the at-most-once \
             contract (got `{description}`)",
        );
    }

    #[test]
    fn malformed_gts_type_id_lifts_through_sdk_invalid_base_gts_id() {
        // A single but malformed `gts_type_id` must surface the SDK-side
        // `INVALID_BASE_GTS_ID` reason — NOT the host's `VALIDATION`
        // bucket — so caller-facing diagnostics distinguish "wrong
        // shape" from "missing / duplicate".
        let err = parse_required_gts_type_id(&[p("gts_type_id", "not-a-valid-prefix")])
            .expect_err("malformed gts_type_id MUST reject");
        let (field, reason, _) = first_violation(&err);
        assert_eq!(field, "gts_type_id");
        assert_eq!(reason, "INVALID_BASE_GTS_ID");
    }

    #[test]
    fn well_formed_gts_type_id_round_trips_through_the_typed_newtype() {
        let raw = HAPPY_RECORD_GTS_ID;
        let parsed = parse_required_gts_type_id(&[p("gts_type_id", raw)])
            .expect("well-formed gts_type_id passes");
        assert_eq!(AsRef::<str>::as_ref(&parsed), raw);
    }
}

// ---------------------------------------------------------------------------
// parse_required_time_range — the raw path's typed mandatory range
// ---------------------------------------------------------------------------
//
// `parse_required_time_range` is the only place the raw path lifts the
// `from` / `to` query-string pair into a validated `TimeRange`. Six
// failure modes collapse onto the same `400` at the wire boundary
// (absent, duplicated, offset-less, unparseable, inverted, empty), so
// each is pinned separately here — including which of the two parameters
// the violation blames, because "one of your timestamps is wrong" is not
// an actionable diagnostic.

mod parse_required_time_range_tests {
    use time::{OffsetDateTime, UtcOffset};
    use toolkit_canonical_errors::CanonicalError;
    use toolkit_canonical_errors::context::InvalidArgumentV1;

    use super::super::parse_required_time_range;
    use super::{RANGE_FROM, RANGE_TO};

    fn p(key: &str, value: &str) -> (String, String) {
        (key.to_owned(), value.to_owned())
    }

    fn first_violation(err: &CanonicalError) -> (String, String, String) {
        match err {
            CanonicalError::InvalidArgument {
                ctx: InvalidArgumentV1::FieldViolations { field_violations },
                ..
            } => {
                let v = field_violations.first().expect("at least one violation");
                (v.field.clone(), v.reason.clone(), v.description.clone())
            }
            _ => panic!("expected InvalidArgument with field_violations, got {err:?}"),
        }
    }

    #[test]
    fn a_well_formed_pair_maps_from_to_the_lower_bound_and_to_to_the_upper() {
        // Also the anti-swap check: the two bounds are an hour apart, so a
        // helper that read `to` into the lower slot would produce an
        // inverted range and fail rather than pass by symmetry.
        let range = parse_required_time_range(&[p("from", RANGE_FROM), p("to", RANGE_TO)])
            .expect("a well-formed pair parses");
        assert_eq!(range.lower_inclusive(), OffsetDateTime::UNIX_EPOCH);
        assert_eq!(
            range.upper_exclusive(),
            OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        );
    }

    #[test]
    fn a_non_utc_offset_is_accepted_and_normalized_to_the_same_instant() {
        // `Timestamp` in the contract requires an offset, not UTC
        // specifically. A caller sending `01:00:00+01:00` and one sending
        // `00:00:00Z` name the same instant and MUST obtain the same
        // range — the equivalence the covered period gets on ingestion.
        let range = parse_required_time_range(&[
            p("from", "1970-01-01T01:00:00+01:00"),
            p("to", "1970-01-01T03:00:00+02:00"),
        ])
        .expect("an offset-bearing pair parses");
        assert_eq!(
            range.lower_inclusive(),
            OffsetDateTime::UNIX_EPOCH,
            "`01:00:00+01:00` is the epoch instant",
        );
        assert_eq!(
            range.upper_exclusive(),
            OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
            "`03:00:00+02:00` is one hour after the epoch",
        );
        assert_eq!(
            range.lower_inclusive().offset(),
            UtcOffset::UTC,
            "normalization moves the offset, never the instant",
        );
        assert_eq!(range.upper_exclusive().offset(), UtcOffset::UTC);
    }

    #[test]
    fn an_absent_from_is_rejected_naming_from() {
        let err = parse_required_time_range(&[p("to", RANGE_TO)])
            .expect_err("an absent lower bound MUST reject");
        let (field, reason, description) = first_violation(&err);
        assert_eq!(field, "from");
        assert_eq!(reason, "VALIDATION");
        assert!(
            description.contains("missing required query parameter"),
            "description MUST cite the missing-required idiom (got `{description}`)",
        );
    }

    #[test]
    fn an_absent_to_is_rejected_naming_to() {
        // Separately from `from`: a helper that parsed only the lower bound
        // and defaulted the upper one would pass the test above.
        let err = parse_required_time_range(&[p("from", RANGE_FROM)])
            .expect_err("an absent upper bound MUST reject");
        let (field, _, _) = first_violation(&err);
        assert_eq!(field, "to");
    }

    #[test]
    fn a_duplicate_from_is_rejected_instead_of_silently_last_winning() {
        let err = parse_required_time_range(&[
            p("from", RANGE_FROM),
            p("from", "1999-01-01T00:00:00Z"),
            p("to", RANGE_TO),
        ])
        .expect_err("a duplicated lower bound MUST reject");
        let (field, _, description) = first_violation(&err);
        assert_eq!(field, "from");
        assert!(
            description.contains("at most once"),
            "duplicate-rejection description MUST cite the at-most-once \
             contract (got `{description}`)",
        );
    }

    #[test]
    fn an_offsetless_bound_is_rejected_rather_than_assumed_to_be_utc() {
        // A bare local time would attribute usage to whatever offset the
        // server happened to assume, so RFC 3339 (which requires an
        // offset) is the parser, not a lenient date-time reader. Pinned on
        // both bounds so a lenient parse on either one is caught.
        for (offsetless, other) in [("from", "to"), ("to", "from")] {
            let err = parse_required_time_range(&[
                p(offsetless, "2026-01-01T00:00:00"),
                p(other, if other == "to" { RANGE_TO } else { RANGE_FROM }),
            ])
            .expect_err("an offset-less timestamp MUST reject");
            let (field, reason, description) = first_violation(&err);
            assert_eq!(field, offsetless, "the violation MUST name the bad bound");
            assert_eq!(reason, "VALIDATION");
            assert!(
                description.contains("offset"),
                "description MUST say an offset is required (got `{description}`)",
            );
        }
    }

    #[test]
    fn a_date_only_bound_is_rejected() {
        let err = parse_required_time_range(&[p("from", "2026-01-01"), p("to", RANGE_TO)])
            .expect_err("a date without a time MUST reject");
        assert_eq!(first_violation(&err).0, "from");
    }

    #[test]
    fn an_inverted_range_is_rejected_naming_time_range() {
        // Both bounds parse; the ordering is what fails, so the violation
        // belongs to the range as a whole rather than to either parameter.
        let err = parse_required_time_range(&[p("from", RANGE_TO), p("to", RANGE_FROM)])
            .expect_err("an inverted range MUST reject");
        let (field, _, description) = first_violation(&err);
        assert_eq!(field, "time_range");
        assert!(
            description.contains("from < to"),
            "description MUST state the ordering contract (got `{description}`)",
        );
    }

    #[test]
    fn an_empty_range_is_rejected() {
        // `[from, to)` with equal bounds selects nothing. Rejected at
        // construction rather than dispatched as a guaranteed-empty read,
        // so the boundary is `to > from` and not `to >= from`.
        let err = parse_required_time_range(&[p("from", RANGE_FROM), p("to", RANGE_FROM)])
            .expect_err("an empty range MUST reject");
        assert_eq!(first_violation(&err).0, "time_range");
    }

    #[test]
    fn a_one_microsecond_range_is_accepted() {
        // The other side of the same boundary: the narrowest non-empty
        // range must pass, so the rejection above is about emptiness and
        // not about narrowness.
        let range = parse_required_time_range(&[
            p("from", "1970-01-01T00:00:00Z"),
            p("to", "1970-01-01T00:00:00.000001Z"),
        ])
        .expect("the narrowest non-empty range is valid");
        // Not `upper > lower` — that holds for every constructible
        // `TimeRange` and so asserts nothing. The exact width additionally
        // pins that the RFC 3339 parser kept the fractional second instead
        // of rounding it away.
        assert_eq!(
            range.upper_exclusive() - range.lower_inclusive(),
            time::Duration::microseconds(1),
        );
    }
}

// ---------------------------------------------------------------------------
// reject_unknown_list_params — the list allowlist admits both page-size
// spellings and nothing beyond the declared set.
//
// `toolkit::api::odata::ODataParams` declares `limit` with
// `#[serde(alias = "$top")]`, so `$top` and `limit` fold onto the same
// `ODataQuery.limit` slot. An allowlist entry with no binding behind it
// would be worse than a rejection — it would accept the parameter,
// ignore it, and hand back a full page the caller reads as their
// requested page — so each admitted spelling must be one the extractor
// actually binds.
// ---------------------------------------------------------------------------

mod reject_unknown_list_params_tests {
    use toolkit_canonical_errors::CanonicalError;
    use toolkit_canonical_errors::context::InvalidArgumentV1;

    use super::super::reject_unknown_list_params;

    fn p(key: &str, value: &str) -> (String, String) {
        (key.to_owned(), value.to_owned())
    }

    fn violating_field(err: &CanonicalError) -> Option<String> {
        let CanonicalError::InvalidArgument { ctx, .. } = err else {
            return None;
        };
        match ctx {
            InvalidArgumentV1::FieldViolations { field_violations } => {
                field_violations.first().map(|v| v.field.clone())
            }
            _ => None,
        }
    }

    #[test]
    fn allowed_params_pass() {
        reject_unknown_list_params(&[
            p("$filter", "resource_id eq 'r1'"),
            p("$orderby", "tenant_id asc"),
            p("limit", "10"),
            p("cursor", "opaque"),
            p("gts_type_id", "g"),
            p("from", "1970-01-01T00:00:00Z"),
            p("to", "1970-01-01T01:00:00Z"),
            p("metadata.user_id", "u1"),
        ])
        .expect("the list allowlist admits every documented parameter");
    }

    #[test]
    fn the_range_parameters_are_admitted_on_the_list_path() {
        // `from` / `to` are the raw path's carrier for the mandatory
        // covered-period range. Rejecting either here would refuse a
        // parameter the handler goes on to require, so the endpoint could
        // never be called successfully at all.
        for admitted in ["from", "to"] {
            reject_unknown_list_params(&[p("gts_type_id", "g"), p(admitted, "x")]).unwrap_or_else(
                |err| {
                    panic!("`{admitted}` MUST be admitted on the list path, got {err:?}");
                },
            );
        }
    }

    #[test]
    fn top_is_admitted_as_the_canonical_page_size_spelling() {
        // `$top` is canonical OData (OASIS OData 4.01 Part 2 §5.1.6) and
        // the toolkit extractor binds it as an alias of `limit`. Rejecting
        // it here would refuse a page size the platform honours.
        reject_unknown_list_params(&[p("gts_type_id", "g"), p("$top", "5")])
            .expect("`$top` MUST be admitted: the extractor binds it onto `ODataQuery.limit`");
    }

    #[test]
    fn select_is_rejected_because_no_projection_is_applied() {
        // The toolkit extractor parses `$select` into `ODataQuery.select`,
        // but this gear never reads that field: the handler returns whole
        // DTOs and the plugin selects a fixed column list. Admitting it
        // would answer `200` with every field to a caller who asked for
        // one, which reads as a satisfied projection rather than an
        // unsupported parameter.
        let err = reject_unknown_list_params(&[p("gts_type_id", "g"), p("$select", "id")])
            .expect_err("`$select` MUST be rejected: no code path applies the projection");
        assert_eq!(
            violating_field(&err).as_deref(),
            Some("$select"),
            "the violation MUST name `$select` so the caller learns which parameter is unsupported",
        );
    }

    #[test]
    fn unknown_parameter_is_rejected_with_field_naming_the_offender() {
        let err = reject_unknown_list_params(&[p("unknown_param", "x")])
            .expect_err("unknown param MUST reject");
        assert_eq!(violating_field(&err).as_deref(), Some("unknown_param"));
    }
}

// ---------------------------------------------------------------------------
// reject_unknown_aggregate_params — aggregate allowlist is STRICTER than list
//
// The list path admits `$top`, `cursor`, `$orderby`, and `limit`; the
// aggregate path intentionally rejects them (the aggregation result is
// not paginated). Verify the asymmetry directly — a regression that
// copy-pasted the list allowlist into the aggregate validator would not
// be caught by any other test.
// ---------------------------------------------------------------------------

mod reject_unknown_aggregate_params_tests {
    use toolkit_canonical_errors::CanonicalError;
    use toolkit_canonical_errors::context::InvalidArgumentV1;

    use super::super::reject_unknown_aggregate_params;

    fn p(key: &str, value: &str) -> (String, String) {
        (key.to_owned(), value.to_owned())
    }

    fn violating_field(err: &CanonicalError) -> Option<String> {
        let CanonicalError::InvalidArgument { ctx, .. } = err else {
            return None;
        };
        match ctx {
            InvalidArgumentV1::FieldViolations { field_violations } => {
                field_violations.first().map(|v| v.field.clone())
            }
            _ => None,
        }
    }

    #[test]
    fn allowed_params_pass() {
        // `$filter`, typed `gts_type_id`, and `metadata.<key>` entries are
        // explicitly admitted on the aggregate path.
        reject_unknown_aggregate_params(&[
            p("$filter", "x"),
            p("gts_type_id", "g"),
            p("metadata.user_id", "u1"),
        ])
        .expect("aggregate allowlist admits $filter, gts_type_id, metadata.<key>");
    }

    #[test]
    fn list_only_params_are_rejected_on_aggregate_path() {
        // Each of these is on `OUR_ODATA_PARAMS` for the LIST path but
        // NOT on `AGGREGATE_ODATA_PARAMS`. The aggregate validator MUST
        // reject them — silent admission would let a caller paginate an
        // unpaginated endpoint and ship an inconsistent wire contract.
        //
        // `$select` is deliberately absent: it is rejected on BOTH paths,
        // so it demonstrates no asymmetry. Its list-path rejection is
        // pinned by `reject_unknown_list_params_tests`.
        for forbidden in ["$top", "cursor", "limit", "$orderby"] {
            assert!(
                reject_unknown_aggregate_params(&[p(forbidden, "v")]).is_err(),
                "`{forbidden}` MUST be rejected on the aggregate path - \
                 list-only OData parameters cannot leak through here",
            );
        }
    }

    #[test]
    fn range_query_parameters_are_rejected_on_the_aggregate_path() {
        // The aggregate path reads its range from the request body
        // (`AggregationRequest.time_range`). Admitting the query-string
        // spelling too would take a range nothing reads and answer `200`
        // over whatever the body said — an accepted-and-ignored parameter,
        // which is the drift these allowlists exist to stop. It must be
        // named in a 400 instead.
        for body_only in ["from", "to"] {
            let err = reject_unknown_aggregate_params(&[
                p("gts_type_id", "g"),
                p(body_only, "1970-01-01T00:00:00Z"),
            ])
            .expect_err("a range query parameter MUST be rejected on the aggregate path");
            assert_eq!(
                violating_field(&err).as_deref(),
                Some(body_only),
                "the violation MUST name `{body_only}` so the caller learns to \
                 move the range into the body",
            );
        }
    }

    #[test]
    fn unknown_parameter_is_rejected_with_field_naming_the_offender() {
        // The violation MUST identify which parameter was unrecognised
        // so the caller can fix THAT parameter, not guess.
        let err = reject_unknown_aggregate_params(&[p("unknown_param", "x")])
            .expect_err("unknown param MUST reject");
        let field = match err {
            toolkit_canonical_errors::CanonicalError::InvalidArgument { ctx, .. } => match ctx {
                toolkit_canonical_errors::context::InvalidArgumentV1::FieldViolations {
                    field_violations,
                } => field_violations
                    .first()
                    .map(|v| v.field.clone())
                    .unwrap_or_default(),
                _ => String::new(),
            },
            _ => panic!("unknown-param rejection MUST be InvalidArgument"),
        };
        assert_eq!(field, "unknown_param");
    }
}

// ---------------------------------------------------------------------------
// handle_create_usage_records — batch-size guard.
//
// The handler mirrors the service's `1..=MAX_BATCH_RECORDS` gate. The
// short-circuit MUST refuse the request BEFORE the per-record loop
// allocates, so a regression that drops the gate would let the service
// see an oversized batch and a denial-of-service vector reopens.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_with_empty_batch_rejects_with_invalid_batch_size_before_service() {
    let (service, resolver) = service_with_sentinel_pdp();

    let response = handle_create_usage_records(
        Extension(SecurityContext::anonymous()),
        Extension(service),
        Json(CreateUsageRecordsRequest {
            records: Vec::new(),
        }),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "empty batch MUST lift to 400 (NOT 200 with an empty results array)",
    );
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.contains("problem+json"),
        "InvalidBatchSize MUST surface as application/problem+json (got `{content_type}`)",
    );
    assert_eq!(
        resolver.calls(),
        0,
        "batch-size gate MUST short-circuit BEFORE reaching the PDP",
    );
}

#[tokio::test]
async fn create_with_batch_above_cap_rejects_without_iterating_records() {
    use crate::domain::service::MAX_BATCH_RECORDS;

    let (service, resolver) = service_with_sentinel_pdp();

    // One over the cap: gate MUST refuse the request as a whole, NOT
    // walk the per-record loop and emit MAX_BATCH_RECORDS + 1 `Rejected`
    // entries.
    let oversize = MAX_BATCH_RECORDS + 1;
    let records: Vec<_> = (0..oversize)
        .map(|i| CreateUsageRecordRequest {
            gts_type_id: HAPPY_RECORD_GTS_ID.to_owned(),
            tenant_id: Uuid::from_u128(2),
            resource_ref: ResourceRefDto {
                resource_id: format!("rsc-{i}"),
                resource_type: "compute.vm".to_owned(),
            },
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: format!("idem-oversize-{i}"),
            invalidates: None,
            reason_code: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        })
        .collect();

    let response = handle_create_usage_records(
        Extension(SecurityContext::anonymous()),
        Extension(service),
        Json(CreateUsageRecordsRequest { records }),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "MAX_BATCH_RECORDS + 1 records MUST lift to 400 InvalidBatchSize",
    );

    // The wire payload MUST be a single canonical Problem (NOT a 207
    // envelope with per-record rejections) so the contract surface is
    // unambiguous about whether the batch was even considered.
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value =
        serde_json::from_slice(&body_bytes).expect("Problem body is JSON");
    assert!(
        body.get("results").is_none(),
        "oversize-batch rejection MUST NOT surface a 207-shape `results` envelope \
         (got body: {body})",
    );
    assert!(
        body.get("status").and_then(serde_json::Value::as_u64) == Some(400),
        "Problem body MUST carry status 400",
    );

    assert_eq!(
        resolver.calls(),
        0,
        "oversize-batch gate MUST short-circuit BEFORE reaching the PDP",
    );
}

// ---------------------------------------------------------------------------
// handle_list_usage_records — wire-boundary validation + round-trip
//
// The list handler is otherwise untested at the wire boundary. The
// service-layer tests cover authorize / compose / dispatch; these tests
// pin two handler-only concerns:
//
//   1. Pre-service validation rejections (missing / malformed `gts_type_id`,
//      unknown query parameter, cursor / filter-hash mismatch) surface
//      as a `400` canonical envelope before the service runs.
//   2. A successful list lifts the plugin's `Page<UsageRecord>` to the
//      wire as `Page<UsageRecordDto>` — items projected via
//      `UsageRecordDto::from`, `page_info` carried through verbatim.
// ---------------------------------------------------------------------------

mod handle_list_usage_records_tests {
    use std::sync::Arc;

    use axum::extract::{Extension, Query};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use toolkit::api::canonical_prelude::OData;
    use toolkit::client_hub::ClientHub;
    use toolkit_odata::{CursorV1, ODataQuery, Page as ODataPage, PageInfo, SortDir};
    use toolkit_security::{SecurityContext, pep_properties};
    use uuid::Uuid;

    use super::super::handle_list_usage_records;
    use super::{HAPPY_RECORD_GTS_ID, sample_persisted_record};
    use crate::domain::Service;
    use crate::domain::test_support::{
        CountingPermitResolver, CountingUnreachableResolver, HappyPathPlugin, ServiceFixture,
        authenticated_ctx, enforcer_for, fake_declaration_source_with_fold,
    };

    fn service_no_plugin() -> Arc<Service> {
        let hub = Arc::new(ClientHub::new());
        let resolver = CountingUnreachableResolver::new();
        let enforcer = enforcer_for(Arc::clone(&resolver) as _);
        Arc::new(Service::new(hub, "cyberfabric".to_owned(), enforcer))
    }

    /// A `Service` bound to `plugin` under a permit that pins the caller's
    /// tenant, with a `DeclarationSource` that resolves every meter —
    /// `list_usage_records` now resolves the queried meter's declaration
    /// (Spec §3.11 `metadata_filter` gating), so the default
    /// `UnavailableDeclarationSource` a bare `Service::new` carries would
    /// turn every happy-path call here into a `ServiceUnavailable` before
    /// ever reaching the plugin.
    fn service_with_permit_plugin(plugin: &Arc<HappyPathPlugin>, suffix: &str) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .with_resolver(CountingPermitResolver::new(
                pep_properties::OWNER_TENANT_ID,
                Uuid::from_u128(2).to_string(),
            ))
            .build(
                Arc::clone(plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
                suffix,
            )
    }

    #[tokio::test]
    async fn missing_gts_type_id_returns_400() {
        let service = service_no_plugin();

        let response = handle_list_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(super::list_params(&[])),
            OData(ODataQuery::new()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            super::first_violation_field(response).await.as_deref(),
            Some("gts_type_id"),
            "the 400 MUST blame this test's own subject: a status-only \
             assertion would stay green if the validator order flipped and a \
             different parameter became the blamed one",
        );
    }

    #[tokio::test]
    async fn malformed_gts_type_id_returns_400() {
        // A single but shape-invalid `gts_type_id` must surface through the
        // SDK's `MeterTypeId` mapping as a 400 — covering
        // gts_type_id-shape rejections, not just missing / duplicate /
        // unknown-param.
        let service = service_no_plugin();

        let response = handle_list_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(super::list_params(&[("gts_type_id", "not-a-valid-prefix")])),
            OData(ODataQuery::new()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            super::first_violation_field(response).await.as_deref(),
            Some("gts_type_id"),
            "the 400 MUST blame this test's own subject: a status-only \
             assertion would stay green if the validator order flipped and a \
             different parameter became the blamed one",
        );
    }

    #[tokio::test]
    async fn unknown_query_parameter_returns_400() {
        // A parameter that is neither an OData token, a typed
        // (`gts_type_id`), nor a `metadata.<key>` entry MUST be refused
        // rather than silently dropped — silent drop is a documented
        // contract-drift surface.
        let service = service_no_plugin();

        let response = handle_list_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(super::list_params(&[
                ("gts_type_id", HAPPY_RECORD_GTS_ID),
                ("totally_unknown", "x"),
            ])),
            OData(ODataQuery::new()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            super::first_violation_field(response).await.as_deref(),
            Some("totally_unknown"),
            "the 400 MUST blame this test's own subject: a status-only \
             assertion would stay green if the validator order flipped and a \
             different parameter became the blamed one",
        );
    }

    #[tokio::test]
    async fn a_valid_cursor_paginates_and_the_plugin_gets_the_bound_order() {
        // The seam the keyset floor's two modes create: the handler skips
        // normalization on a cursor request and rebuilds the order from
        // the token's signed keys, while the service then REQUIRES that
        // order to be a sound keyset. The wire outcome therefore depends
        // on those two agreeing, and every other full-stack cursor test
        // here is a 400 — so nothing drove a successful continuation end
        // to end.
        //
        // `f` carries the fingerprint the service binds a continuation to
        // — the caller's `$filter` and all three typed parameters — because a
        // token bound to anything else is refused before the order is ever
        // reached. The order is still the subject; the fingerprint is the
        // precondition.
        let plugin = HappyPathPlugin::new();
        plugin.set_list_usage_records_response(ODataPage::empty(0));
        let service = service_with_permit_plugin(&plugin, "test.handler.list_records.cursor.v1");

        let mut q = ODataQuery::new();
        q.cursor = Some(CursorV1 {
            k: vec!["2026-06-12T00:00:00Z".into(), uuid::Uuid::nil().to_string()],
            o: SortDir::Asc,
            s: "+window_end,+id".to_owned(),
            f: Some(super::list_fingerprint(&ODataQuery::new())),
            d: "fwd".to_owned(),
        });
        // Note: `q.order` intentionally empty, mirroring the toolkit OData
        // extractor on a cursor request.

        let response = handle_list_usage_records(
            Extension(authenticated_ctx()),
            Extension(service),
            Query(super::list_params(&[("gts_type_id", HAPPY_RECORD_GTS_ID)])),
            OData(q),
        )
        .await
        .into_response();

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "a continuation bound to the canonical keyset MUST paginate",
        );
        assert_eq!(
            plugin
                .last_list_order()
                .expect("the plugin MUST have been dispatched"),
            vec![
                ("window_end".to_owned(), SortDir::Asc),
                ("id".to_owned(), SortDir::Asc),
            ],
            "the plugin MUST receive the order the token was minted under, \
             rebuilt from its signed keys and neither defaulted nor widened",
        );
    }

    #[tokio::test]
    async fn a_cursor_bound_to_an_unsound_keyset_returns_400_naming_the_cursor() {
        // The other half of the seam. A token whose signed keys omit the
        // period end decodes fine and passes cursor validation, then the
        // service refuses it rather than widening the order past the two
        // boundary values the token carries. The `400` must blame `cursor`:
        // the caller sent no `$orderby` on this request and could not.
        let plugin = HappyPathPlugin::new();
        plugin.set_list_usage_records_response(ODataPage::empty(0));
        let service = service_with_permit_plugin(&plugin, "test.handler.list_records.badcursor.v1");

        let mut q = ODataQuery::new();
        q.cursor = Some(CursorV1 {
            k: vec!["r1".into(), uuid::Uuid::nil().to_string()],
            o: SortDir::Asc,
            s: "+resource_id,+id".to_owned(),
            // The fingerprint matches, so the refusal below is the keyset
            // check and not the query-mismatch check next to it.
            f: Some(super::list_fingerprint(&ODataQuery::new())),
            d: "fwd".to_owned(),
        });

        let response = handle_list_usage_records(
            Extension(authenticated_ctx()),
            Extension(service),
            Query(super::list_params(&[("gts_type_id", HAPPY_RECORD_GTS_ID)])),
            OData(q),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            super::first_violation_field(response).await.as_deref(),
            Some("cursor"),
        );
        assert!(
            plugin.last_list_order().is_none(),
            "a refused continuation MUST NOT reach the plugin",
        );
    }

    #[tokio::test]
    async fn a_cursor_minted_over_another_query_returns_400_naming_the_cursor() {
        // A continuation whose bound query is not this request's MUST be
        // refused rather than silently resumed over a different row set.
        // The comparison moved behind the service when the read range left
        // `$filter`: the edge's own `filter_hash` covers `$filter` alone,
        // and an in-process caller has none at all, so the service is the
        // only owner that can hold both surfaces to the rule. This test
        // therefore needs a service that gets as far as the read path,
        // where the earlier edge-only version needed no plugin at all.
        let plugin = HappyPathPlugin::new();
        plugin.set_list_usage_records_response(ODataPage::empty(0));
        let service =
            service_with_permit_plugin(&plugin, "test.handler.list_records.stale_cursor.v1");

        let mut q = ODataQuery::new();
        q.cursor = Some(CursorV1 {
            k: vec!["2026-06-12T00:00:00Z".into(), uuid::Uuid::nil().to_string()],
            o: SortDir::Asc,
            s: "+window_end,+id".to_owned(),
            f: Some("minted_over_something_else".into()),
            d: "fwd".to_owned(),
        });

        let response = handle_list_usage_records(
            Extension(authenticated_ctx()),
            Extension(service),
            Query(super::list_params(&[("gts_type_id", HAPPY_RECORD_GTS_ID)])),
            OData(q),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            super::first_violation_field(response).await.as_deref(),
            Some("cursor"),
            "the 400 MUST blame the token the caller sent",
        );
        assert!(
            plugin.last_list_order().is_none(),
            "a cursor bound to another query MUST NOT reach the plugin: \
             being served is a wrong page, and a wrong page is a 200",
        );
    }

    #[tokio::test]
    async fn a_page_two_carrying_only_a_cursor_change_is_served_over_rest() {
        // The legitimate continuation, end to end over REST and with a
        // `$filter` present, which is what makes it the regression test
        // for the edge giving up its own comparison: the extractor's
        // `filter_hash` is a hash of `$filter` alone, so an edge still
        // comparing it against the wider fingerprint the plugin minted
        // would reject exactly this request.
        //
        // Page one is dispatched for real and its fingerprint read off
        // what the plugin received, so nothing here restates a value the
        // domain owns.
        let plugin = HappyPathPlugin::new();
        plugin.set_list_usage_records_response(ODataPage::empty(0));
        let service = service_with_permit_plugin(&plugin, "test.handler.list_records.page_two.v1");

        let mut page_one = ODataQuery::from(Some(
            toolkit_odata::parse_filter_string("resource_id eq 'r1'")
                .expect("test filter parses")
                .into_expr(),
        ));
        // Mirror the toolkit extractor: it hashes the `$filter` alone.
        page_one.filter_hash = toolkit_odata::short_filter_hash(page_one.filter());

        let response = handle_list_usage_records(
            Extension(authenticated_ctx()),
            Extension(Arc::clone(&service)),
            Query(super::list_params(&[("gts_type_id", HAPPY_RECORD_GTS_ID)])),
            OData(page_one.clone()),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK, "page one must be served");

        let minted = plugin
            .last_list_query()
            .expect("page one MUST have been dispatched")
            .filter_hash
            .expect("every dispatch MUST carry the fingerprint the plugin mints");

        let mut page_two = page_one;
        page_two.cursor = Some(CursorV1 {
            k: vec!["2026-06-12T00:00:00Z".into(), uuid::Uuid::nil().to_string()],
            o: SortDir::Asc,
            s: "+window_end,+id".to_owned(),
            f: Some(minted),
            d: "fwd".to_owned(),
        });

        let response = handle_list_usage_records(
            Extension(authenticated_ctx()),
            Extension(service),
            Query(super::list_params(&[("gts_type_id", HAPPY_RECORD_GTS_ID)])),
            OData(page_two),
        )
        .await
        .into_response();

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "page two of an unchanged query MUST be served: the cursor the \
             plugin minted is bound to the filter AND the range, so an edge \
             comparing its filter-only hash would refuse every one of them",
        );
    }

    #[tokio::test]
    async fn list_rejects_a_missing_from_parameter() {
        // The range is mandatory on the wire. Without it the read would be
        // an unbounded scan, so the handler MUST refuse before the service
        // runs rather than defaulting a bound.
        let service = service_no_plugin();

        let response = handle_list_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(vec![
                ("gts_type_id".to_owned(), HAPPY_RECORD_GTS_ID.to_owned()),
                ("to".to_owned(), super::RANGE_TO.to_owned()),
            ]),
            OData(ODataQuery::new()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            super::first_violation_field(response).await.as_deref(),
            Some("from"),
            "the violation MUST name the absent parameter",
        );
    }

    #[tokio::test]
    async fn list_rejects_a_missing_to_parameter() {
        let service = service_no_plugin();

        let response = handle_list_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(vec![
                ("gts_type_id".to_owned(), HAPPY_RECORD_GTS_ID.to_owned()),
                ("from".to_owned(), super::RANGE_FROM.to_owned()),
            ]),
            OData(ODataQuery::new()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            super::first_violation_field(response).await.as_deref(),
            Some("to"),
        );
    }

    #[tokio::test]
    async fn list_rejects_an_offsetless_from_parameter() {
        // `2026-01-01T00:00:00` carries no offset. A silent UTC assumption
        // would attribute usage to whatever offset the server happened to
        // pick, so this is a 400 rather than a lenient parse.
        let service = service_no_plugin();

        let response = handle_list_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(vec![
                ("gts_type_id".to_owned(), HAPPY_RECORD_GTS_ID.to_owned()),
                ("from".to_owned(), "2026-01-01T00:00:00".to_owned()),
                ("to".to_owned(), super::RANGE_TO.to_owned()),
            ]),
            OData(ODataQuery::new()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            super::first_violation_field(response).await.as_deref(),
            Some("from"),
        );
    }

    #[tokio::test]
    async fn list_rejects_an_inverted_range() {
        let service = service_no_plugin();

        let response = handle_list_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(vec![
                ("gts_type_id".to_owned(), HAPPY_RECORD_GTS_ID.to_owned()),
                ("from".to_owned(), "2026-02-01T00:00:00Z".to_owned()),
                ("to".to_owned(), "2026-01-01T00:00:00Z".to_owned()),
            ]),
            OData(ODataQuery::new()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            super::first_violation_field(response).await.as_deref(),
            Some("time_range"),
            "both bounds parse; the ordering is what failed, so the violation \
             belongs to the range rather than to either parameter",
        );
    }

    #[tokio::test]
    async fn list_rejects_a_duplicate_from_parameter() {
        // Two different lower bounds: last-wins would silently pick one and
        // mask the caller bug.
        let service = service_no_plugin();

        let response = handle_list_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(vec![
                ("gts_type_id".to_owned(), HAPPY_RECORD_GTS_ID.to_owned()),
                ("from".to_owned(), super::RANGE_FROM.to_owned()),
                ("from".to_owned(), "1999-01-01T00:00:00Z".to_owned()),
                ("to".to_owned(), super::RANGE_TO.to_owned()),
            ]),
            OData(ODataQuery::new()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            super::first_violation_field(response).await.as_deref(),
            Some("from"),
        );
    }

    #[tokio::test]
    async fn list_forwards_the_parsed_wire_range_to_the_plugin() {
        // The whole point of the wire parameters: the range a caller sent
        // must arrive at the SPI as a typed `TimeRange`. A handler that
        // dropped or substituted it would still answer `200` with a page,
        // so the assertion is on what the plugin was handed.
        let plugin = HappyPathPlugin::new();
        plugin.set_list_usage_records_response(ODataPage::empty(0));
        let service = service_with_permit_plugin(&plugin, "test.handler.list_records.range.v1");

        let response = handle_list_usage_records(
            Extension(authenticated_ctx()),
            Extension(service),
            Query(super::list_params(&[("gts_type_id", HAPPY_RECORD_GTS_ID)])),
            OData(ODataQuery::new()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let range = plugin
            .last_list_time_range()
            .expect("the plugin MUST have been dispatched");
        assert_eq!(
            range.lower_inclusive(),
            time::OffsetDateTime::UNIX_EPOCH,
            "`from` MUST become the inclusive lower bound",
        );
        assert_eq!(
            range.upper_exclusive(),
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
            "`to` MUST become the exclusive upper bound",
        );
    }

    #[tokio::test]
    async fn happy_path_maps_page_items_through_dto_and_preserves_page_info() {
        // Plugin returns a 2-item `Page<UsageRecord>` with a non-default
        // `PageInfo`. The handler MUST:
        //   1. project each item via `UsageRecordDto::from` (id + the
        //      derived `entry_type` are the cheapest, regression-prone
        //      witnesses), and
        //   2. carry `page_info` verbatim (`next_cursor`, `prev_cursor`,
        //      `limit`).
        let plugin = HappyPathPlugin::new();
        let item_a = sample_persisted_record(Uuid::new_v4(), Uuid::from_u128(2));
        let item_b = sample_persisted_record(Uuid::new_v4(), Uuid::from_u128(2));
        let expected_uuids = [item_a.id, item_b.id];
        plugin.set_list_usage_records_response(ODataPage::new(
            vec![item_a, item_b],
            PageInfo {
                next_cursor: Some("next-cursor-blob".to_owned()),
                prev_cursor: Some("prev-cursor-blob".to_owned()),
                limit: 137,
            },
        ));

        let service = service_with_permit_plugin(&plugin, "test.handler.list_records.happy.v1");

        let response = handle_list_usage_records(
            Extension(authenticated_ctx()),
            Extension(service),
            Query(super::list_params(&[("gts_type_id", HAPPY_RECORD_GTS_ID)])),
            OData(ODataQuery::new()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body collected");
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");

        let items = body
            .get("items")
            .and_then(serde_json::Value::as_array)
            .expect("wire body MUST carry `items`");
        assert_eq!(items.len(), 2);
        let actual_uuids: Vec<_> = items
            .iter()
            .map(|i| {
                i.get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            actual_uuids,
            expected_uuids.map(|u| u.to_string()).to_vec(),
            "items[i].id MUST echo plugin order (preserves keyset \
             pagination contract)",
        );
        for (i, item) in items.iter().enumerate() {
            assert_eq!(
                item.get("entry_type").and_then(serde_json::Value::as_str),
                Some("record"),
                "items[{i}].entry_type MUST be the derived lowercase \
                 `record` (projection through UsageRecordDto::from)",
            );
        }

        let page_info = body
            .get("page_info")
            .expect("wire body MUST carry `page_info`");
        assert_eq!(
            page_info
                .get("next_cursor")
                .and_then(serde_json::Value::as_str),
            Some("next-cursor-blob"),
        );
        assert_eq!(
            page_info
                .get("prev_cursor")
                .and_then(serde_json::Value::as_str),
            Some("prev-cursor-blob"),
        );
        assert_eq!(
            page_info.get("limit").and_then(serde_json::Value::as_u64),
            Some(137),
            "page_info.limit MUST carry the plugin-reported limit \
             verbatim (NOT the gateway MAX_PAGE_SIZE cap)",
        );
    }
}

// ---------------------------------------------------------------------------
// handle_query_aggregated_usage_records — wire validation + body projection
//
// The aggregate handler shares the metadata + gts_type_id surface with list,
// but its allowlist is stricter (no `$top`, no `cursor`) and it ships only
// group-by dimensions in the body — no aggregation parameter; the fold is
// resolved from the queried type's declaration. The tests here pin only
// what list tests can't: the stricter allowlist, the body lift through
// `AggregationRequestDto::into_group_by`, and the result
// projection through `AggregationResultDto`.
// ---------------------------------------------------------------------------

mod handle_query_aggregated_usage_records_tests {
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use axum::Json;
    use axum::extract::{Extension, Query};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use bigdecimal::BigDecimal;
    use toolkit::api::canonical_prelude::OData;
    use toolkit::client_hub::ClientHub;
    use toolkit_odata::ODataQuery;
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{AggregationBucket, AggregationResult};

    use super::super::handle_query_aggregated_usage_records;
    use crate::api::rest::dto::{AggregationDimensionDto, AggregationRequestDto};
    use crate::domain::Service;
    use crate::domain::test_support::{
        CountingUnreachableResolver, RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture,
        authenticated_ctx, enforcer_for, fake_declaration_source_with_fold,
        recording_plugin_resolver,
    };

    const VALID_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn service_no_plugin() -> Arc<Service> {
        let hub = Arc::new(ClientHub::new());
        let resolver = CountingUnreachableResolver::new();
        let enforcer = enforcer_for(Arc::clone(&resolver) as _);
        Arc::new(Service::new(hub, "cyberfabric".to_owned(), enforcer))
    }

    fn no_group() -> AggregationRequestDto {
        AggregationRequestDto {
            time_range: super::range_body(),
            group_by: Vec::new(),
        }
    }

    #[tokio::test]
    async fn missing_gts_type_id_on_aggregate_path_returns_400() {
        // The aggregate path uses the same `parse_required_gts_type_id`
        // helper as list; a missing `gts_type_id` MUST be refused with a 400
        // here too.
        let service = service_no_plugin();

        let response = handle_query_aggregated_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(Vec::new()),
            OData(ODataQuery::new()),
            Json(no_group()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            super::first_violation_field(response).await.as_deref(),
            Some("gts_type_id"),
        );
    }

    #[tokio::test]
    async fn malformed_gts_type_id_on_aggregate_path_returns_400() {
        // Parallel to the list-side `malformed_gts_type_id_returns_400` test:
        // a shape-invalid `gts_type_id` on the aggregate path MUST surface
        // through `parse_required_gts_type_id` as a 400.
        let service = service_no_plugin();

        let response = handle_query_aggregated_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(vec![(
                "gts_type_id".to_owned(),
                "not-a-valid-prefix".to_owned(),
            )]),
            OData(ODataQuery::new()),
            Json(no_group()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn cursor_parameter_is_rejected_on_aggregate_path() {
        // `cursor` is allowed by the LIST allowlist but NOT by the
        // aggregate allowlist — aggregation is not paginated. The
        // handler MUST refuse it with a 400.
        let service = service_no_plugin();

        let response = handle_query_aggregated_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(vec![
                ("gts_type_id".to_owned(), VALID_GTS_ID.to_owned()),
                ("cursor".to_owned(), "any-blob".to_owned()),
            ]),
            OData(ODataQuery::new()),
            Json(no_group()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            super::first_violation_field(response).await.as_deref(),
            Some("cursor"),
            "the 400 MUST blame `cursor` rather than some other parameter \
             this request also carries",
        );
    }

    #[tokio::test]
    async fn aggregation_body_with_invalid_metadata_key_lifts_through_tryfrom() {
        // `AggregationDimension::Metadata` carries a typed
        // `MetadataKey` — an empty / oversized key in the body MUST
        // surface through `AggregationRequestDto::into_group_by`'s
        // host-side canonical lift, NOT bypass the typed boundary. Pinned
        // with the empty-string key, which the SDK rejects on
        // `MetadataKey::new`.
        let service = service_no_plugin();

        let response = handle_query_aggregated_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(vec![("gts_type_id".to_owned(), VALID_GTS_ID.to_owned())]),
            OData(ODataQuery::new()),
            Json(AggregationRequestDto {
                time_range: super::range_body(),
                group_by: vec![AggregationDimensionDto::Metadata(String::new())],
            }),
        )
        .await
        .into_response();

        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "empty metadata-dimension key MUST refuse at the handler boundary",
        );
    }

    #[tokio::test]
    async fn aggregate_rejects_a_from_query_parameter() {
        // The aggregate path reads its range from the body. Accepting the
        // query-string spelling as well would take a range nothing reads
        // and answer `200` over whatever the body said, so it MUST be named
        // in a 400 instead of ignored.
        let service = service_no_plugin();

        let response = handle_query_aggregated_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(vec![
                ("gts_type_id".to_owned(), VALID_GTS_ID.to_owned()),
                ("from".to_owned(), "1970-01-01T00:00:00Z".to_owned()),
            ]),
            OData(ODataQuery::new()),
            Json(no_group()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            super::first_violation_field(response).await.as_deref(),
            Some("from"),
            "the violation MUST name the parameter so the caller learns to \
             move the range into the body",
        );
    }

    #[tokio::test]
    async fn aggregate_rejects_an_inverted_body_range() {
        let service = service_no_plugin();

        let response = handle_query_aggregated_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(vec![("gts_type_id".to_owned(), VALID_GTS_ID.to_owned())]),
            OData(ODataQuery::new()),
            Json(AggregationRequestDto {
                time_range: crate::api::rest::dto::TimeRangeDto {
                    from: super::EPOCH_PLUS_ONE_HOUR,
                    to: time::OffsetDateTime::UNIX_EPOCH,
                },
                group_by: Vec::new(),
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            super::first_violation_field(response).await.as_deref(),
            Some("time_range"),
        );
    }

    #[tokio::test]
    async fn aggregate_forwards_the_body_range_to_the_plugin() {
        // Mirror of the raw path's forwarding test, across the other
        // carrier: the range in `AggregationRequest.time_range` must reach
        // the SPI as a typed `TimeRange`.
        let plugin = RecordingPlugin::new();
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
                RECORDING_PLUGIN_SUFFIX,
            );
        plugin.set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });

        let response = handle_query_aggregated_usage_records(
            Extension(authenticated_ctx()),
            Extension(service),
            Query(vec![("gts_type_id".to_owned(), VALID_GTS_ID.to_owned())]),
            OData(ODataQuery::new()),
            Json(no_group()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let range = plugin
            .last_aggregate_time_range()
            .expect("the plugin MUST have been dispatched");
        assert_eq!(range.lower_inclusive(), time::OffsetDateTime::UNIX_EPOCH);
        assert_eq!(
            range.upper_exclusive(),
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        );
    }

    #[tokio::test]
    async fn happy_path_projects_aggregation_result_through_dto() {
        // Plugin returns a 2-bucket `AggregationResult`; the handler
        // MUST surface a 200 OK body whose `buckets` array projects
        // each bucket through `AggregationBucketDto` — `key` carried
        // verbatim, `value` serialised as a decimal string per the
        // `bigdecimal_str_option` contract. The service resolves the fold
        // from the declaration (there is no `op` on the wire any more), so
        // this wires a working Type Resolver rather than the plugin-side
        // catalog this test used before the catalog was retired.
        let source = fake_declaration_source_with_fold("SUM");
        let plugin = RecordingPlugin::new();
        let service = ServiceFixture::default()
            .with_source(source)
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
                RECORDING_PLUGIN_SUFFIX,
            );
        plugin.set_query_aggregated_usage_records_response(AggregationResult {
            buckets: vec![
                AggregationBucket {
                    key: vec!["eu".to_owned()],
                    value: Some(BigDecimal::from(42)),
                },
                AggregationBucket {
                    key: vec!["us".to_owned()],
                    value: None,
                },
            ],
        });

        let response = handle_query_aggregated_usage_records(
            Extension(authenticated_ctx()),
            Extension(service),
            Query(vec![("gts_type_id".to_owned(), VALID_GTS_ID.to_owned())]),
            OData(ODataQuery::new()),
            Json(AggregationRequestDto {
                time_range: super::range_body(),
                group_by: vec![AggregationDimensionDto::ResourceType],
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body collected");
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");
        let buckets = body
            .get("buckets")
            .and_then(serde_json::Value::as_array)
            .expect("wire body MUST carry `buckets`");
        assert_eq!(buckets.len(), 2);

        assert_eq!(
            buckets[0]
                .get("key")
                .and_then(serde_json::Value::as_array)
                .and_then(|a| a.first())
                .and_then(serde_json::Value::as_str),
            Some("eu"),
        );
        assert_eq!(
            buckets[0].get("value").and_then(serde_json::Value::as_str),
            Some("42"),
            "non-empty bucket value MUST serialise as the decimal string \
             form, NOT a JSON number (the float-round-trip safety \
             requires the bigdecimal_str_option codec)",
        );
        assert!(
            buckets[1]
                .get("value")
                .is_none_or(serde_json::Value::is_null),
            "empty-set bucket value MUST serialise as null per the \
             SDK contract for `MIN over an empty set` etc.",
        );
    }
}

// ---------------------------------------------------------------------------
// handle_backfill_usage_records — POST /usage-collector/v1/records/backfill
//
// The backfill route shares the live route's whole handler body
// (`dispatch_usage_record_batch`); the dispatched `Service` entry point is
// the only variation, and with it the `RecordOrigin` the gateway stamps.
// So the tests here pin exactly what the shared body cannot: that this
// handler dispatches to `Service::backfill_usage_records`, evidenced by
// the origin on the record the plugin is HANDED — not by the origin on the
// record the fixture hands back, which would be asserting the fixture.
//
// The envelope itself (200 all-accepted / 207 any-rejected, index-ordered
// results) is asserted here too, because it is the published contract of
// this route even though the code realizing it is shared.
// ---------------------------------------------------------------------------

mod handle_backfill_usage_records_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use axum::Json;
    use axum::extract::Extension;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use usage_collector_sdk::{CreateUsageRecord, IdempotencyKey, MeterTypeId, RecordOrigin};
    use uuid::Uuid;

    use super::super::handle_backfill_usage_records;
    use super::{HAPPY_RECORD_GTS_ID, ResourceRefDto};
    use crate::api::rest::dto::{CreateUsageRecordRequest, CreateUsageRecordsRequest};
    use crate::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, fake_declaration_source_with_fold,
        projected_with_origin, recent_window_end, recent_window_start,
    };

    /// One valid wire record against the happy-path meter.
    fn wire_record(tenant_id: Uuid, idem: &str) -> CreateUsageRecordRequest {
        CreateUsageRecordRequest {
            gts_type_id: HAPPY_RECORD_GTS_ID.to_owned(),
            tenant_id,
            resource_ref: ResourceRefDto {
                resource_id: "rsc-backfill".to_owned(),
                resource_type: "compute.vm".to_owned(),
            },
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: idem.to_owned(),
            invalidates: None,
            reason_code: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    /// The domain submission the gateway derives from [`wire_record`], so
    /// the echo fixture can be programmed with the same projection the
    /// service will produce.
    fn domain_submission(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            gts_type_id: MeterTypeId::new(HAPPY_RECORD_GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: usage_collector_sdk::ResourceRef::new("rsc-backfill", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    fn service_and_plugin(
        results: Vec<
            Result<
                usage_collector_sdk::UsageRecord,
                usage_collector_sdk::UsageCollectorPluginError,
            >,
        >,
        instance: &str,
    ) -> (Arc<crate::domain::Service>, Arc<HappyPathPlugin>) {
        let plugin = HappyPathPlugin::new();
        plugin.set_create_records(results);
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(
                Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
                instance,
            );
        (service, plugin)
    }

    async fn wire_body(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body collected");
        serde_json::from_slice(&bytes).expect("body is JSON")
    }

    /// An all-accepted backfill batch is `200 OK`, and the entries it
    /// returns carry `origin: backfill`.
    ///
    /// The load-bearing assertion is on the record the plugin was HANDED:
    /// `forwarded[0].origin == Backfill` is what fails if this handler
    /// dispatches to `Service::create_usage_records` instead. The wire
    /// assertion rides on the echo fixture, which is programmed through
    /// `projected_with_origin(.., Backfill)` precisely so a live-stamping
    /// regression cannot be papered over by a `Live` projection agreeing
    /// with itself.
    #[tokio::test]
    async fn backfill_all_accepted_is_200_and_stamps_origin_backfill() {
        let tenant_id = Uuid::from_u128(7);
        let submission = domain_submission(tenant_id, "idem-backfill-0");
        let echoed = projected_with_origin(&submission, RecordOrigin::Backfill);
        let persisted_id = echoed.id;
        let (service, plugin) = service_and_plugin(
            vec![Ok(echoed)],
            "test.handler.backfill_records.accepted.v1",
        );

        let req = CreateUsageRecordsRequest {
            records: vec![wire_record(tenant_id, "idem-backfill-0")],
        };

        let response = handle_backfill_usage_records(
            Extension(authenticated_ctx()),
            Extension(service),
            Json(req),
        )
        .await
        .into_response();

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "an all-accepted backfill batch MUST surface as 200 OK, not 207: \
             the backfill route publishes the same envelope as POST /records",
        );

        let body = wire_body(response).await;
        let results = body
            .get("results")
            .and_then(serde_json::Value::as_array)
            .expect("response carries a `results` array");
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0]
                .get("outcome")
                .and_then(serde_json::Value::as_str),
            Some("accepted"),
        );
        let record = results[0]
            .get("record")
            .expect("accepted item carries `record`");
        assert_eq!(
            record.get("id").and_then(serde_json::Value::as_str),
            Some(persisted_id.to_string().as_str()),
            "wire body MUST echo the service-returned record",
        );
        assert_eq!(
            record.get("origin").and_then(serde_json::Value::as_str),
            Some("backfill"),
            "an entry accepted on the backfill route MUST come back \
             `origin: backfill`",
        );

        let forwarded = plugin
            .last_create_records_input()
            .expect("plugin received the eligible batch");
        assert_eq!(forwarded.len(), 1);
        assert_eq!(
            forwarded[0].origin,
            RecordOrigin::Backfill,
            "the handler MUST dispatch to `Service::backfill_usage_records`; \
             a dispatch to `create_usage_records` hands the plugin an entry \
             stamped `Live`",
        );
    }

    /// A backfill batch with at least one rejection is `207 Multi-Status`,
    /// and the per-entry results stay ordered by input index.
    ///
    /// Input index 1 is rejected at the handler fold (bad `gts_type_id`
    /// prefix) and never reaches the service, so the accepted entries sit
    /// at indices 0 and 2 with a gap between them — which is exactly the
    /// bookkeeping a second copy of the shared body would break.
    #[tokio::test]
    async fn backfill_with_one_rejection_is_207_and_preserves_input_order() {
        let tenant_id = Uuid::from_u128(8);
        let echoed_0 = projected_with_origin(
            &domain_submission(tenant_id, "idem-backfill-mixed-0"),
            RecordOrigin::Backfill,
        );
        let echoed_2 = projected_with_origin(
            &domain_submission(tenant_id, "idem-backfill-mixed-2"),
            RecordOrigin::Backfill,
        );
        let (persisted_0, persisted_2) = (echoed_0.id, echoed_2.id);
        assert_ne!(persisted_0, persisted_2, "test premise: distinct entries");
        let (service, plugin) = service_and_plugin(
            vec![Ok(echoed_0), Ok(echoed_2)],
            "test.handler.backfill_records.mixed.v1",
        );

        let mut bad = wire_record(tenant_id, "idem-backfill-mixed-1");
        bad.gts_type_id = "not-a-valid-prefix".to_owned();
        let req = CreateUsageRecordsRequest {
            records: vec![
                wire_record(tenant_id, "idem-backfill-mixed-0"),
                bad,
                wire_record(tenant_id, "idem-backfill-mixed-2"),
            ],
        };

        let response = handle_backfill_usage_records(
            Extension(authenticated_ctx()),
            Extension(service),
            Json(req),
        )
        .await
        .into_response();

        assert_eq!(
            response.status(),
            StatusCode::MULTI_STATUS,
            "any rejection in a backfill batch MUST surface as 207 Multi-Status",
        );

        let body = wire_body(response).await;
        let results = body
            .get("results")
            .and_then(serde_json::Value::as_array)
            .expect("response carries a `results` array");
        assert_eq!(results.len(), 3);

        let outcome = |i: usize| {
            results[i]
                .get("outcome")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
        };
        let index_field = |i: usize| results[i].get("index").and_then(serde_json::Value::as_u64);

        assert_eq!(outcome(0), "accepted");
        assert_eq!(index_field(0), Some(0));
        assert_eq!(
            results[0]
                .get("record")
                .and_then(|r| r.get("id"))
                .and_then(serde_json::Value::as_str),
            Some(persisted_0.to_string().as_str()),
        );
        assert_eq!(
            results[0]
                .get("record")
                .and_then(|r| r.get("origin"))
                .and_then(serde_json::Value::as_str),
            Some("backfill"),
        );

        assert_eq!(outcome(1), "rejected", "input index 1 MUST be rejected");
        assert_eq!(index_field(1), Some(1));

        assert_eq!(outcome(2), "accepted");
        assert_eq!(index_field(2), Some(2));
        assert_eq!(
            results[2]
                .get("record")
                .and_then(|r| r.get("id"))
                .and_then(serde_json::Value::as_str),
            Some(persisted_2.to_string().as_str()),
            "the second accepted entry MUST land at input index 2, not at the \
             index it held in the eligible sub-batch",
        );

        let forwarded = plugin
            .last_create_records_input()
            .expect("plugin received the eligible batch");
        assert_eq!(
            forwarded.len(),
            2,
            "plugin MUST receive only the handler-validated records",
        );
        for entry in &forwarded {
            assert_eq!(
                entry.origin,
                RecordOrigin::Backfill,
                "every entry of a backfill batch MUST reach the plugin \
                 stamped `Backfill`",
            );
        }
    }

    /// `origin` is server-assigned: it records the route an entry
    /// travelled, so a caller that could name it could file live traffic
    /// as history (or the reverse) and defeat the marker's whole purpose.
    /// `CreateUsageRecordRequest`'s `deny_unknown_fields` is what refuses
    /// it, and this pins that the refusal survives — on the body shape
    /// this route accepts, which is the same one `POST /records` accepts.
    ///
    /// The accepting half is the anchor: without it, a rename or a removal
    /// of some unrelated required field would make the rejecting half pass
    /// for the wrong reason.
    #[test]
    fn a_request_body_naming_its_own_origin_is_refused() {
        let record = || {
            serde_json::json!({
                "gts_type_id": HAPPY_RECORD_GTS_ID,
                "tenant_id": Uuid::from_u128(9).to_string(),
                "resource_ref": {
                    "resource_id": "rsc-backfill",
                    "resource_type": "compute.vm",
                },
                "value": "1",
                "idempotency_key": "idem-backfill-origin",
                "window_start": "2026-07-07T00:00:00Z",
                "window_end": "2026-07-07T01:00:00Z",
            })
        };

        // Positive anchor: the very same body, minus `origin`, is accepted.
        serde_json::from_value::<CreateUsageRecordsRequest>(serde_json::json!({
            "records": [record()],
        }))
        .expect("the backfill body without `origin` MUST deserialize");

        let mut with_origin = record();
        with_origin
            .as_object_mut()
            .expect("record is a JSON object")
            .insert("origin".to_owned(), serde_json::json!("live"));
        let err = serde_json::from_value::<CreateUsageRecordsRequest>(serde_json::json!({
            "records": [with_origin],
        }))
        .expect_err("a caller-supplied `origin` MUST be refused");
        assert!(
            err.to_string().contains("origin"),
            "the refusal MUST name `origin` so the caller knows which member \
             to drop (got `{err}`)",
        );
    }
}
