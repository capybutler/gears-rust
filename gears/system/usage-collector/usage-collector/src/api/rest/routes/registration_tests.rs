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
