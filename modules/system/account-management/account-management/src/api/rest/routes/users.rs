//! `OperationBuilder` route registration for the
//! `/account-management/v1/tenants/{tenant_id}/users*` endpoints.

use axum::Router;
use modkit::api::OpenApiRegistry;
use modkit::api::operation_builder::OperationBuilder;

use crate::api::rest::{dto, handlers};

const API_TAG: &str = "Tenant Users";

/// Collection path: `GET / POST /tenants/{tenant_id}/users`.
const COLLECTION_PATH: &str = "/account-management/v1/tenants/{tenant_id}/users";
/// Single-user path: `DELETE /tenants/{tenant_id}/users/{user_id}`.
const ENTRY_PATH: &str = "/account-management/v1/tenants/{tenant_id}/users/{user_id}";

pub(super) fn register_users_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // GET /account-management/v1/tenants/{tenant_id}/users
    router = OperationBuilder::get(COLLECTION_PATH)
        .operation_id("account_management.list_tenant_users")
        .summary("List users provisioned in a tenant")
        .description(
            "List users provisioned in the tenant via the configured IdP plugin. Supplying \
             `user_id` turns the response into an authoritative existence check -- the page \
             contains at most one element and an empty page is the canonical \"user absent\" \
             signal (no 404). When `user_id` is supplied the server pins `top=1` and ignores \
             any client-provided `limit` / `cursor` -- the filtered shape is an existence \
             check, not a paginated query. AM persists no local user state; every read is a \
             pass-through to the IdP.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("tenant_id", "Tenant UUID")
        // `query_param_typed(..., "uuid")` currently degrades to a
        // plain-string schema in modkit's `OpenApiRegistry` (the
        // registry special-cases only `integer` / `number` /
        // `boolean`). The runtime serde-side deserialization still
        // rejects malformed UUIDs at the wire boundary via
        // `Query<ListUsersQuery>`'s `user_id: Option<Uuid>` field,
        // so callers see a clean 400 on bad input regardless of
        // OpenAPI fidelity. The `"uuid"` hint is kept so the
        // registry picks the format up automatically once modkit
        // adds string-format awareness; see the runtime-spec drift
        // pinned in the route-level test plan.
        .query_param_typed(
            "user_id",
            false,
            "Optional UUID user identifier filter for point lookup behavior.",
            "uuid",
        )
        .query_param_typed(
            "limit",
            false,
            "Maximum number of users to return (1..200, default 50).",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .handler(handlers::list_users)
        .json_response_with_schema::<modkit_odata::Page<dto::UserDto>>(
            openapi,
            http::StatusCode::OK,
            "Paginated list of tenant users",
        )
        .standard_errors(openapi)
        // 501 / 503 are outside the standard set: `idp_unsupported_operation`
        // and `idp_unavailable` surface from `IdpPluginClient`.
        .problem_response(
            openapi,
            http::StatusCode::NOT_IMPLEMENTED,
            "IdP plugin does not support the requested operation",
        )
        .problem_response(
            openapi,
            http::StatusCode::SERVICE_UNAVAILABLE,
            "IdP unavailable (transport failure or timeout)",
        )
        .register(router, openapi);

    // POST /account-management/v1/tenants/{tenant_id}/users
    router = OperationBuilder::post(COLLECTION_PATH)
        .operation_id("account_management.create_tenant_user")
        .summary("Provision a user in a tenant")
        .description(
            "Provision a user in the tenant via the configured IdP plugin. AM persists no \
             local user state -- the IdP becomes the source of truth on success. Returns \
             HTTP 201 Created with the projected user body. AM does NOT expose a per-user \
             GET; clients that need to re-read the user use the filtered listing \
             `GET /tenants/{tenant_id}/users?user_id=<id>`.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("tenant_id", "Tenant UUID")
        .json_request::<dto::UserCreateRequestDto>(openapi, "User provisioning payload")
        .handler(handlers::create_user)
        .json_response_with_schema::<dto::UserDto>(
            openapi,
            http::StatusCode::CREATED,
            "Tenant user provisioned",
        )
        .standard_errors(openapi)
        .problem_response(
            openapi,
            http::StatusCode::NOT_IMPLEMENTED,
            "IdP plugin does not support user provisioning",
        )
        .problem_response(
            openapi,
            http::StatusCode::SERVICE_UNAVAILABLE,
            "IdP unavailable (transport failure or timeout)",
        )
        .register(router, openapi);

    // DELETE /account-management/v1/tenants/{tenant_id}/users/{user_id}
    router = OperationBuilder::delete(ENTRY_PATH)
        .operation_id("account_management.delete_tenant_user")
        .summary("Deprovision a user from a tenant")
        .description(
            "Deprovision a user from the tenant via the configured IdP plugin. Idempotent: \
             returns 204 No Content whether the IdP removed the row or reported the user as \
             already absent; a subsequent retry of the same DELETE also returns 204. \
             Provider implementations MUST NOT silently no-op on a genuinely supported \
             mutating operation -- unsupported deprovision surfaces as \
             `code=idp_unsupported_operation`.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("tenant_id", "Tenant UUID")
        .path_param("user_id", "IdP-issued UUID user identifier")
        .handler(handlers::delete_user)
        .no_content_response(http::StatusCode::NO_CONTENT, "User deprovisioned")
        .standard_errors(openapi)
        .problem_response(
            openapi,
            http::StatusCode::NOT_IMPLEMENTED,
            "IdP plugin does not support user deprovisioning",
        )
        .problem_response(
            openapi,
            http::StatusCode::SERVICE_UNAVAILABLE,
            "IdP unavailable (transport failure or timeout)",
        )
        .register(router, openapi);

    router
}
