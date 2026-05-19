//! Handler-level unit tests for the tenant-metadata REST surface.
//!
//! Scope: pin the seams the [`super::list_metadata`] /
//! [`super::get_metadata`] / [`super::upsert_metadata`] /
//! [`super::delete_metadata`] / [`super::resolve_metadata`] handler
//! functions add on top of the [`MetadataService`] contract. Tests
//! call the handler functions directly with synthesized axum
//! extractors (`Extension(...)`, `Path(...)`, `Json(...)`,
//! `OData(...)`) against a service wired from in-crate fakes
//! ([`FakeMetadataRepo`], [`FakeTenantRepo`]) plus
//! [`StubMetadataSchemaRegistry`] + [`mock_enforcer`].
//!
//! Out of scope here:
//!
//! * Wire-shape / DTO conversions — pinned in
//!   [`crate::api::rest::dto_tests`] (the `from_entry`,
//!   `from_resolution`, transparent-`PutTenantMetadataDto` round
//!   trips).
//! * Service-layer guard ordering, walk-up algorithm, PUT idempotency
//!   — pinned in [`crate::domain::metadata::service_tests`].
//!
//! Each test would fail if the corresponding handler line were
//! broken: the path-tenant-id wrap site, the
//! `UpsertMetadataRequest::new(schema_id, body.value)` assembly, the
//! `no_content()` 204 branch, the `ResolvedTenantMetadataDto`
//! schema-id echo from the typed projection, and the empty-resolution
//! → `resolved=false` branch.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test-support fakes panic on poisoned mutex; the canonical expect form is shared with FakeMetadataRepo's tests"
)]
#![allow(
    clippy::too_many_lines,
    reason = "handler-level fixtures intentionally seed multi-row scenarios inline so each test reads as a self-contained scenario"
)]

use std::sync::Arc;

use axum::Extension;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use modkit::api::odata::OData;
use modkit_odata::ODataQuery;
use modkit_security::SecurityContext;
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use gts::GtsSchemaId;

use crate::api::rest::dto::PutTenantMetadataDto;
use crate::api::rest::handlers::metadata::{
    delete_metadata, get_metadata, list_metadata, resolve_metadata, upsert_metadata,
};
use crate::domain::metadata::registry::{InheritancePolicy, StubMetadataSchemaRegistry};
use crate::domain::metadata::repo::MetadataRepo;
use crate::domain::metadata::service::MetadataService;
use crate::domain::metadata::test_support::FakeMetadataRepo;
use crate::domain::tenant::model::{TenantModel, TenantStatus};
use crate::domain::tenant::test_support::{FakeTenantRepo, mock_enforcer};

// ---- helpers ----------------------------------------------------

fn fixed_now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("epoch")
}

/// Subject tenant id used by every handler-test `ctx()`. The
/// `MockAuthZResolver` returns an `InTenantSubtree` predicate rooted
/// at this id; [`seed_tenant`] materialises matching closure rows so
/// the PEP-derived scope resolves to a non-empty visible set on the
/// `FakeTenantRepo`. Mirrors the shape used by
/// `domain::metadata::service_tests`.
const fn ctx_subject_root() -> Uuid {
    Uuid::from_u128(0xCAFE_BABE)
}

fn ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(0xCAFE))
        .subject_tenant_id(ctx_subject_root())
        .build()
        .expect("ctx")
}

fn schema_a() -> GtsSchemaId {
    GtsSchemaId::new("gts.cf.core.am.tenant_metadata.v1~vendor.app.metadata.theme.v1~")
}

fn schema_b() -> GtsSchemaId {
    GtsSchemaId::new("gts.cf.core.am.tenant_metadata.v1~vendor.app.metadata.billing.v1~")
}

fn schema_a_raw() -> String {
    "gts.cf.core.am.tenant_metadata.v1~vendor.app.metadata.theme.v1~".to_owned()
}

/// Seed a single tenant + its `(subject_root, id, 0)` /
/// `(id, id, 0)` closure rows so the PEP scope sees it. Mirrors
/// `domain::metadata::service_tests::seed_tenant`.
fn seed_tenant(fake: &FakeTenantRepo, id: Uuid, parent_id: Option<Uuid>, status: TenantStatus) {
    let now = fixed_now();
    let depth = u32::from(parent_id.is_some());
    fake.insert_tenant_raw(TenantModel {
        id,
        parent_id,
        name: format!("t-{id}"),
        status,
        self_managed: false,
        tenant_type_uuid: Uuid::from_u128(0xAA),
        depth,
        created_at: now,
        updated_at: now,
        deleted_at: None,
    });
    let subject_root = ctx_subject_root();
    fake.seed_closure(subject_root, id, 0, status);
    if subject_root != id {
        fake.seed_closure(id, id, 0, status);
    }
}

/// Build a `MetadataService` wired from the in-crate fakes + the
/// permissive [`mock_enforcer`] + a deterministic clock. Mirrors
/// `service_tests::make_service`.
fn build_service(
    md_repo: Arc<FakeMetadataRepo>,
    tenants: Arc<FakeTenantRepo>,
    registry: Arc<StubMetadataSchemaRegistry>,
) -> Arc<MetadataService> {
    let now = fixed_now();
    let now_fn = Arc::new(move || now);
    Arc::new(MetadataService::new(md_repo, tenants, registry, mock_enforcer()).with_now_fn(now_fn))
}

async fn seed_metadata_row(
    fake: &FakeMetadataRepo,
    tenant_id: Uuid,
    schema_id: &GtsSchemaId,
    value: serde_json::Value,
) {
    let schema_uuid = gts::GtsID::new(schema_id.as_ref())
        .expect("valid GTS id in tests")
        .to_uuid();
    let scope = modkit_security::AccessScope::allow_all();
    fake.upsert_for_tenant(&scope, tenant_id, schema_uuid, value, fixed_now(), None)
        .await
        .expect("seed upsert");
}

// ---- list_metadata ---------------------------------------------

#[tokio::test]
async fn list_metadata_wraps_entries_with_path_tenant_id() {
    // The handler maps the service `Page<MetadataEntry>` items via
    // `TenantMetadataEntryDto::from_entry(tenant_id, entry)` using the
    // **path** tenant_id (`MetadataEntry` itself carries no
    // `tenant_id` field). A regression that wired the wrap to e.g.
    // `Uuid::nil()` or a sibling id would fail this assertion. The
    // DTO wire-shape pin lives in `dto_tests::entry_dto_round_trip_…`;
    // here we pin the handler-side path-id echo specifically.
    let md_repo = Arc::new(FakeMetadataRepo::new());
    let tenants = Arc::new(FakeTenantRepo::new());
    let registry = Arc::new(StubMetadataSchemaRegistry::with_seed(vec![
        (schema_a(), InheritancePolicy::OverrideOnly),
        (schema_b(), InheritancePolicy::OverrideOnly),
    ]));
    let tid = Uuid::from_u128(0x100);
    seed_tenant(&tenants, tid, None, TenantStatus::Active);
    seed_metadata_row(&md_repo, tid, &schema_a(), json!({"theme": "dark"})).await;
    seed_metadata_row(&md_repo, tid, &schema_b(), json!({"plan": "pro"})).await;

    let svc = build_service(md_repo, tenants, registry);

    let response = list_metadata(
        Extension(ctx()),
        Extension(svc),
        Path(tid),
        OData(ODataQuery::default()),
    )
    .await
    .expect("list happy path");

    let axum::Json(page) = response;
    assert_eq!(page.items.len(), 2, "two seeded rows surface on the page");
    for item in &page.items {
        assert_eq!(
            item.tenant_id, tid,
            "every DTO MUST echo the path-supplied tenant_id (not e.g. nil or a per-row intrinsic)",
        );
    }
}

// ---- get_metadata ----------------------------------------------

#[tokio::test]
async fn get_metadata_parses_schema_id_from_path_segment() {
    // The handler builds `GtsSchemaId::new(&raw)` from the raw path
    // segment and forwards the typed id to the service. Pin that a
    // valid chained id round-trips through the handler into the
    // wire DTO unchanged. A regression that bypassed the typed
    // construction (e.g. forwarded the raw `String`) would surface
    // here as a missing `schema_id` field on the response shape.
    let md_repo = Arc::new(FakeMetadataRepo::new());
    let tenants = Arc::new(FakeTenantRepo::new());
    let registry = Arc::new(StubMetadataSchemaRegistry::with_seed(vec![(
        schema_a(),
        InheritancePolicy::OverrideOnly,
    )]));
    let tid = Uuid::from_u128(0x100);
    seed_tenant(&tenants, tid, None, TenantStatus::Active);
    seed_metadata_row(&md_repo, tid, &schema_a(), json!({"theme": "dark"})).await;

    let svc = build_service(md_repo, tenants, registry);

    let axum::Json(dto) = get_metadata(
        Extension(ctx()),
        Extension(svc),
        Path((tid, schema_a_raw())),
    )
    .await
    .expect("get happy path");

    assert_eq!(
        dto.schema_id,
        schema_a_raw(),
        "typed GtsSchemaId round-trips back to the same chained string on the wire",
    );
    assert_eq!(dto.tenant_id, tid, "path tenant_id echoed on the DTO");
    assert_eq!(dto.value, json!({"theme": "dark"}));
}

// ---- upsert_metadata ----------------------------------------------

#[tokio::test]
async fn put_metadata_assembles_request_from_path_and_body() {
    // The handler builds `UpsertMetadataRequest::new(schema_id,
    // body.value)` from the path-supplied `schema_id` and the
    // transparent JSON body. Pin that the assembled request reaches
    // the service: the post-write entry carries the body's value.
    // A regression that swapped the two arguments — or constructed
    // a different request shape — would surface here as either a
    // mismatched value or a 400 from the service.
    let md_repo = Arc::new(FakeMetadataRepo::new());
    let tenants = Arc::new(FakeTenantRepo::new());
    let registry = Arc::new(StubMetadataSchemaRegistry::with_seed(vec![(
        schema_a(),
        InheritancePolicy::OverrideOnly,
    )]));
    let tid = Uuid::from_u128(0x100);
    seed_tenant(&tenants, tid, None, TenantStatus::Active);

    let svc = build_service(md_repo, tenants, registry);

    let body = PutTenantMetadataDto {
        value: json!({"theme": "light", "contrast": 12}),
    };
    let axum::Json(dto) = upsert_metadata(
        Extension(ctx()),
        Extension(svc),
        Path((tid, schema_a_raw())),
        axum::Json(body),
    )
    .await
    .expect("put happy path");

    assert_eq!(
        dto.value,
        json!({"theme": "light", "contrast": 12}),
        "post-write entry MUST carry the body value unchanged",
    );
    assert_eq!(
        dto.schema_id,
        schema_a_raw(),
        "path-supplied schema_id echoed on the response DTO",
    );
    assert_eq!(dto.tenant_id, tid);
}

// ---- delete_metadata --------------------------------------------

#[tokio::test]
async fn delete_metadata_returns_204_no_content_on_success() {
    // The handler returns `no_content().into_response()` on the
    // `Ok(())` arm. Pin the precise status — a regression that
    // wrapped the empty response into e.g. `Json(())` or returned 200
    // would change the status code under the typed `IntoResponse`
    // pipeline.
    let md_repo = Arc::new(FakeMetadataRepo::new());
    let tenants = Arc::new(FakeTenantRepo::new());
    let registry = Arc::new(StubMetadataSchemaRegistry::with_seed(vec![(
        schema_a(),
        InheritancePolicy::OverrideOnly,
    )]));
    let tid = Uuid::from_u128(0x100);
    seed_tenant(&tenants, tid, None, TenantStatus::Active);
    seed_metadata_row(&md_repo, tid, &schema_a(), json!({"theme": "dark"})).await;

    let svc = build_service(md_repo, tenants, registry);

    let response = delete_metadata(
        Extension(ctx()),
        Extension(svc),
        Path((tid, schema_a_raw())),
    )
    .await
    .expect("delete happy path")
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::NO_CONTENT,
        "successful delete MUST be 204 No Content (RFC 7231)",
    );
}

// ---- resolve_metadata -------------------------------------------

#[tokio::test]
async fn resolve_metadata_echoes_typed_projection_schema_id() {
    // The handler comment is explicit: "Echo from the typed projection
    // rather than re-using the raw path string". `GtsSchemaId::new`
    // performs no normalisation (stores the input verbatim), so the
    // observable contract is that the echoed `schema_id` on the DTO
    // matches `GtsSchemaId::new(raw).as_ref()` — i.e. the same string
    // that flowed through the typed wrapper. A regression that echoed
    // the raw path binding directly would still pass today; this test
    // pins the data-flow `path String → GtsSchemaId → as_ref() →
    // echoed String` so any future change to `GtsSchemaId::new`'s
    // normalisation (or to the handler's echo source) stays
    // wire-visible.
    let md_repo = Arc::new(FakeMetadataRepo::new());
    let tenants = Arc::new(FakeTenantRepo::new());
    let registry = Arc::new(StubMetadataSchemaRegistry::with_seed(vec![(
        schema_a(),
        InheritancePolicy::OverrideOnly,
    )]));
    let tid = Uuid::from_u128(0x100);
    seed_tenant(&tenants, tid, None, TenantStatus::Active);
    seed_metadata_row(&md_repo, tid, &schema_a(), json!({"theme": "own"})).await;

    let svc = build_service(md_repo, tenants, registry);

    let raw = schema_a_raw();
    let axum::Json(dto) =
        resolve_metadata(Extension(ctx()), Extension(svc), Path((tid, raw.clone())))
            .await
            .expect("resolve happy path");

    let typed_echo = GtsSchemaId::new(&raw).as_ref().to_owned();
    assert_eq!(
        dto.schema_id, typed_echo,
        "handler MUST echo the typed projection's `as_ref()` (data flow: path String -> \
         GtsSchemaId -> as_ref() -> echoed String)",
    );
    assert_eq!(dto.tenant_id, tid);
    assert!(dto.resolved, "own row resolved");
    assert_eq!(dto.value, Some(json!({"theme": "own"})));
}

#[tokio::test]
async fn resolve_metadata_resolved_false_when_walk_yields_nothing() {
    // The handler wraps the service's `Option<MetadataEntry>` via
    // `ResolvedTenantMetadataDto::from_resolution(...)`. The empty-walk
    // branch surfaces `resolved=false` with NO `value` key (per FEATURE
    // §3: "empty resolution is HTTP 200 with an empty response").
    // Pin both: the boolean and the schema_id echo on the empty arm.
    // DTO-level pin for the same conversion lives in
    // `dto_tests::resolved_dto_none_omits_value_…`; this test adds the
    // handler-level path-tenant-id echo + the typed-id projection.
    let md_repo = Arc::new(FakeMetadataRepo::new());
    let tenants = Arc::new(FakeTenantRepo::new());
    let registry = Arc::new(StubMetadataSchemaRegistry::with_seed(vec![(
        schema_a(),
        InheritancePolicy::OverrideOnly,
    )]));
    let tid = Uuid::from_u128(0x100);
    seed_tenant(&tenants, tid, None, TenantStatus::Active);
    // No metadata rows seeded — override_only walk on a row-free
    // tenant returns `None` from the service.

    let svc = build_service(md_repo, tenants, registry);

    let raw = schema_a_raw();
    let axum::Json(dto) =
        resolve_metadata(Extension(ctx()), Extension(svc), Path((tid, raw.clone())))
            .await
            .expect("resolve empty-walk happy path");

    assert!(
        !dto.resolved,
        "empty walk-up MUST surface resolved=false (NOT 404 per FEATURE section 3)",
    );
    assert!(
        dto.value.is_none(),
        "empty walk-up MUST NOT carry a `value` (skip_serializing_if drops the key on wire)",
    );
    assert_eq!(
        dto.schema_id,
        GtsSchemaId::new(&raw).as_ref(),
        "schema_id echo MUST fire on the empty-walk arm too",
    );
    assert_eq!(dto.tenant_id, tid);
}
