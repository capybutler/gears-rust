//! [`OperationBuilder`](modkit::api::operation_builder::OperationBuilder)
//! route registration for the Account Management module.
//!
//! Each sub-module declares its `register_<area>_routes(router, openapi)`
//! function; [`register_routes`] wires them all into the runtime router
//! and attaches the service-level axum extensions consumed by the
//! handlers in [`super::handlers`].

use std::sync::Arc;

use axum::Router;
use modkit::api::OpenApiRegistry;

use crate::api::rest::handlers::conversions::ConcreteConversionService;
use crate::api::rest::handlers::tenants::ConcreteTenantService;
use crate::domain::metadata::service::MetadataService;
use crate::domain::user::service::UserService;

mod conversions;
mod metadata;
mod tenants;
mod users;

/// Wire every Account Management REST route into the supplied
/// `router`. Called once from
/// [`crate::module::AccountManagementModule::register_rest`].
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    tenant_service: Arc<ConcreteTenantService>,
    metadata_service: Arc<MetadataService>,
    user_service: Arc<UserService>,
    conversion_service: Arc<ConcreteConversionService>,
) -> Router {
    router = tenants::register_tenants_routes(router, openapi);
    router = metadata::register_metadata_routes(router, openapi);
    router = users::register_users_routes(router, openapi);
    router = conversions::register_conversions_routes(router, openapi);
    router = router.layer(axum::Extension(tenant_service));
    router = router.layer(axum::Extension(metadata_service));
    router = router.layer(axum::Extension(user_service));
    router = router.layer(axum::Extension(conversion_service));
    router
}
