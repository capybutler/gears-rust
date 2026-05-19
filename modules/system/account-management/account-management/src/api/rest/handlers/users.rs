//! REST handlers for tenant-scoped user ops. `IdP` is the authoritative
//! source of truth — AM holds no local user table. PEP gate runs inside
//! `UserService`; handlers forward `SecurityContext` + body. Pagination
//! construction is the one boundary-validation that runs at the handler
//! layer (mapped to `DomainError::Validation` → HTTP 400).
//! `DomainError → CanonicalError` via the `From` impl in
//! `crate::infra::sdk_error_mapping`.

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, Query};
use axum::response::IntoResponse;
use serde::Deserialize;
use tracing::field::Empty;
use uuid::Uuid;

use account_management_sdk::IdpUserPagination;
use account_management_sdk::ListUsersQuery as SdkListUsersQuery;
use modkit::api::canonical_prelude::*;
use modkit_security::SecurityContext;

use crate::api::rest::dto::{UserCreateRequestDto, UserDto};
use crate::domain::error::DomainError;
use crate::domain::user::service::UserService;

/// `user_id` turns the listing into an authoritative existence check
/// (at most one row) per
/// `dod-idp-user-operations-contract-user-projection-schema`; combining
/// it with `limit` / `cursor` is rejected as a wire-shape error.
#[derive(Debug, Default, Deserialize)]
pub struct ListUsersQuery {
    #[serde(default)]
    pub user_id: Option<Uuid>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub cursor: Option<String>,
}

/// `GET /account-management/v1/tenants/{tenant_id}/users`
///
/// With `user_id`: empty page is the authoritative absent signal (no
/// 404) per FEATURE §5.5 `DoD`.
///
/// # Errors
///
/// Surfaces a canonical `Problem` envelope. Notable codes:
/// `validation` (400 — `limit` out of range on the unfiltered
/// listing path; tenant not in `Active` status; cursor too long;
/// `user_id` combined with `limit` or `cursor`, which is a
/// point-lookup existence check and cannot be paginated),
/// `cross_tenant_denied` (403), tenant `not_found` (404),
/// `idp_unavailable` (503), `idp_unsupported_operation` (501).
#[tracing::instrument(
    skip(svc, ctx, query),
    fields(tenant_id = %tenant_id, request_id = Empty)
)]
pub async fn list_users(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<UserService>>,
    Path(tenant_id): Path<Uuid>,
    Query(query): Query<ListUsersQuery>,
) -> ApiResult<Json<modkit_odata::Page<UserDto>>> {
    // Pagination bounds (`top` zero / oversized, cursor too long) and
    // the filtered-shape combination guard (`user_id` + `limit` /
    // `cursor`) are wire-shape rejections — surface as `Validation`
    // (400) BEFORE the service runs so the call never touches the
    // `IdP`.
    let pagination = pagination_for(&query, svc.max_listing_top())?;
    let mut list_query = SdkListUsersQuery::new(pagination);
    if let Some(user_id) = query.user_id {
        list_query = list_query.with_user_id_filter(user_id);
    }
    let page = svc.list_users(&ctx, tenant_id, list_query).await?;
    Ok(Json(page.map_items(UserDto::from_idp_user)))
}

/// `pagination_for`: build pagination for the wire query. `?user_id`
/// is an existence check — reject combinations with `limit` or
/// `cursor` (Validation 400) so spec-compliant clients see the
/// constraint instead of getting a silently overridden
/// `top=1`/`cursor=None` response.
fn pagination_for(query: &ListUsersQuery, max_top: u32) -> Result<IdpUserPagination, DomainError> {
    if query.user_id.is_some() {
        if query.limit.is_some() || query.cursor.is_some() {
            return Err(DomainError::Validation {
                detail: format!(
                    "list_users: `user_id` is a point-lookup existence check and cannot \
                     be combined with pagination knobs (got limit={:?}, cursor={:?}); \
                     omit both to receive at most one matching row",
                    query.limit, query.cursor,
                ),
            });
        }
        IdpUserPagination::new(1, None)
    } else {
        let requested = query.limit.unwrap_or(IdpUserPagination::DEFAULT_TOP);
        let clamped = requested.min(max_top);
        IdpUserPagination::new(clamped, query.cursor.clone())
    }
    .map_err(|err| DomainError::Validation {
        detail: format!("list_users: invalid pagination: {err}"),
    })
}

#[cfg(test)]
#[path = "users_tests.rs"]
mod tests;

/// `POST /account-management/v1/tenants/{tenant_id}/users`
///
/// Returns HTTP 201 with the `IdP`-projected user body.
///
/// # No `Location` header
///
/// AM intentionally does not surface a single-user GET — per
/// DECOMPOSITION §2.5 the user-ops API list is exactly
/// `{listUsers, createUser, deleteUser}`. The canonical read-back
/// shape is the filtered listing
/// `GET /tenants/{tenant_id}/users?user_id=<id>`. A `Location`
/// pointing at `/users/{user_id}` would imply a per-user resource
/// that does not exist on the AM REST surface and would yield a 405.
///
/// # Errors
///
/// Surfaces a canonical `Problem` envelope. Notable codes:
/// `validation` (400 — tenant not in `Active` status; empty /
/// whitespace-only username; oversized fields; GTS schema rejection;
/// provider-side validation), `cross_tenant_denied` (403), tenant
/// `not_found` (404), `idp_unavailable` (503),
/// `idp_unsupported_operation` (501).
#[tracing::instrument(
    skip(svc, ctx, body),
    fields(tenant_id = %tenant_id, request_id = Empty)
)]
pub async fn create_user(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<UserService>>,
    Path(tenant_id): Path<Uuid>,
    Json(body): Json<UserCreateRequestDto>,
) -> ApiResult<impl IntoResponse> {
    let payload = body.into_idp_new_user();
    let user = svc.create_user(&ctx, tenant_id, payload).await?;
    let dto = UserDto::from_idp_user(user);
    // 201 without a `Location` header: AM does not expose per-user
    // `GET /tenants/{tenant_id}/users/{user_id}`, so the canonical
    // `modkit::api::response::created_json` helper (which stamps a
    // `Location` pointing at the new resource) would emit a header
    // that resolves to 404 on follow-up. The raw tuple is intentional;
    // do NOT swap to `created_json` without first landing the per-user
    // GET endpoint.
    Ok((axum::http::StatusCode::CREATED, Json(dto)))
}

/// `DELETE /account-management/v1/tenants/{tenant_id}/users/{user_id}`
///
/// Retry-safe: the plugin maps vendor "user does not exist" responses
/// to `Ok(())` per
/// `dod-idp-user-operations-contract-deprovision-idempotency`, so a
/// repeat DELETE also returns 204.
///
/// # Errors
///
/// Surfaces a canonical `Problem` envelope. Notable codes:
/// `validation` (400 — tenant not in `Active` status; `resolve_active_tenant`
/// rejects `Provisioning` / `Suspended` / `Deleted` per
/// `feature-idp-user-operations-contract` `DoD` line 292), `cross_tenant_denied`
/// (403), tenant `not_found` (404), `idp_unavailable` (503),
/// `idp_unsupported_operation` (501 — provider genuinely does not support
/// deprovisioning; this MUST NOT silently no-op per PRD §5.5).
#[tracing::instrument(
    skip(svc, ctx),
    fields(tenant_id = %tenant_id, user_id = %user_id, request_id = Empty)
)]
pub async fn delete_user(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<UserService>>,
    Path((tenant_id, user_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    svc.delete_user(&ctx, tenant_id, user_id).await?;
    Ok(no_content().into_response())
}
