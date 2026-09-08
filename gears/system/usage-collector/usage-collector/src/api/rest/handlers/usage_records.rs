//! REST handlers for the foundation `/usage-collector/v1/records`
//! create + read surface. Each handler is a thin pass-through: it pulls
//! the gateway-resolved `SecurityContext`, dispatches to the domain
//! [`Service`], and lifts `UsageCollectorError` through the host-owned
//! canonical mapping. PDP authorization runs inside the `Service`
//! method each handler dispatches to.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Extension, Path, Query};
use time::OffsetDateTime;
use toolkit::api::canonical_prelude::*;
use toolkit_canonical_errors::Problem;
use toolkit_odata::{ODataQuery, Page as ODataPage};
use toolkit_security::SecurityContext;
use usage_collector_sdk::{
    AggregationDimension, CreateUsageRecord, IdempotencyKey, Invalidation, MetadataFilter,
    MetadataKey, MeterTypeId, ReasonCode, ResourceRef, SubjectRef, TimeRange, UsageCollectorError,
    UsageRecord,
};
use uuid::Uuid;

use crate::api::rest::dto::{
    AggregationResultDto, CreateUsageRecordRequest, CreateUsageRecordResultDto,
    CreateUsageRecordsRequest, CreateUsageRecordsResponse, QueryAggregatedUsageRecordsRequest,
    UsageRecordDto,
};
use crate::domain::Service;
use crate::domain::query::establish_keyset_order;
use crate::domain::service::MAX_BATCH_RECORDS;
use crate::infra::sdk_error_mapping::{
    UsageRecordResource,
    usage_collector_error_to_canonical_for_usage_record as usage_collector_error_to_canonical,
    usage_record_error_to_problem,
};

/// `POST /usage-collector/v1/records`
///
/// Batch-create one or more usage records. Per-record validation /
/// authorization / dispatch failures surface as `Rejected` entries inside
/// the response envelope at their input index; the response status is
/// `200 OK` when every record was accepted and `207 Multi-Status` when
/// at least one record was rejected.
///
/// Whole-request failures (handle resolution, batch SPI dispatch) still
/// short-circuit through the canonical `Problem` envelope.
// @cpt-flow:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1
// @cpt-dod:cpt-cf-usage-collector-dod-foundation-entity-security-context:p1
// @cpt-dod:cpt-cf-usage-collector-dod-foundation-principle-fail-closed:p2
// @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-api-post-records:p1
// @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-entity-security-context:p1
pub async fn handle_create_usage_records(
    // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization:p1:inst-algo-attrib-receive-ctx
    // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-submit
    // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-missing-ctx
    Extension(ctx): Extension<SecurityContext>,
    // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-missing-ctx
    Extension(service): Extension<Arc<Service>>,
    Json(req): Json<CreateUsageRecordsRequest>,
    // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-submit
) -> ApiResult<impl IntoResponse> {
    // Mirror `Service::create_usage_records`' `1..=MAX_BATCH_RECORDS` gate at
    // the handler so an oversized or empty wire payload is rejected as
    // `InvalidArgument` before the per-record loop allocates / iterates.
    // The service still enforces the same invariant for non-REST callers.
    let actual = req.records.len();
    if actual == 0 || actual > MAX_BATCH_RECORDS {
        return Err(usage_collector_error_to_canonical(
            UsageCollectorError::invalid_batch_size(actual, 1, MAX_BATCH_RECORDS),
        ));
    }

    let mut indexed_results: Vec<(usize, CreateUsageRecordResultDto)> =
        Vec::with_capacity(req.records.len());
    let mut eligible: Vec<(usize, CreateUsageRecord)> = Vec::new();

    for (index, item) in req.records.into_iter().enumerate() {
        match record_request_into_domain(item) {
            Ok(record) => eligible.push((index, record)),
            Err(problem) => indexed_results.push((
                index,
                CreateUsageRecordResultDto::Rejected {
                    index,
                    error: problem,
                },
            )),
        }
    }
    // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization:p1:inst-algo-attrib-receive-ctx

    if !eligible.is_empty() {
        let (indices, batch): (Vec<usize>, Vec<CreateUsageRecord>) = eligible.into_iter().unzip();

        // Batch-level dispatch failure (plugin resolution, SPI size
        // mismatch) bubbles through `?` as a whole-request canonical
        // envelope — the same failure would have hit every record
        // identically. `Service::create_usage_records` post-condition:
        // one result per dispatched record, in order.
        let per_record = service
            .create_usage_records(&ctx, batch)
            .await
            .map_err(usage_collector_error_to_canonical)?;
        for (index, outcome) in indices.into_iter().zip(per_record) {
            indexed_results.push((index, per_record_outcome(index, outcome)));
        }
    }

    // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-compose-response
    indexed_results.sort_by_key(|(idx, _)| *idx);
    let results: Vec<CreateUsageRecordResultDto> =
        indexed_results.into_iter().map(|(_, item)| item).collect();
    // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-compose-response

    let any_rejected = results
        .iter()
        .any(|item| matches!(item, CreateUsageRecordResultDto::Rejected { .. }));

    // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-return-200
    // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-return-207
    let status = if any_rejected {
        StatusCode::MULTI_STATUS
    } else {
        StatusCode::OK
    };

    Ok((status, Json(CreateUsageRecordsResponse { results })))
    // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-return-207
    // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-return-200
}

/// `GET /usage-collector/v1/records/{id}`
///
/// Read a single usage record by `uuid`. A malformed `uuid` path segment
/// surfaces as the canonical `InvalidArgument` problem; a missing record
/// surfaces as the canonical `NotFound` problem; a PDP denial surfaces as
/// `Forbidden`; a Plugin SPI transport / readiness / persistence fault
/// surfaces as `ServiceUnavailable`. On success the response is HTTP 200
/// with the wire-projected [`UsageRecordDto`] body.
// @cpt-flow:cpt-cf-usage-collector-flow-usage-emission-get-record:p1
// @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-api-get-records-id:p1
pub async fn handle_get_usage_record(
    // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-get-record:p1:inst-get-record-missing-ctx
    Extension(ctx): Extension<SecurityContext>,
    // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-get-record:p1:inst-get-record-missing-ctx
    Extension(service): Extension<Arc<Service>>,
    Path(uuid_raw): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = parse_record_id(&uuid_raw)?;
    // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-get-record:p1:inst-get-record-spi-fail
    let record = service
        .get_usage_record(&ctx, id)
        .await
        .map_err(usage_collector_error_to_canonical)?;
    // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-get-record:p1:inst-get-record-spi-fail
    // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-get-record:p1:inst-get-record-success
    Ok((StatusCode::OK, Json(UsageRecordDto::from(record))))
    // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-get-record:p1:inst-get-record-success
}

/// `GET /usage-collector/v1/records`
///
/// Keyset-paginated raw read over the persisted usage records.
///
/// `gts_type_id`, `from` and `to` are the mandatory non-OData query
/// parameters, and all three carry typed values: the SDK trait and the
/// plugin SPI take `gts_type_id` as a [`MeterTypeId`] and the `from` / `to`
/// pair as one validated [`TimeRange`]. An entry is selected when the end
/// of its covered period falls in that range — `from <= window_end < to`
/// (`cpt-cf-usage-collector-adr-window-end-selection`).
///
/// The range never travels inside `$filter`: a predicate naming either
/// covered-period bound is rejected, because it would be a second,
/// possibly contradictory, constraint on something the range already
/// fixes. A missing, duplicated, offset-less, or inverted bound is a
/// `400 InvalidArgument` raised where the parameter is parsed — the same
/// place and the same order as a malformed `gts_type_id`, which has always
/// been parsed before the PDP call. `$filter` / `$orderby` / `$top` (alias
/// `limit`) / `cursor` flow through the standard [`OData`] extractor.
///
/// Gateway-side guards applied before the service is invoked:
///
/// * **`$top` cap** — `ODataQuery.limit` is bounded by [`MAX_PAGE_SIZE`].
///   A caller passing `?$top=1000000` receives a `400 InvalidArgument`
///   so they cannot silently misinterpret a clamped page as complete.
/// * **Cursor decoding** — when a `cursor` is present, its signed keys are
///   decoded into the effective `$orderby`; a malformed token surfaces as
///   the canonical `cursor_decode` `Problem`. The decoded `CursorV1` flows
///   to the plugin via `ODataQuery.cursor` unchanged. Both of the
///   *comparisons* a cursor needs happen behind the service, which is the
///   only layer an in-process caller also passes through: the order the
///   plugin sorts by is taken from the token there, and whether the token
///   was
///   minted over *this* query — the caller's `$filter` together with
///   `gts_type_id`, the `from` / `to` range and every `metadata.<key>`
///   filter — is checked behind the service, which is the only layer both
///   surfaces pass through, and refused as `FILTER_MISMATCH` against
///   `cursor`.
/// * **`$orderby` admissibility and normalization** — a caller order is
///   refused here, naming `$orderby`, when it mixes sort directions or
///   names a key that is not a mandatory record attribute; an admissible
///   one then gains whichever of `window_end` / `id` it does not already
///   name, in its own sort direction, so the sort tuple is unique. An
///   omitted `$orderby` yields `(window_end asc, id asc)`; an
///   `$orderby=id` yields `(id asc, window_end asc)`, because the missing
///   key is appended and a named one is left where the caller put it. Both
///   halves are [`establish_keyset_order`]: the invariant is owned
///   in the domain so that an in-process caller gets it too, and mirrored
///   here so the caller's own input is blamed by name. Same arrangement as
///   the batch-size gate on the create surface above. A continuation has
///   no caller order to normalize; the order its token was minted under is
///   checked behind the service instead, and a token bound to an unsound
///   keyset is refused as `INVALID_CURSOR` against `cursor`.
///
/// Per-key metadata filtering is the typed side-channel
/// [`MetadataFilter`] from the SDK — `toolkit-odata` has no surface for
/// filtering on dynamic JSON-map keys. The wire encoding is **repeated
/// query parameters of the form `metadata.<key>=<value>`**:
///
/// * `?metadata.user_id=u1&metadata.user_id=u2` → one filter on
///   `user_id` whose value set is `{u1, u2}` (OR within the key).
/// * `?metadata.user_id=u1&metadata.region=eu` → two filters
///   `user_id ∈ {u1}` AND `region ∈ {eu}` (AND across keys).
/// * Missing entirely → no metadata filter.
///
/// PDP authorization, PDP-constraint composition into the `OData` filter,
/// and the plugin SPI dispatch all happen inside
/// [`Service::list_usage_records`]; this handler is a thin wrapper that
/// applies the gateway-side guards, parses the typed query parameters,
/// and projects the returned records to the wire [`UsageRecordDto`]
/// shape.
// @cpt-flow:cpt-cf-usage-collector-flow-usage-query-query-raw:p1
// @cpt-dod:cpt-cf-usage-collector-dod-usage-query-fr-query-raw:p1
// @cpt-dod:cpt-cf-usage-collector-dod-usage-query-constraint-nfr-thresholds:p1
// @cpt-dod:cpt-cf-usage-collector-dod-usage-query-cursor-v1-toolkit-adoption:p1
pub async fn handle_list_usage_records(
    // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-missing-ctx
    Extension(ctx): Extension<SecurityContext>,
    // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-missing-ctx
    Extension(service): Extension<Arc<Service>>,
    Query(params): Query<Vec<(String, String)>>,
    OData(query): OData,
) -> ApiResult<Json<ODataPage<UsageRecordDto>>> {
    // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-request-received
    let PreparedListRequest {
        gts_type_id,
        time_range,
        metadata_filter,
        query,
    } = prepare_list_request(&params, query)?;
    // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-request-received

    let page = service
        .list_usage_records(&ctx, gts_type_id, time_range, &query, &metadata_filter)
        .await
        .map_err(usage_collector_error_to_canonical)?;

    // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-return
    Ok(Json(page.map_items(UsageRecordDto::from)))
    // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-return
}

/// `POST /usage-collector/v1/records/aggregate`
///
/// Server-side aggregated read over the persisted usage records.
///
/// The wire shape mirrors `GET /usage-collector/v1/records` apart from where
/// the range travels: `gts_type_id` is a mandatory typed query parameter and
/// the `OData` `$filter` plus the `metadata.<key>=<value>` typed
/// side-channel flow through query parameters, while the mandatory
/// `time_range` and the group-by dimensions ship in the JSON body
/// (`AggregationRequest` in `docs/usage-collector-v1.yaml`). There is no
/// aggregation parameter: the fold is resolved from the queried type's
/// declaration.
///
/// `from` / `to` are deliberately NOT accepted in the query string here.
/// The aggregate path is a `POST` with a declared body, so the contract
/// puts the range there; admitting the query-string spelling as well would
/// accept a parameter nothing reads, which is exactly the drift the
/// parameter allowlists exist to stop. Selection is the same single
/// predicate either way — `from <= window_end < to`
/// (`cpt-cf-usage-collector-adr-window-end-selection`) — and the range is
/// never a `$filter` conjunct.
///
/// `$orderby`, `$top` / `limit`, and `cursor` are likewise not accepted —
/// the aggregation result is not paginated (the SDK contract emits one
/// `AggregationResult` per call).
///
/// PDP authorization, declaration resolution, PDP-constraint composition
/// into the `OData` filter, and the plugin SPI dispatch all happen inside
/// [`Service::query_aggregated_usage_records`]; this handler is a thin
/// wrapper that parses the typed query parameters, lifts the
/// [`QueryAggregatedUsageRecordsRequest`] body into typed group-by
/// dimensions, dispatches to the service, and projects the result to the
/// wire [`AggregationResultDto`] shape.
// @cpt-flow:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1
// @cpt-dod:cpt-cf-usage-collector-dod-usage-query-fr-query-aggregation:p1
// @cpt-dod:cpt-cf-usage-collector-dod-usage-query-api-post-records-aggregate:p1
pub async fn handle_query_aggregated_usage_records(
    // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-missing-ctx
    Extension(ctx): Extension<SecurityContext>,
    // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-missing-ctx
    Extension(service): Extension<Arc<Service>>,
    Query(params): Query<Vec<(String, String)>>,
    OData(query): OData,
    Json(req): Json<QueryAggregatedUsageRecordsRequest>,
) -> ApiResult<Json<AggregationResultDto>> {
    // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-request-received
    let PreparedAggregateRequest {
        gts_type_id,
        time_range,
        metadata_filter,
        query,
        group_by,
    } = prepare_aggregate_request(&params, query, req)?;
    // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-request-received

    let result = service
        .query_aggregated_usage_records(
            &ctx,
            gts_type_id,
            time_range,
            &query,
            &metadata_filter,
            &group_by,
        )
        .await
        .map_err(usage_collector_error_to_canonical)?;

    // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-return
    Ok(Json(AggregationResultDto::from(result)))
    // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-return
}

/// Everything the aggregate path's pre-service validators produce.
///
/// A struct rather than a tuple. Every element type happens to be distinct
/// today, so a positional swap would not compile — but that is an accident
/// of the current field set, and the first field added alongside a
/// same-typed sibling takes the accident away silently. Named fields make
/// the swap impossible instead of merely inconvenient, which matters most
/// for `time_range`: a range crossed with another parameter is an
/// unbounded or wrong-window scan that still answers `200`.
struct PreparedAggregateRequest {
    gts_type_id: MeterTypeId,
    time_range: TimeRange,
    metadata_filter: Vec<MetadataFilter>,
    query: ODataQuery,
    group_by: Vec<AggregationDimension>,
}

/// Bundle of every aggregate-path pre-service validator: parameter
/// allowlist, typed `gts_type_id`, metadata filters, and the body-shape
/// projection into the typed [`TimeRange`] and [`AggregationDimension`]s.
/// Propagates the canonical envelope verbatim on the first failing
/// validator.
///
/// The range comes out of the body here, not the query string
/// (`AggregationRequest.time_range`), so `deny_unknown_fields` on the DTO
/// has already refused a body carrying anything else and serde has already
/// refused one omitting `time_range`; what is left to check is the
/// ordering, which [`TimeRange::new`] owns.
fn prepare_aggregate_request(
    params: &[(String, String)],
    query: ODataQuery,
    req: QueryAggregatedUsageRecordsRequest,
) -> Result<PreparedAggregateRequest, CanonicalError> {
    reject_unknown_aggregate_params(params)?;
    let gts_type_id = parse_required_gts_type_id(params)?;
    let time_range =
        TimeRange::try_from(req.time_range).map_err(usage_collector_error_to_canonical)?;
    let metadata_filter = parse_metadata_filters(params)?;
    let group_by = req
        .into_group_by()
        .map_err(usage_collector_error_to_canonical)?;
    Ok(PreparedAggregateRequest {
        gts_type_id,
        time_range,
        metadata_filter,
        query,
        group_by,
    })
}

/// `$`-prefixed `OData` parameters accepted on the aggregate path. `$top`
/// (alias `limit`) and `cursor` are intentionally excluded — aggregation
/// is not paginated. `$select` is excluded on both paths, for the reason
/// given on [`OUR_ODATA_PARAMS`].
const AGGREGATE_ODATA_PARAMS: &[&str] = &["$filter"];

/// Reject any query parameter on the aggregate path that is not in the
/// declared aggregate-OData set, the typed aggregate parameters
/// (`gts_type_id`), or a `metadata.<key>` entry. Silent drop of
/// unrecognised parameters is a documented contract-drift surface,
/// mirroring `list_usage_records`.
///
/// Checks [`TYPED_AGGREGATE_PARAMS`] rather than [`TYPED_LIST_PARAMS`], so
/// `from` / `to` are named in a `400` here instead of being accepted and
/// ignored: the aggregate path reads its range from the request body.
fn reject_unknown_aggregate_params(params: &[(String, String)]) -> Result<(), CanonicalError> {
    if let Some((key, _)) = params.iter().find(|(k, _)| {
        !AGGREGATE_ODATA_PARAMS.contains(&k.as_str())
            && !TYPED_AGGREGATE_PARAMS.contains(&k.as_str())
            && !k.starts_with(METADATA_PREFIX)
    }) {
        return Err(UsageRecordResource::invalid_argument()
            .with_field_violation(
                key,
                format!(
                    "unrecognised query parameter `{key}`; expected one of \
                     {TYPED_AGGREGATE_PARAMS:?}, OData parameters \
                     {AGGREGATE_ODATA_PARAMS:?}, or `metadata.<key>` entries"
                ),
                "VALIDATION",
            )
            .create());
    }
    Ok(())
}

/// Everything the raw path's pre-service validators produce — see
/// [`PreparedAggregateRequest`] for why this is a struct and not a tuple.
struct PreparedListRequest {
    gts_type_id: MeterTypeId,
    time_range: TimeRange,
    metadata_filter: Vec<MetadataFilter>,
    query: ODataQuery,
}

/// Bundle of every pre-service validator: parameter allowlist, typed
/// `gts_type_id`, the typed `from` / `to` range, metadata filters, and the
/// `prepare_list_query` gateway-side guards. Propagates the canonical
/// envelope verbatim on the first failing validator.
fn prepare_list_request(
    params: &[(String, String)],
    query: ODataQuery,
) -> Result<PreparedListRequest, CanonicalError> {
    reject_unknown_list_params(params)?;
    let gts_type_id = parse_required_gts_type_id(params)?;
    let time_range = parse_required_time_range(params)?;
    let metadata_filter = parse_metadata_filters(params)?;
    let query = prepare_list_query(query)?;
    Ok(PreparedListRequest {
        gts_type_id,
        time_range,
        metadata_filter,
        query,
    })
}

/// Maximum number of records the gateway will request from the plugin
/// in a single page per
/// `cpt-cf-usage-collector-dod-usage-query-constraint-nfr-thresholds`.
/// A caller-supplied `$top` / `limit` above this ceiling is rejected with
/// 400 `InvalidArgument` (never silently clamped) so the plugin cannot be
/// coaxed into unbounded reads.
pub const MAX_PAGE_SIZE: u64 = 1000;

/// Maximum number of distinct `metadata.<key>` filters accepted on a
/// single list / aggregate query. Each filter expands into a plugin- /
/// DB-side predicate, so the cap bounds query cost end-to-end.
pub const MAX_METADATA_FILTERS: usize = 16;

/// Maximum number of values inside a single `metadata.<key>=…` filter
/// (OR-within-key). The plugin must translate the value set into a
/// `key IN (...)` predicate; capping the cardinality keeps that
/// rewrite bounded.
pub const MAX_METADATA_FILTER_VALUES: usize = 32;

/// `$`-prefixed `OData` parameters (parsed by the toolkit `OData`
/// extractor) and the non-`OData` scalars the toolkit also accepts.
///
/// Both `$top` (canonical `OData`, OASIS `OData` 4.01 Part 2 §5.1.6) and
/// `limit` (toolkit alias) are admitted: [`toolkit::api::odata::ODataParams`]
/// declares `limit` with `#[serde(alias = "$top")]`, so the extractor folds
/// both spellings onto the same `ODataQuery.limit` slot. Sending both in one
/// request is ambiguous and the extractor rejects it as a duplicate field
/// before this allowlist runs.
///
/// `$select` is absent on purpose. The toolkit extractor parses it into
/// `ODataQuery.select`, but this gear never applies the projection — the
/// handler returns whole [`UsageRecordDto`]s and the plugin selects a
/// fixed column list — so admitting it would take a caller's field list,
/// discard it, and return every field under a `200` that reads as the
/// requested projection. Rejecting names the parameter instead of
/// leaving the caller to diff the response against what they asked for.
const OUR_ODATA_PARAMS: &[&str] = &["$filter", "$orderby", "$top", "limit", "cursor"];

/// Typed query parameters on the raw path carrying SDK values that are NOT
/// part of the `OData` surface. The covered-period range travels here
/// rather than in `$filter` (DESIGN §3.3 rule 5).
const TYPED_LIST_PARAMS: &[&str] = &["gts_type_id", "from", "to"];

/// Typed query parameters on the aggregate path. The range is in the
/// request body there (`AggregationRequest.time_range`), so `from` / `to`
/// are not accepted in the query string — an accepted-and-ignored
/// parameter is exactly the drift this allowlist exists to stop.
const TYPED_AGGREGATE_PARAMS: &[&str] = &["gts_type_id"];

/// Prefix marking the typed-side-channel [`MetadataFilter`] entries
/// (`metadata.<key>=<value>`, repeatable).
const METADATA_PREFIX: &str = "metadata.";

/// Apply gateway-side guards on the parsed [`ODataQuery`]:
/// 1. reject `$top > MAX_PAGE_SIZE` as `InvalidArgument` (no silent
///    clamp; a caller asking for more rows than the page-size cap MUST
///    be told so they can paginate explicitly);
/// 2. run the domain's keyset floor [`establish_keyset_order`] on the
///    caller's `$orderby`, which refuses a mixed-direction or
///    non-mandatory order key and otherwise appends whichever of
///    `window_end` / `id` the caller did not name, in their direction;
/// 3. decode the optional cursor's signed tokens, refusing a malformed
///    token, and materialize the order they carry onto the query — a
///    mirror of the domain's own binding, which is the authority.
///
/// Step 2 exists here, and not only behind the service, so a caller's own
/// input is refused where it is parsed and the `400` blames `$orderby` —
/// the wire parameter they actually sent — before any authorization or
/// plugin work happens. The floor itself is the domain's, and the service
/// applies it again before dispatch; it is idempotent, so the second
/// application is a no-op on this path.
// @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-odata-parse
// @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-cursor-validate
fn prepare_list_query(mut query: ODataQuery) -> Result<ODataQuery, CanonicalError> {
    // 1. $top cap. Reject above the cap so the caller observes the
    //    boundary rather than silently receiving a truncated page that
    //    looks complete.
    //
    //    The violation names `$top` — the canonical spelling — because the
    //    parsed `ODataQuery` does not record which of the two accepted
    //    spellings arrived on the wire, and the detail names the alias so
    //    a `limit` caller still recognises their own parameter. Same
    //    convention as `toolkit_odata`'s own `InvalidLimit` mapping.
    match query.limit {
        Some(l) if l > MAX_PAGE_SIZE => {
            return Err(UsageRecordResource::invalid_argument()
                .with_field_violation(
                    "$top",
                    format!("page size ($top, alias limit) must be <= {MAX_PAGE_SIZE}, got {l}"),
                    "VALIDATION",
                )
                .create());
        }
        // Within the cap: keep the caller's page size unchanged.
        Some(_) => {}
        None => query.limit = Some(MAX_PAGE_SIZE),
    }

    // 2. $orderby admissibility + the canonical keyset fields, both from
    // the domain's first-page floor. It refuses a mixed-direction order and an
    // order key that is not a mandatory record attribute — `created_at` is
    // one such name now, so a stale caller order fails closed rather than
    // resolving — and otherwise appends whichever of `window_end` / `id`
    // the caller did not name, in their own direction, so the sort tuple
    // is globally unique. Without both names the plugin's keyset predicate
    // would skip rows sharing the boundary value that did not fit on the
    // previous page: silent data loss across page boundaries.
    //
    // Skipped when a cursor is present: the toolkit OData extractor leaves
    // `order` empty on a cursor request (and rejects `$orderby` + `cursor`
    // together), so there is no caller order to floor yet — the effective
    // one is reconstructed from the token's signed keys in step 3, and the
    // service then puts it through `require_continuation_keyset`, which
    // checks rather than extends.
    if query.cursor.is_none() {
        establish_keyset_order(&mut query).map_err(usage_collector_error_to_canonical)?;
    }

    // 3. Cursor validation + order materialization. When a cursor is
    // present, `query.order` is empty by toolkit convention; the effective
    // keyset order lives in the cursor's signed-token payload (`cursor.s`).
    // Derive it, validate the cursor against it, then write it back into
    // `query.order` so it propagates to the storage plugin. The plugin
    // reads `query.order` directly to build BOTH the
    // `ORDER BY` and the keyset continuation predicate and has no access to
    // the cursor's token derivation — leaving the order empty makes the
    // plugin reject the continuation with "keyset order must not be empty"
    // (surfacing as a 500 on every cursor follow-up).
    if let Some(cursor) = query.cursor.as_ref() {
        // Decoding is what happens here, and only decoding: a token whose
        // signed keys are malformed is refused where it arrives, in wire
        // vocabulary. Both *comparisons* `toolkit_odata::validate_cursor_against`
        // would make are gone from this edge, for two different reasons.
        //
        // The filter hash, because the query a continuation is bound to is
        // the caller's `$filter` AND all three typed parameters, and the
        // service owns that fingerprint end to end (`read_fingerprint`).
        // The extractor's `filter_hash` covers `$filter` alone, so an edge
        // comparing it against a cursor the plugin minted from the wider
        // value would reject every legitimate page two — and teaching the
        // edge to recompute the wider value would not fix it either,
        // because `filter_hash` is `None` for an in-process caller, so the
        // edge cannot be the owner.
        //
        // The order, because comparing it here is vacuous by construction:
        // `equals_signed_tokens` parses `cursor.s` with the same rules
        // `from_signed_tokens` just used, so a value derived from `s` can
        // never fail a comparison against `s`. The comparison that is not
        // vacuous is against the order the plugin will actually sort by,
        // and that lives behind the service in `bind_continuation_order`,
        // which overwrites rather than compares — the only form that holds
        // for an in-process caller, who sets `order` and `cursor`
        // independently.
        //
        // The assignment below is a mirror of that binding, not the
        // authority for it: same source, same result, so the service
        // re-deriving it is a no-op. Same arrangement as
        // `establish_keyset_order` on the first-page path above.
        query.order = toolkit_odata::ODataOrderBy::from_signed_tokens(&cursor.s)
            .map_err(CanonicalError::from)?;
    }

    Ok(query)
}
// @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-cursor-validate
// @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-odata-parse

/// Reject any query parameter that is not one of the declared `OData`
/// parameters, one of the typed list parameters, or a
/// `metadata.<key>` entry — silent drop of unrecognised parameters is a
/// documented contract-drift surface.
fn reject_unknown_list_params(params: &[(String, String)]) -> Result<(), CanonicalError> {
    if let Some((key, _)) = params.iter().find(|(k, _)| {
        !OUR_ODATA_PARAMS.contains(&k.as_str())
            && !TYPED_LIST_PARAMS.contains(&k.as_str())
            && !k.starts_with(METADATA_PREFIX)
    }) {
        return Err(UsageRecordResource::invalid_argument()
            .with_field_violation(
                key,
                format!(
                    "unrecognised query parameter `{key}`; expected one of \
                     {TYPED_LIST_PARAMS:?}, OData parameters \
                     {OUR_ODATA_PARAMS:?}, or `metadata.<key>` entries"
                ),
                "VALIDATION",
            )
            .create());
    }
    Ok(())
}

/// Extract the mandatory `gts_type_id` query parameter and validate it
/// through [`MeterTypeId::new`]. A missing value surfaces as the
/// canonical `InvalidArgument` `Problem` with a field violation on
/// `gts_type_id`; a malformed value lifts through the SDK's
/// [`UsageCollectorError::InvalidArgument`] mapping. A duplicate
/// occurrence is rejected so silent last-wins ambiguity cannot mask a
/// caller bug.
fn parse_required_gts_type_id(params: &[(String, String)]) -> Result<MeterTypeId, CanonicalError> {
    let raw = require_single_value(params, "gts_type_id")?;
    MeterTypeId::new(raw.clone()).map_err(usage_collector_error_to_canonical)
}

/// Extract the mandatory `from` / `to` query parameters into a validated
/// [`TimeRange`].
///
/// Both are parsed as RFC 3339, which rejects an offset-less timestamp —
/// `docs/usage-collector-v1.yaml`'s `Timestamp` requires an offset, and a
/// bare local time would silently attribute usage to whatever offset the
/// server happened to assume. Absence and duplicates go through
/// [`require_single_value`], so last-wins ambiguity cannot mask a caller
/// bug, and the ordering check is [`TimeRange::new`]'s.
///
/// Runs pre-PDP, alongside [`parse_required_gts_type_id`]: a request whose
/// typed parameters do not parse has no shape to authorize.
fn parse_required_time_range(params: &[(String, String)]) -> Result<TimeRange, CanonicalError> {
    let from = parse_range_bound(params, "from")?;
    let to = parse_range_bound(params, "to")?;
    TimeRange::new(from, to).map_err(usage_collector_error_to_canonical)
}

/// Parse one RFC 3339 range bound out of `params`, blaming `key` on
/// failure so the caller learns which of the two parameters is wrong.
fn parse_range_bound(
    params: &[(String, String)],
    key: &'static str,
) -> Result<OffsetDateTime, CanonicalError> {
    let raw = require_single_value(params, key)?;
    OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339).map_err(|err| {
        UsageRecordResource::invalid_argument()
            .with_field_violation(
                key,
                format!(
                    "query parameter `{key}` must be an RFC 3339 timestamp with an \
                     offset (e.g. `1970-01-01T00:00:00Z`): {err}"
                ),
                "VALIDATION",
            )
            .create()
    })
}

/// Group `metadata.<key>=<value>` entries into a `Vec<MetadataFilter>`
/// — one filter per distinct key, with that key's full ordered value
/// list (duplicates preserved verbatim — the OR semantics within a
/// single filter make them harmless).
///
/// An empty key (`metadata.=value`) is rejected as a canonical
/// `InvalidArgument` `Problem`; a `metadata.<key>` with an empty value
/// is admitted verbatim because the SDK does not constrain
/// [`MetadataFilter`] values beyond their `String` type.
// @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-metadata-filter-parse
fn parse_metadata_filters(
    params: &[(String, String)],
) -> Result<Vec<MetadataFilter>, CanonicalError> {
    // BTreeMap so the resulting filter vector is in deterministic key
    // order regardless of query-string ordering, which keeps plugin
    // request shapes idempotent across equivalent inputs.
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, v) in params {
        let Some(key) = k.strip_prefix(METADATA_PREFIX) else {
            continue;
        };
        if key.is_empty() {
            return Err(UsageRecordResource::invalid_argument()
                .with_field_violation(
                    k,
                    format!(
                        "metadata filter key must be non-empty (`{k}=...` has no key after the `metadata.` prefix)"
                    ),
                    "VALIDATION",
                )
                .create());
        }
        groups.entry(key.to_owned()).or_default().push(v.clone());
    }
    if groups.len() > MAX_METADATA_FILTERS {
        return Err(UsageRecordResource::invalid_argument()
            .with_field_violation(
                "metadata",
                format!(
                    "{} distinct `metadata.<key>` filters exceeds cap {MAX_METADATA_FILTERS}",
                    groups.len()
                ),
                "VALIDATION",
            )
            .create());
    }
    if let Some((key, values)) = groups
        .iter()
        .find(|(_, v)| v.len() > MAX_METADATA_FILTER_VALUES)
    {
        return Err(UsageRecordResource::invalid_argument()
            .with_field_violation(
                format!("metadata.{key}"),
                format!(
                    "{} values on `metadata.{key}` exceeds cap {MAX_METADATA_FILTER_VALUES}",
                    values.len()
                ),
                "VALIDATION",
            )
            .create());
    }
    groups
        .into_iter()
        .map(|(key, values)| {
            MetadataFilter::new(key, values).map_err(usage_collector_error_to_canonical)
        })
        .collect()
}
// @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-metadata-filter-parse

/// Find the unique value for `key`, rejecting both absence and
/// duplicates with a canonical `InvalidArgument` envelope.
fn require_single_value<'a>(
    params: &'a [(String, String)],
    key: &'static str,
) -> Result<&'a String, CanonicalError> {
    let mut iter = params.iter().filter(|(k, _)| k == key);
    let Some((_, value)) = iter.next() else {
        return Err(UsageRecordResource::invalid_argument()
            .with_field_violation(
                key,
                format!("missing required query parameter `{key}`"),
                "VALIDATION",
            )
            .create());
    };
    if iter.next().is_some() {
        return Err(UsageRecordResource::invalid_argument()
            .with_field_violation(
                key,
                format!("query parameter `{key}` must appear at most once"),
                "VALIDATION",
            )
            .create());
    }
    Ok(value)
}

/// Convert one per-record submission into the identity-free domain create
/// input, lifting `gts_type_id`-, attribution-, `idempotency_key`-,
/// `reason_code`- and metadata-shape failures into per-record `Problem`
/// envelopes. The covered period is caller-supplied and forwarded verbatim;
/// it is validated — and rejected, never truncated — where it is read,
/// inside [`usage_collector_sdk::CreateUsageRecord::try_into_usage_record`],
/// which is also where the entry's `id` is derived once, authoritatively,
/// inside [`Service::create_usage_records`]. An accepted entry carries no
/// lifecycle flag to stamp: it is never rewritten
/// (`cpt-cf-usage-collector-adr-append-only-invalidation`).
///
/// This is also the fold point for the correction reference, and therefore
/// the one place on the REST path where both-or-neither is enforced. The
/// wire keeps `invalidates` and `reason_code` as two flat siblings because
/// the OAS declares them that way; the domain keeps them as one
/// [`Invalidation`], so a half-shape is unrepresentable past this line and
/// nothing downstream re-checks it.
///
/// Three boundaries carry that one rule **on a submission path**, and none
/// of them is redundant, because each is the only one standing on its own
/// path:
///
/// * in-process — the pair is one type, so the shape never exists to be
///   checked;
/// * a JSON body deserialized straight into
///   [`usage_collector_sdk::CreateUsageRecord`] — refused by the SDK's own
///   shadow struct, untyped because a `Deserialize` erases everything but a
///   message;
/// * a REST body — arrives flat through the DTO and is refused here, typed,
///   naming the missing half.
///
/// A fourth applies the same rule off the submission paths: the shadow
/// behind [`usage_collector_sdk::UsageRecord`] refuses a half-shape when a
/// persisted entry is rehydrated from a wire body. Counting it among the
/// three above is what makes the enumeration wrong, not what makes it
/// long.
#[allow(clippy::result_large_err)]
fn record_request_into_domain(req: CreateUsageRecordRequest) -> Result<CreateUsageRecord, Problem> {
    let gts_type_id = MeterTypeId::new(req.gts_type_id)
        .map_err(|err| Problem::from(usage_collector_error_to_canonical(err)))?;

    let resource_ref = ResourceRef::try_from(req.resource_ref)
        .map_err(|err| Problem::from(usage_collector_error_to_canonical(err)))?;

    let subject_ref = req
        .subject_ref
        .map(SubjectRef::try_from)
        .transpose()
        .map_err(|err| Problem::from(usage_collector_error_to_canonical(err)))?;

    let idempotency_key = IdempotencyKey::new(req.idempotency_key)
        .map_err(|err| Problem::from(usage_collector_error_to_canonical(err)))?;

    let metadata = metadata_from_wire(req.metadata)
        .map_err(|err| Problem::from(usage_collector_error_to_canonical(err)))?;

    let invalidation = match (req.invalidates, req.reason_code) {
        (Some(target), Some(reason)) => {
            let reason = ReasonCode::new(reason)
                .map_err(|err| Problem::from(usage_collector_error_to_canonical(err)))?;
            Some(Invalidation { target, reason })
        }
        (None, None) => None,
        // `field` names the half the caller has to add, not the half they
        // sent — a rejection naming what is already there is not
        // actionable.
        (Some(_), None) => {
            return Err(Problem::from(usage_collector_error_to_canonical(
                UsageCollectorError::invalidation_reference_incomplete("reason_code"),
            )));
        }
        (None, Some(_)) => {
            return Err(Problem::from(usage_collector_error_to_canonical(
                UsageCollectorError::invalidation_reference_incomplete("invalidates"),
            )));
        }
    };

    Ok(CreateUsageRecord {
        gts_type_id,
        tenant_id: req.tenant_id,
        resource_ref,
        subject_ref,
        metadata,
        value: req.value,
        idempotency_key,
        invalidation,
        window_start: req.window_start,
        window_end: req.window_end,
    })
}

/// Convert the typed wire `BTreeMap<String, String>` into the SDK's
/// validating [`BTreeMap<MetadataKey, String>`]. Structural shape errors
/// (non-object, non-string value, etc.) are already rejected at axum's
/// JSON boundary by the DTO type; only per-key validation remains here.
///
/// Closed-shape membership against the resolved meter declaration's
/// `metadata_fields` and the configurable size cap remain a service-layer check
/// (`validate_submit_record_metadata`) that runs after this conversion.
fn metadata_from_wire(
    raw: BTreeMap<String, String>,
) -> Result<BTreeMap<MetadataKey, String>, UsageCollectorError> {
    let mut out = BTreeMap::new();
    for (k, v) in raw {
        let key = MetadataKey::new(k)?;
        out.insert(key, v);
    }
    Ok(out)
}

/// Lift one per-record service outcome into the wire-shaped envelope.
// @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization:p1:inst-algo-attrib-return
fn per_record_outcome(
    index: usize,
    outcome: Result<UsageRecord, UsageCollectorError>,
) -> CreateUsageRecordResultDto {
    match outcome {
        Ok(record) => CreateUsageRecordResultDto::Accepted {
            index,
            record: UsageRecordDto::from(record),
        },
        Err(err) => CreateUsageRecordResultDto::Rejected {
            index,
            error: usage_record_error_to_problem(err),
        },
    }
}
// @cpt-end:cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization:p1:inst-algo-attrib-return

/// Parse the URL path `{uuid}` segment the GET single-record handler
/// takes. A malformed input surfaces as the canonical `InvalidArgument`
/// `Problem` with a field violation on `id`; this error shape is
/// host-private (it cannot originate inside the transport-agnostic SDK).
///
fn parse_record_id(uuid_raw: &str) -> Result<Uuid, CanonicalError> {
    Uuid::parse_str(uuid_raw).map_err(|_| {
        UsageRecordResource::invalid_argument()
            .with_field_violation(
                "id",
                format!("usage record id `{uuid_raw}` is not a valid UUID"),
                "VALIDATION",
            )
            .create()
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "usage_records_tests.rs"]
mod usage_records_tests;
