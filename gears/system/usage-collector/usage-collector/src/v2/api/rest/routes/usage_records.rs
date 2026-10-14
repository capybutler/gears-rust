//! `OperationBuilder` route registration for the foundation
//! `/usage-collector/v2/records` create + read surface, and for the
//! `/records/backfill` bulk-import route registered alongside it.
//! Every route is registered with `.no_license_required()` — the
//! foundation create surface is platform-internal substrate.

use axum::Router;
use toolkit::api::canonical_prelude::*;
use toolkit::api::operation_builder::OperationBuilderODataExt;
use toolkit::api::{OpenApiRegistry, OperationBuilder};
use usage_collector_sdk::v2::UsageRecordFilterField;

use super::{dto, handlers};

const USAGE_RECORDS_TAG: &str = "Usage Records";

/// The bulk-import route carries its own tag rather than sitting under
/// [`USAGE_RECORDS_TAG`]: it is a separate operator-facing surface with
/// its own authorization story, and the published contract groups it that
/// way.
const BACKFILL_TAG: &str = "Backfill";

/// `DESIGN.md` §3.10's consistency floor, re-exported from the SDK so the
/// route descriptions here, in `usage_feed.rs` and in `reconciliation.rs`
/// render the same text `UsageCollectorPluginV2`'s trait doc binds plugins to.
///
/// The SDK owns the wording: a plugin author depends on `-sdk` alone and has
/// to be able to read it, and a second copy here would drift.
// @cpt-dod:cpt-cf-usage-collector-dod-consistency-floor-published:p1
// @cpt-dod:cpt-cf-usage-collector-dod-staleness-coupling-recorded:p1
pub(super) use usage_collector_sdk::v2::CONSISTENCY_FLOOR_STATEMENT;

/// `DESIGN.md` §3.10's read-after-write rule, published on the ingestion
/// endpoint and all three query endpoints.
// @cpt-dod:cpt-cf-usage-collector-dod-consistency-read-after-write-rule:p1
const READ_AFTER_WRITE_NOTE: &str = "Read-after-write flows (admission control, post-emit \
    summary, immediate-readback dashboards) must consume the ingestion acknowledgement, \
    never a query surface.";

/// `DESIGN.md` Section 3.10's raw-tailing caveat: published only on the raw
/// path (`list_usage_records`), the one surface it describes.
const RAW_TAILING_NOTE: &str = "Raw tailing is best-effort, not a change feed; a consumer \
    that must not miss entries reads GET /usage-collector/v2/feed instead.";

/// `DESIGN.md` Section 3.10's dedup-identity-visibility horizon: published
/// only on the ingestion surface, where
/// `cpt-cf-usage-collector-dod-dedup-visibility-consistency-horizon`'s own
/// Assertion looks for it.
const DEDUP_HORIZON_NOTE: &str = "An accepted entry's dedup identity stays visible to \
    subsequent ingestion attempts for as long as the referenced type's retention policy \
    keeps it (a per-meter floor, not a gear-wide window); a retry arriving after the entry \
    has aged out of retention draws no guaranteed outcome.";

/// Realizes `dod-pair-remains-readable` by an absence: no route in this
/// file, `usage_feed.rs` or `reconciliation.rs` registers a mutating verb
/// (`grep -rn 'OperationBuilder::\(put\|patch\|delete\)' usage-collector/src/`
/// returns nothing), and `UsageCollectorPluginV2` only creates or reads —
/// so no surface exists that could rewrite a withdrawn entry.
// @cpt-dod:cpt-cf-usage-collector-fr-ingestion:p1
// @cpt-dod:cpt-cf-usage-collector-component-ingestion-gateway:p1
// @cpt-dod:cpt-cf-usage-collector-dod-pair-remains-readable:p1
pub(super) fn register_usage_record_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
) -> Router {
    router = OperationBuilder::post("/usage-collector/v2/records")
        .operation_id("usage_collector.v2.create_usage_records")
        .summary("Create usage records")
        .description(format!(
            "Submit a batch of usage records for persistence. {READ_AFTER_WRITE_NOTE} \
             {DEDUP_HORIZON_NOTE}"
        ))
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

    // The path comes from `usage_collector_sdk::v2::BACKFILL_ROUTE_PATH`, not a
    // literal, so the SDK's past-tolerance rejection message (which points
    // here) can't drift from the route. The summary omits
    // `usage-collector-v2.yaml`'s "isolated from live ingestion" claim:
    // workload isolation isn't implemented yet (see the TODO on
    // `Service::backfill_usage_records`), and publishing it would put a
    // guarantee on the wire the code doesn't keep.
    // @cpt-dod:cpt-cf-usage-collector-dod-backfill-route:p2
    router = OperationBuilder::post(usage_collector_sdk::v2::BACKFILL_ROUTE_PATH)
        .operation_id("usage_collector.backfill_usage_records")
        .summary("Bulk historical import of periods the live path rejects")
        .description(
            "Identical validation and request shape to POST /records, differing as \
             follows: every accepted entry is stamped `origin: backfill`, the \
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

    // @cpt-flow:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-query-raw:p1
    //
    // DE0802 requires every `$`-prefixed `OData` parameter to go through
    // `OperationBuilderODataExt`, which has no `$top` method (`$top` binds
    // on the wire via `ODataParams.limit`'s `#[serde(alias = "$top")]`
    // instead). Declared by hand so `openapi_contract_tests` doesn't see an
    // accepted parameter missing from the document. The `let` only scopes
    // the `#[allow]` (unstable on expressions; on the function it would
    // exempt the other routes registered here too). Drop both once the
    // toolkit grows `with_odata_top()`.
    #[allow(unknown_lints, de0802_use_odata_ext)]
    let list_records_route = OperationBuilder::get("/usage-collector/v2/records")
        .operation_id("usage_collector.v2.list_usage_records")
        .summary("List usage records")
        .description(format!(
            "Keyset-paginated raw read over the persisted usage records. \
             {CONSISTENCY_FLOOR_STATEMENT} {READ_AFTER_WRITE_NOTE} {RAW_TAILING_NOTE}"
        ))
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
        // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-request-received
        .authenticated()
        // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-request-received
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

    // @cpt-flow:cpt-cf-usage-collector-flow-query-aggregated-usage:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-query-aggregation:p1
    router = OperationBuilder::post("/usage-collector/v2/records/aggregate")
        .operation_id("usage_collector.v2.query_aggregated_usage_records")
        .summary("Query server-side aggregated usage")
        .description(format!(
            "Server-side aggregation over the persisted usage records. Carries no \
             aggregation parameter: the fold (`SUM` / `COUNT` / `MAX` / `MIN` / \
             `LATEST`) is resolved from the queried type's declaration. \
             {CONSISTENCY_FLOOR_STATEMENT} {READ_AFTER_WRITE_NOTE}"
        ))
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
        // @cpt-begin:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-request-received
        .authenticated()
        // @cpt-end:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-request-received
        .no_license_required()
        // The range is in the body on this path (`AggregationRequest.time_range`),
        // so no `from` / `to` query parameter is declared or accepted here.
        .json_request::<dto::AggregationRequest>(
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

    // @cpt-flow:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1
    router = OperationBuilder::get("/usage-collector/v2/records/{id}")
        .operation_id("usage_collector.v2.get_usage_record")
        .summary("Get a usage record")
        .description(format!(
            "Read a single usage record by `id`. {CONSISTENCY_FLOOR_STATEMENT} \
             {READ_AFTER_WRITE_NOTE}"
        ))
        .tag(USAGE_RECORDS_TAG)
        .path_param("id", "Usage-record UUID")
        // @cpt-begin:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-get-record-submit
        .authenticated()
        // @cpt-end:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-get-record-submit
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

    router
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "usage_records_tests.rs"]
mod usage_records_tests;
