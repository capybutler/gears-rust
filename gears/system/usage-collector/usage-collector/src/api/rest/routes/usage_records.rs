//! `OperationBuilder` route registration for the foundation
//! `/usage-collector/v1/records` create + read surface, and for the
//! `/records/backfill` bulk-import route registered alongside it.
//! Every route is registered with `.no_license_required()` — the
//! foundation create surface is platform-internal substrate.

use axum::Router;
use toolkit::api::canonical_prelude::*;
use toolkit::api::operation_builder::OperationBuilderODataExt;
use toolkit::api::{OpenApiRegistry, OperationBuilder};
use usage_collector_sdk::UsageRecordFilterField;

use super::{dto, handlers};

const USAGE_RECORDS_TAG: &str = "Usage Records";

/// The bulk-import route carries its own tag rather than sitting under
/// [`USAGE_RECORDS_TAG`]: it is a separate operator-facing surface with
/// its own authorization story, and the published contract groups it that
/// way.
const BACKFILL_TAG: &str = "Backfill";

// @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-api-post-records:p1
// @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-fr-ingestion:p1
// @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-component-ingestion-gateway:p1
pub(super) fn register_usage_record_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
) -> Router {
    // @cpt-begin:cpt-cf-usage-collector-dod-usage-emission-api-post-records:p1:inst-register-route-create-records
    router = OperationBuilder::post("/usage-collector/v1/records")
        .operation_id("usage_collector.create_usage_records")
        .summary("Create usage records")
        .description("Submit a batch of usage records for persistence.")
        .tag(USAGE_RECORDS_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<dto::CreateUsageRecordsRequest>(openapi, "Usage-record create payload")
        .handler(handlers::handle_create_usage_records)
        .json_response_with_schema::<dto::CreateUsageRecordsResponse>(
            openapi,
            StatusCode::OK,
            "All records accepted",
        )
        .json_response_with_schema::<dto::CreateUsageRecordsResponse>(
            openapi,
            StatusCode::MULTI_STATUS,
            "At least one record was rejected; inspect each per-record outcome",
        )
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);
    // @cpt-end:cpt-cf-usage-collector-dod-usage-emission-api-post-records:p1:inst-register-route-create-records

    // The path comes from `usage_collector_sdk::BACKFILL_ROUTE_PATH`, not a
    // literal: the SDK's past-tolerance rejection tells a caller to resubmit
    // here, and a route that moved out from under that message would leave
    // the rejection pointing at nothing.
    //
    // `usage-collector-v1.yaml` enumerates FOUR ways this route differs from
    // `POST /records`; the description below names THREE. The omitted one is
    // workload isolation, which this slice does not implement — the route is
    // `Service::create_usage_records_for_origin` under a different origin,
    // sharing the live path's runtime, connection pool and fan-out budget
    // (the TODO on `Service::backfill_usage_records`). Publishing the
    // contract's fourth claim would put an isolation guarantee on the wire
    // that the code falsifies, so the summary drops "isolated from live
    // ingestion" for the same reason. Both divergences from the document are
    // deliberate and are recorded with the slice.
    router = OperationBuilder::post(usage_collector_sdk::BACKFILL_ROUTE_PATH)
        .operation_id("usage_collector.backfill_usage_records")
        .summary("Bulk historical import of periods the live path rejects")
        .description(
            "Identical validation and request shape to POST /records, differing in \
             three respects: every accepted entry is stamped `origin: backfill`, the \
             live path's past bound on the covered period does not apply because this \
             route exists for exactly the periods that bound rejects, and submissions \
             whose covered period ends further back than the configured backfill \
             window require elevated authorization. The route takes measurements and \
             invalidation entries alike, mixed in one batch.",
        )
        .tag(BACKFILL_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<dto::CreateUsageRecordsRequest>(openapi, "Usage-record import payload")
        .handler(handlers::handle_backfill_usage_records)
        .json_response_with_schema::<dto::CreateUsageRecordsResponse>(
            openapi,
            StatusCode::OK,
            "Every entry accepted or deduplicated",
        )
        .json_response_with_schema::<dto::CreateUsageRecordsResponse>(
            openapi,
            StatusCode::MULTI_STATUS,
            "At least one entry rejected; inspect each per-entry outcome",
        )
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    // @cpt-flow:cpt-cf-usage-collector-flow-usage-query-query-raw:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-query-fr-query-raw:p1
    //
    // DE0802 requires every `$`-prefixed `OData` parameter to be declared
    // through `OperationBuilderODataExt`, so that the description and the
    // schema come from one place and gears cannot drift apart on them.
    // The trait offers `with_odata_filter`, `with_odata_orderby`, and
    // `with_odata_select` — there is no `$top` method. #4422 bound `$top`
    // on the wire (`ODataParams.limit` gained `#[serde(alias = "$top")]`)
    // without adding one, so the rule has nothing to satisfy it with here.
    //
    // The choice is therefore between declaring `$top` by hand and leaving
    // an accepted parameter out of the published document. Declaring it
    // wins: an endpoint that honours a page-size spelling and does not
    // document it is the drift `openapi_contract_tests` exists to stop.
    // The `let` binding exists only to carry the attribute: attributes on
    // expressions are unstable, and putting this on the function would
    // exempt the other four routes registered here too. Drop both once
    // the toolkit grows `with_odata_top()`.
    #[allow(unknown_lints, de0802_use_odata_ext)]
    let list_records_route = OperationBuilder::get("/usage-collector/v1/records")
        .operation_id("usage_collector.list_usage_records")
        .summary("List usage records")
        .description("Keyset-paginated raw read over the persisted usage records.")
        .tag(USAGE_RECORDS_TAG)
        .query_param(
            "gts_type_id",
            true,
            "Usage-type GTS instance id (mandatory)",
        )
        // The covered-period range is a first-class parameter on this path,
        // never a `$filter` conjunct: an entry is selected when its period
        // end falls in `[from, to)`. A `GET` has no body, so the raw path
        // carries the range in the query string while the aggregate path
        // carries it in its declared request body.
        .query_param(
            "from",
            true,
            "Inclusive lower bound of the covered-period range (mandatory; RFC 3339 \
             with an offset, normalized to UTC)",
        )
        .query_param(
            "to",
            true,
            "Exclusive upper bound of the covered-period range (mandatory; RFC 3339 \
             with an offset, normalized to UTC)",
        )
        .query_param(
            "metadata.<key>",
            false,
            "Repeated metadata-filter entries; OR within a key, AND across keys",
        )
        // Both page-size spellings are declared because the toolkit `OData`
        // extractor binds `limit` with `#[serde(alias = "$top")]` and folds
        // them onto one slot; publishing only one would under-report the
        // accepted surface. Sending both in a single request is ambiguous
        // and the extractor rejects it.
        .query_param_typed(
            "$top",
            false,
            "Page size, canonical OData spelling (rejected with 400 if above 1000)",
            "integer",
        )
        .query_param_typed(
            "limit",
            false,
            "Page size hint, alias of `$top` (rejected with 400 if above 1000)",
            "integer",
        )
        .query_param("cursor", false, "Opaque CursorV1 continuation token")
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-request-received
        .authenticated()
        // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-request-received
        .no_license_required()
        .handler(handlers::handle_list_usage_records)
        .json_response_with_schema::<toolkit_odata::Page<dto::UsageRecordDto>>(
            openapi,
            StatusCode::OK,
            "Usage records page",
        )
        // No `.with_odata_select()`: nothing in this gear applies the
        // projection. The handler returns whole `UsageRecordDto`s and the
        // plugin selects a fixed column list, so declaring `$select` would
        // advertise a parameter that is read and discarded.
        .with_odata_filter::<UsageRecordFilterField>()
        .with_odata_orderby::<UsageRecordFilterField>()
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);
    router = list_records_route;

    // @cpt-flow:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-query-fr-query-aggregation:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-query-api-post-records-aggregate:p1
    router = OperationBuilder::post("/usage-collector/v1/records/aggregate")
        .operation_id("usage_collector.query_aggregated_usage_records")
        .summary("Query server-side aggregated usage")
        .description(
            "Server-side aggregation over the persisted usage records. Carries no \
             aggregation parameter: the fold (`SUM` / `COUNT` / `MAX` / `MIN` / \
             `LATEST`) is resolved from the queried type's declaration.",
        )
        .tag(USAGE_RECORDS_TAG)
        .query_param(
            "gts_type_id",
            true,
            "Usage-type GTS instance id (mandatory)",
        )
        .query_param(
            "metadata.<key>",
            false,
            "Repeated metadata-filter entries; OR within a key, AND across keys",
        )
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-request-received
        .authenticated()
        // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-request-received
        .no_license_required()
        // The range is in the body on this path (`AggregationRequest.time_range`),
        // so no `from` / `to` query parameter is declared or accepted here.
        .json_request::<dto::AggregationRequestDto>(
            openapi,
            "Mandatory time range plus optional group-by dimensions",
        )
        .handler(handlers::handle_query_aggregated_usage_records)
        .json_response_with_schema::<dto::AggregationResultDto>(
            openapi,
            StatusCode::OK,
            "Aggregation result",
        )
        .with_odata_filter::<UsageRecordFilterField>()
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    // @cpt-flow:cpt-cf-usage-collector-flow-usage-emission-get-record:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-api-get-records-id:p1
    // @cpt-begin:cpt-cf-usage-collector-dod-usage-emission-api-get-records-id:p1:inst-register-route-get-record
    router = OperationBuilder::get("/usage-collector/v1/records/{id}")
        .operation_id("usage_collector.get_usage_record")
        .summary("Get a usage record")
        .description("Read a single usage record by `id`.")
        .tag(USAGE_RECORDS_TAG)
        .path_param("id", "Usage-record UUID")
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-get-record:p1:inst-get-record-submit
        .authenticated()
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-get-record:p1:inst-get-record-submit
        .no_license_required()
        .handler(handlers::handle_get_usage_record)
        .json_response_with_schema::<dto::UsageRecordDto>(
            openapi,
            StatusCode::OK,
            "The persisted usage record",
        )
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);
    // @cpt-end:cpt-cf-usage-collector-dod-usage-emission-api-get-records-id:p1:inst-register-route-get-record

    router
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "usage_records_tests.rs"]
mod usage_records_tests;
