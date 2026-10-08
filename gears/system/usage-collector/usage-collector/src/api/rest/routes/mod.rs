//! `OperationBuilder` route registration for the foundation REST surface.
//! Per-resource registrars live under this module and are composed by
//! [`register_routes`], which then attaches the shared
//! `Extension<Arc<Service>>` layer consumed by every handler.

use std::sync::Arc;

use axum::Router;
use toolkit::api::OpenApiRegistry;

use crate::api::rest::{dto, handlers};
use crate::domain::Service;

mod reconciliation;
mod usage_feed;
mod usage_records;

/// Compose every per-resource registrar onto `router`.
///
/// Split out of [`register_routes`] because it is the whole REST surface
/// minus the one thing a contract check cannot supply — the
/// `Extension<Arc<Service>>` layer the handlers need at request time.
/// `openapi_contract_tests` builds its registry through this function, so
/// a registrar reaches production and the conformance check together and
/// a registrar wired into only one of them cannot exist. Duplicating the
/// composition in the test would leave that hole open: a third registrar
/// added here and missing from the document would compare two incomplete
/// views and stay green.
///
/// **This function's three registrars are the gear's whole REST surface**,
/// which is what makes it the realizing site for
/// `cpt-cf-usage-collector-dod-no-gear-local-privacy-workflow`'s first
/// sentence — no fourth registrar exposing the clause's forbidden
/// operation vocabulary exists here. (Its second sentence, binding the storage plugin
/// interface, is realized separately at
/// `usage_collector_sdk::plugin_api::UsageCollectorPluginV1` — a different
/// seam, marked at its own declaration.) Pinned by
/// `data_classification_tests::the_gear_exposes_no_privacy_workflow_operation`,
/// which scans every production file under this crate's `src/`, including
/// this one and the three it composes.
// @cpt-dod:cpt-cf-usage-collector-dod-no-gear-local-privacy-workflow:p3
fn register_api_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = usage_records::register_usage_record_routes(router, openapi);
    router = usage_feed::register_usage_feed_routes(router, openapi);
    router = reconciliation::register_reconciliation_routes(router, openapi);
    router
}

/// Register the foundation REST routes onto `router`. Called once
/// from [`crate::module::UsageCollectorModule::register_rest`].
///
/// The `Extension<Arc<Service>>` layer this adds over
/// [`register_api_routes`] is invisible to the registry, so it is pinned
/// by dispatch instead — see [`registration_tests`].
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<Service>,
) -> Router {
    register_api_routes(router, openapi).layer(axum::Extension(service))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod consistency_floor_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod openapi_contract_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod registration_tests;
