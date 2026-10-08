//! `OperationBuilder` route registration for the Query Gateway's fourth read
//! path, `GET /usage-collector/v1/reconciliation` (DESIGN §3.2).

use axum::Router;
use toolkit::api::canonical_prelude::*;
use toolkit::api::{OpenApiRegistry, OperationBuilder};

use super::usage_records::CONSISTENCY_FLOOR_STATEMENT;
use super::{dto, handlers};

const RECONCILIATION_TAG: &str = "Reconciliation";

pub(super) fn register_reconciliation_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
) -> Router {
    // No `.with_odata_filter()` / `.with_odata_orderby()` and no paging
    // parameter: `cpt-cf-usage-collector-dod-reconciliation-single-scope`
    // admits a granularity, a tenant, a meter and a range, and nothing else.
    // Registering any of them would advertise a parameter nothing reads.
    // Pinned against the registered `OperationSpec` by
    // `the_reconciliation_route_registers_only_its_five_scope_and_range_parameters`
    // in `openapi_contract_tests`.
    router = OperationBuilder::get("/usage-collector/v1/reconciliation")
        .operation_id("usage_collector.get_reconciliation_metadata")
        .summary("Per-scope ingestion counters and watermarks")
        .description(format!(
            "Sufficient for an external reconciliation job to compare gear-side accepted \
             totals against a consumer's processed totals for a range without a full raw \
             scan. Stall detection is consumer-side: the gear exposes the watermarks and \
             computes no stall verdict of its own. {CONSISTENCY_FLOOR_STATEMENT}"
        ))
        .tag(RECONCILIATION_TAG)
        .query_param(
            "scope",
            true,
            "Granularity at which to report. `caller` and `caller_tenant` are reserved and \
             not served in v1.",
        )
        .query_param("gts_type_id", true, "The GTS type to report on.")
        .query_param(
            "tenant_id",
            true,
            "The tenant to report on. One scope per call.",
        )
        .query_param(
            "from",
            true,
            "Inclusive lower bound of the requested range (UTC, RFC 3339).",
        )
        .query_param(
            "to",
            true,
            "Exclusive upper bound of the requested range (UTC, RFC 3339).",
        )
        .authenticated()
        .no_license_required()
        .handler(handlers::handle_get_reconciliation_metadata)
        .json_response_with_schema::<dto::ReconciliationMetadataDto>(
            openapi,
            StatusCode::OK,
            "Reconciliation metadata for the requested scope",
        )
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);
    router
}
