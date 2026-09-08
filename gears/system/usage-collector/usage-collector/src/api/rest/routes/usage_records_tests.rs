//! Unit tests for the foundation usage-record REST route table.
//!
//! Exercises the record-surface routes (`POST /usage-collector/v1/records`
//! and `GET /usage-collector/v1/records/{id}`) against the
//! [`toolkit::api::openapi_registry::OpenApiRegistryImpl`] that
//! [`super::register_usage_record_routes`] populates. Each test pulls the
//! full registered [`toolkit::api::operation_builder::OperationSpec`] and
//! asserts the contract surface that documents the route — operation id,
//! authentication posture, license posture, request body schema, success
//! response schemas, and standard error coverage — so a regression that
//! silently drops `.authenticated()`, `.no_license_required()`,
//! `.json_request::<…>`, `.json_response_with_schema::<…>`, or
//! `.standard_errors()` fails loudly.

use axum::Router;
use axum::http::{Method, StatusCode};
use toolkit::api::openapi_registry::OpenApiRegistryImpl;
use toolkit::api::operation_builder::{OperationSpec, ParamLocation, RequestBodySchema};

use super::register_usage_record_routes;
use crate::api::rest::dto;

fn registry_and_router() -> (OpenApiRegistryImpl, Router) {
    let registry = OpenApiRegistryImpl::new();
    let router = register_usage_record_routes(Router::new(), &registry);
    (registry, router)
}

fn lookup_spec(registry: &OpenApiRegistryImpl, method: &Method, path: &str) -> OperationSpec {
    let key = format!("{}:{}", method.as_str(), path);
    registry
        .operation_specs
        .get(&key)
        .unwrap_or_else(|| panic!("expected route to be registered: {key}"))
        .value()
        .clone()
}

/// Schema component name `OperationBuilder` produces for a DTO is its
/// `utoipa::ToSchema::name()`.
fn schema_name<T: utoipa::ToSchema>() -> String {
    <T as utoipa::ToSchema>::name().to_string()
}

/// Standard `OperationBuilder::standard_errors` set, kept in sync with
/// `libs/toolkit/src/api/operation_builder.rs::standard_errors`. A route
/// that drops `.standard_errors()` will fail this list.
const STANDARD_ERROR_STATUSES: &[u16] = &[400, 401, 403, 404, 409, 429, 500];

fn assert_standard_errors_registered(spec: &OperationSpec) {
    for status in STANDARD_ERROR_STATUSES {
        let found = spec.responses.iter().any(|r| {
            r.status == *status
                && r.content_type == "application/problem+json"
                && r.schema_name().is_some()
        });
        assert!(
            found,
            "operation `{}` MUST declare standard error response {status} \
             (application/problem+json with a registered schema)",
            spec.path,
        );
    }
}

fn assert_authenticated_and_no_license(spec: &OperationSpec) {
    assert!(
        spec.authenticated,
        "operation `{}:{}` MUST be `.authenticated()` — \
         the foundation surface refuses anonymous callers",
        spec.method, spec.path,
    );
    assert!(
        spec.license_requirement.is_none(),
        "operation `{}:{}` MUST be `.no_license_required()` — \
         the foundation surface is platform-internal substrate",
        spec.method,
        spec.path,
    );
}

#[tokio::test]
async fn create_usage_records_route_is_registered_with_documented_contract() {
    let (registry, _router) = registry_and_router();
    let spec = lookup_spec(&registry, &Method::POST, "/usage-collector/v1/records");

    assert_eq!(
        spec.operation_id.as_deref(),
        Some("usage_collector.create_usage_records"),
    );
    assert_authenticated_and_no_license(&spec);

    // `.json_request::<CreateUsageRecordsRequest>` — request body declared
    // as a registered schema reference, not inline / multipart.
    let body = spec
        .request_body
        .as_ref()
        .expect("create-records route MUST declare a JSON request body");
    assert_eq!(body.content_type, "application/json");
    assert!(body.required, "create-records body MUST be required");
    let expected_request = schema_name::<dto::CreateUsageRecordsRequest>();
    match &body.schema {
        RequestBodySchema::Ref { schema_name } => assert_eq!(schema_name, &expected_request),
        other => panic!("expected schema ref to `{expected_request}`, got {other:?}"),
    }

    // Two success responses (200 OK and 207 Multi-Status), both bound to
    // the `CreateUsageRecordsResponse` schema.
    let expected_response = schema_name::<dto::CreateUsageRecordsResponse>();
    for status in [StatusCode::OK, StatusCode::MULTI_STATUS] {
        let response = spec
            .responses
            .iter()
            .find(|r| r.status == status.as_u16())
            .unwrap_or_else(|| panic!("create-records route MUST declare a {status} response"));
        assert_eq!(response.content_type, "application/json");
        assert_eq!(
            response.schema_name(),
            Some(expected_response.as_str()),
            "create-records {status} MUST point at `CreateUsageRecordsResponse`",
        );
    }

    assert_standard_errors_registered(&spec);
}

#[tokio::test]
async fn get_usage_record_route_is_registered_with_documented_contract() {
    let (registry, _router) = registry_and_router();
    let spec = lookup_spec(&registry, &Method::GET, "/usage-collector/v1/records/{id}");

    assert_eq!(
        spec.operation_id.as_deref(),
        Some("usage_collector.get_usage_record"),
    );
    assert_authenticated_and_no_license(&spec);

    // Path param `id`. Get carries no JSON request body.
    let id_param = spec
        .params
        .iter()
        .find(|p| p.name == "id" && p.location == ParamLocation::Path)
        .expect("get route MUST declare path param `id`");
    assert!(id_param.required, "path param `id` MUST be required");
    assert!(
        spec.request_body.is_none(),
        "get route MUST NOT declare a request body",
    );

    // Single success response — 200 OK with the `UsageRecordDto` schema.
    let expected_response = schema_name::<dto::UsageRecordDto>();
    let ok_resp = spec
        .responses
        .iter()
        .find(|r| r.status == StatusCode::OK.as_u16())
        .expect("get route MUST declare a 200 OK response");
    assert_eq!(ok_resp.content_type, "application/json");
    assert_eq!(
        ok_resp.schema_name(),
        Some(expected_response.as_str()),
        "get 200 MUST point at `UsageRecordDto`",
    );

    assert_standard_errors_registered(&spec);
}

/// The backfill route is registered, and registered against
/// `usage_collector_sdk::BACKFILL_ROUTE_PATH` rather than a literal: the
/// SDK's past-tolerance rejection tells a caller to resubmit at that
/// constant, so a route registered anywhere else would leave that
/// rejection pointing at a 404.
#[tokio::test]
async fn backfill_usage_records_route_is_registered_with_documented_contract() {
    let (registry, _router) = registry_and_router();
    let spec = lookup_spec(
        &registry,
        &Method::POST,
        usage_collector_sdk::BACKFILL_ROUTE_PATH,
    );

    assert_eq!(
        spec.operation_id.as_deref(),
        Some("usage_collector.backfill_usage_records"),
    );
    assert!(
        spec.tags.iter().any(|tag| tag == "Backfill"),
        "backfill route MUST carry the `Backfill` tag, not the record-surface \
         tag; it is a separate operator-facing surface (tags: {:?})",
        spec.tags,
    );
    assert_authenticated_and_no_license(&spec);

    // Same request schema as `POST /records`: the whole point is that the
    // two routes take one body shape, so a caller moving a rejected
    // submission from one to the other changes only the URL.
    let body = spec
        .request_body
        .as_ref()
        .expect("backfill route MUST declare a JSON request body");
    assert_eq!(body.content_type, "application/json");
    assert!(body.required, "backfill body MUST be required");
    let expected_request = schema_name::<dto::CreateUsageRecordsRequest>();
    match &body.schema {
        RequestBodySchema::Ref { schema_name } => assert_eq!(schema_name, &expected_request),
        other => panic!("expected schema ref to `{expected_request}`, got {other:?}"),
    }

    // Both success statuses, both bound to `CreateUsageRecordsResponse` —
    // the same per-entry envelope `POST /records` publishes.
    let expected_response = schema_name::<dto::CreateUsageRecordsResponse>();
    for status in [StatusCode::OK, StatusCode::MULTI_STATUS] {
        let response = spec
            .responses
            .iter()
            .find(|r| r.status == status.as_u16())
            .unwrap_or_else(|| panic!("backfill route MUST declare a {status} response"));
        assert_eq!(response.content_type, "application/json");
        assert_eq!(
            response.schema_name(),
            Some(expected_response.as_str()),
            "backfill {status} MUST point at `CreateUsageRecordsResponse`",
        );
    }

    assert_standard_errors_registered(&spec);
}

/// `usage-collector-v1.yaml` enumerates four ways the backfill route
/// differs from `POST /records`, the first being that its workload is
/// isolated from live ingestion "so it cannot breach live-path SLOs".
/// That isolation is NOT implemented — the route is
/// `Service::create_usage_records_for_origin` under a different origin,
/// sharing the live path's runtime, pool and fan-out budget. So the
/// registered text names three differences, and neither summary nor
/// description claims an isolation the gear does not provide.
///
/// The positive half of this test is load-bearing: `!contains("isolat")`
/// alone is satisfied by an empty description, which would be a worse
/// contract than the false one.
#[tokio::test]
async fn backfill_route_publishes_three_differences_and_claims_no_workload_isolation() {
    let (registry, _router) = registry_and_router();
    let spec = lookup_spec(
        &registry,
        &Method::POST,
        usage_collector_sdk::BACKFILL_ROUTE_PATH,
    );

    let summary = spec
        .summary
        .as_deref()
        .expect("backfill route MUST carry a summary");
    let description = spec
        .description
        .as_deref()
        .expect("backfill route MUST carry a description");

    // Positive anchors: all three implemented differences are named.
    assert!(
        description.contains("three respects"),
        "description MUST enumerate three differences (got `{description}`)",
    );
    for claim in [
        "origin: backfill",
        "past bound",
        "elevated authorization",
        "invalidation entries",
    ] {
        assert!(
            description.contains(claim),
            "description MUST name `{claim}` (got `{description}`)",
        );
    }
    assert!(
        summary.contains("Bulk historical import"),
        "summary MUST say what the route is for (got `{summary}`)",
    );

    // Negative half: no isolation claim, in either field. `isolat` catches
    // "isolated" / "isolation" alike.
    for (field, text) in [("summary", summary), ("description", description)] {
        assert!(
            !text.to_ascii_lowercase().contains("isolat"),
            "backfill {field} MUST NOT claim workload isolation while \
             `Service::backfill_usage_records` still carries the TODO saying \
             it is unimplemented (got `{text}`)",
        );
    }
    assert!(
        !description.contains("four respects"),
        "description MUST NOT reproduce the contract's four-way enumeration \
         (got `{description}`)",
    );
}
