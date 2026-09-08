//! Wiring test for [`super::register_routes`].
//!
//! [`super::register_api_routes`] is pinned by
//! [`super::openapi_contract_tests`], which deliberately supplies no
//! `Service`. What `register_routes` adds on top is the single
//! `Extension<Arc<Service>>` layer every handler extracts — and no
//! registry-shaped assertion can observe it, because the layer changes
//! nothing about the registered `OperationSpec`s.
//!
//! So this suite dispatches one request through both compositions and
//! pins the difference. With the layer, the handler body runs and answers
//! the plugin's not-found as 404; without it, axum's `Extension`
//! extractor rejects with 500 before any handler body executes. Deleting
//! `.layer(axum::Extension(service))` collapses the first case onto the
//! second and fails loudly.
//!
//! The `Extension<SecurityContext>` the handlers extract *first* is
//! gateway-supplied in production, so both routers under test get it from
//! the test. The status difference therefore turns on the service
//! extension alone.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use toolkit::api::OpenApiRegistryImpl;
use tower::ServiceExt as _;
use usage_collector_sdk::UsageCollectorPluginV1;
use uuid::Uuid;

use crate::domain::Service;
use crate::domain::test_support::{HappyPathPlugin, ServiceFixture, authenticated_ctx};

/// A sample `UsageRecord.id` that the plugin reports as absent, so a handler
/// that actually runs answers 404 — a status the missing-extension path
/// cannot produce.
fn sample_record_id() -> Uuid {
    Uuid::from_u128(0x1234_5678)
}

/// A `Service` whose storage plugin reports the sample record as absent.
fn service_reporting_not_found() -> Arc<Service> {
    let plugin = HappyPathPlugin::new();
    plugin.set_get_usage_record_not_found(sample_record_id());
    ServiceFixture::default().build(
        plugin as Arc<dyn UsageCollectorPluginV1>,
        "test.routes.service_layer.happy.v1",
    )
}

/// `GET /usage-collector/v1/records/{id}` through `router`.
async fn get_sample_record(router: Router) -> StatusCode {
    let request = Request::get(format!(
        "/usage-collector/v1/records/{}",
        sample_record_id()
    ))
    .body(Body::empty())
    .expect("request builds");

    router
        .layer(axum::Extension(authenticated_ctx()))
        .oneshot(request)
        .await
        .expect("Router's error type is Infallible")
        .status()
}

#[tokio::test]
async fn register_routes_attaches_the_service_extension() {
    let status = get_sample_record(super::register_routes(
        Router::new(),
        &OpenApiRegistryImpl::new(),
        service_reporting_not_found(),
    ))
    .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "with Extension<Arc<Service>> layered on, the handler must run and \
         surface the plugin's not-found as 404",
    );
}

#[tokio::test]
async fn the_same_routes_without_the_service_extension_cannot_serve_a_request() {
    // Counterpart to the test above, and the reason the contract suite can
    // assert nothing about the layer: same route set, same request, no
    // service.
    let status = get_sample_record(super::register_api_routes(
        Router::new(),
        &OpenApiRegistryImpl::new(),
    ))
    .await;

    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "without the service layer, axum's Extension extractor must reject \
         the request before the handler body runs",
    );
}

// ---------------------------------------------------------------------------
// 2.2 constraint-no-type-catalog: this gear exposes no usage-type endpoint
// at all, read or write. Declarations are a types-registry surface.
//
// An allow-list rather than a `contains("usage-types")` denylist: today
// `register_api_routes` wires exactly one module
// (`usage_records::register_usage_record_routes`), so asserting the full
// expected `(method, path)` set costs nothing extra and is strictly
// stronger — it also catches a catalog resurrected under any other name,
// not only literally spelled "usage-types".
// ---------------------------------------------------------------------------

#[test]
fn exactly_the_usage_record_routes_are_registered() {
    let reg = OpenApiRegistryImpl::new();
    let _router = super::register_api_routes(Router::new(), &reg);

    let mut registered: Vec<(String, String)> = reg
        .operation_specs
        .iter()
        .map(|entry| (entry.value().method.to_string(), entry.value().path.clone()))
        .collect();
    registered.sort();

    let mut expected = vec![
        ("POST".to_owned(), "/usage-collector/v1/records".to_owned()),
        ("GET".to_owned(), "/usage-collector/v1/records".to_owned()),
        (
            "POST".to_owned(),
            usage_collector_sdk::BACKFILL_ROUTE_PATH.to_owned(),
        ),
        (
            "POST".to_owned(),
            "/usage-collector/v1/records/aggregate".to_owned(),
        ),
        (
            "GET".to_owned(),
            "/usage-collector/v1/records/{id}".to_owned(),
        ),
    ];
    expected.sort();

    assert_eq!(
        registered, expected,
        "register_api_routes must wire exactly the usage-record surface; no \
         usage-type route (or any other route) may be present under any name"
    );
}

// ---------------------------------------------------------------------------
// Each ingestion route reaches its OWN handler.
//
// `openapi_contract_tests` and `usage_records_tests` between them pin the
// path, method, operationId, tag, request schema and response schemas of
// both ingestion routes — and none of that observes `.handler(…)`. The
// handler tests, in turn, call each handler function directly and never
// touch the router. So repointing `POST /records/backfill` at
// `handle_create_usage_records` left every one of those green while the
// route stamped `origin: live` and enforced the live past bound on every
// request — defeating the only thing the route exists for.
//
// This is the seam that closes it: dispatch through the real router and
// read the `RecordOrigin` off the record the storage plugin was handed.
// The crossover is asserted in BOTH directions, because the reverse
// mis-wiring is not the harmless one it looks like — the live route
// pointed at the backfill handler would silently lift the live past
// tolerance for every emitter on the platform.
// ---------------------------------------------------------------------------

/// One valid submission against the happy-path meter, in the shape the
/// gateway derives from the wire body below.
fn backfill_probe_submission() -> usage_collector_sdk::CreateUsageRecord {
    usage_collector_sdk::CreateUsageRecord {
        gts_type_id: usage_collector_sdk::MeterTypeId::new(PROBE_GTS_ID)
            .expect("valid gts_type_id"),
        tenant_id: Uuid::from_u128(0x9911),
        resource_ref: usage_collector_sdk::ResourceRef::new("rsc-route-probe", "compute.vm")
            .expect("valid resource ref"),
        subject_ref: None,
        metadata: std::collections::BTreeMap::new(),
        value: rust_decimal::Decimal::from(1),
        idempotency_key: usage_collector_sdk::IdempotencyKey::new("idem-route-probe")
            .expect("valid idempotency key"),
        invalidation: None,
        window_start: crate::domain::test_support::recent_window_start(),
        window_end: crate::domain::test_support::recent_window_end(),
    }
}

const PROBE_GTS_ID: &str =
    toolkit_gts::gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

/// The wire body carrying [`backfill_probe_submission`].
///
/// Written as JSON rather than serialized from the request DTO, which
/// derives `Deserialize` only — and which is the right shape for this
/// test anyway, since a client posts bytes. A body that drifted from the
/// DTO could not pass silently: `deny_unknown_fields` and the mandatory
/// members make a mismatch a `4xx`, and both assertions below demand
/// `200`.
fn probe_request_body() -> Body {
    let submission = backfill_probe_submission();
    let rfc3339 = |t: time::OffsetDateTime| {
        t.format(&time::format_description::well_known::Rfc3339)
            .expect("fixture timestamp formats as RFC 3339")
    };
    Body::from(
        serde_json::json!({
            "records": [{
                "gts_type_id": PROBE_GTS_ID,
                "tenant_id": submission.tenant_id.to_string(),
                "resource_ref": {
                    "resource_id": "rsc-route-probe",
                    "resource_type": "compute.vm",
                },
                "value": "1",
                "idempotency_key": "idem-route-probe",
                "window_start": rfc3339(submission.window_start),
                "window_end": rfc3339(submission.window_end),
            }],
        })
        .to_string(),
    )
}

/// POST the probe body to `path` through the fully wired router, and
/// return the `RecordOrigin` the gateway stamped on the record it handed
/// the storage plugin.
///
/// The origin is read off what the plugin was HANDED, never off what the
/// fixture hands back: the echo is programmed with the origin under test,
/// so an assertion on the response body alone would be an assertion about
/// the fixture.
async fn origin_stamped_by_route(
    path: &str,
    echo_origin: usage_collector_sdk::RecordOrigin,
) -> (StatusCode, usage_collector_sdk::RecordOrigin) {
    let plugin = HappyPathPlugin::new();
    plugin.set_create_records(vec![Ok(
        crate::domain::test_support::projected_with_origin(
            &backfill_probe_submission(),
            echo_origin,
        ),
    )]);
    let service = ServiceFixture::default()
        .with_source(crate::domain::test_support::fake_declaration_source_with_fold("SUM"))
        .build(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.routes.handler_binding.happy.v1",
        );

    let request = Request::post(path)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(probe_request_body())
        .expect("request builds");

    let status = super::register_routes(Router::new(), &OpenApiRegistryImpl::new(), service)
        .layer(axum::Extension(authenticated_ctx()))
        .oneshot(request)
        .await
        .expect("Router's error type is Infallible")
        .status();

    let forwarded = plugin
        .last_create_records_input()
        .unwrap_or_else(|| panic!("`{path}` MUST reach the storage plugin"));
    assert_eq!(forwarded.len(), 1, "`{path}` MUST dispatch the one record");
    (status, forwarded[0].origin)
}

#[tokio::test]
async fn each_ingestion_route_dispatches_to_its_own_handler() {
    let (live_status, live_origin) = origin_stamped_by_route(
        "/usage-collector/v1/records",
        usage_collector_sdk::RecordOrigin::Live,
    )
    .await;
    assert_eq!(live_status, StatusCode::OK);
    assert_eq!(
        live_origin,
        usage_collector_sdk::RecordOrigin::Live,
        "`POST /usage-collector/v1/records` MUST be bound to \
         `handle_create_usage_records`; bound to the backfill handler it \
         would lift the live past tolerance for every emitter",
    );

    let (backfill_status, backfill_origin) = origin_stamped_by_route(
        usage_collector_sdk::BACKFILL_ROUTE_PATH,
        usage_collector_sdk::RecordOrigin::Backfill,
    )
    .await;
    assert_eq!(backfill_status, StatusCode::OK);
    assert_eq!(
        backfill_origin,
        usage_collector_sdk::RecordOrigin::Backfill,
        "`POST {}` MUST be bound to `handle_backfill_usage_records`; bound to \
         the live handler it would stamp `origin: live` and enforce the live \
         past bound, which is the whole reason this route exists",
        usage_collector_sdk::BACKFILL_ROUTE_PATH,
    );
}
