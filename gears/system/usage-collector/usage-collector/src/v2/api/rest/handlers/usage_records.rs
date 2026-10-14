//! REST handlers for the foundation `/usage-collector/v2/records`
//! create + read surface, and for the `/records/backfill` bulk-import
//! route that shares the create route's whole handler body. Each handler
//! is a thin pass-through: it pulls the gateway-resolved
//! `SecurityContext`, dispatches to the domain [`Service`], and lifts
//! `UsageCollectorError` through the host-owned canonical mapping. PDP
//! authorization runs inside the `Service` method each handler dispatches
//! to.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Extension, Path, Query};
use axum::response::Response;
use time::OffsetDateTime;
use toolkit::api::canonical_prelude::*;
use toolkit_canonical_errors::Problem;
use toolkit_odata::{ODataQuery, Page as ODataPage};
use toolkit_security::SecurityContext;
use usage_collector_sdk::v2::{
    AggregationDimension, CreateUsageRecord, EntryType, IdempotencyKey, MetadataFilter,
    MetadataKey, MeterTypeId, ReasonCode, ResourceRef, SubjectRef, TimeRange, UsageCollectorError,
    UsageQuantity, UsageRecord,
};
use uuid::Uuid;

use crate::v2::api::rest::dto::{
    AggregationRequest, AggregationResultDto, CreateUsageRecordRequest, CreateUsageRecordResultDto,
    CreateUsageRecordsRequest, CreateUsageRecordsResponse, UsageRecordDto,
};
use crate::v2::domain::Service;
use crate::v2::domain::list_query::establish_keyset_order;
use crate::v2::domain::ports::metrics::PdpOp;
use crate::v2::infra::sdk_error_mapping::{
    UsageRecordResource,
    usage_collector_error_to_canonical_for_usage_record as usage_collector_error_to_canonical,
    usage_record_error_to_problem,
};

/// `POST /usage-collector/v2/records`
///
/// Batch-create one or more usage records. Per-record validation /
/// authorization / dispatch failures surface as `Rejected` entries inside
/// the response envelope at their input index; the response status is
/// `200 OK` when every record was accepted and `207 Multi-Status` when
/// at least one record was rejected.
///
/// Whole-request failures (handle resolution, batch SPI dispatch) still
/// short-circuit through the canonical `Problem` envelope.
// @cpt-flow:cpt-cf-usage-collector-flow-emit-usage-record:p1
// @cpt-dod:cpt-cf-usage-collector-entity-model:p1
// @cpt-dod:cpt-cf-usage-collector-principle-fail-closed:p2
pub async fn handle_create_usage_records(
    // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-submit
    // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-missing-ctx
    // @cpt-begin:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-algo-attrib-receive-ctx
    Extension(ctx): Extension<SecurityContext>,
    // @cpt-end:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-algo-attrib-receive-ctx
    // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-missing-ctx
    Extension(service): Extension<Arc<Service>>,
    Json(req): Json<CreateUsageRecordsRequest>,
    // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-submit
) -> ApiResult<impl IntoResponse> {
    // `create_usage_records` is the whole difference between this handler and
    // `handle_backfill_usage_records`, and what makes the gateway stamp
    // `origin = live`.
    dispatch_usage_record_batch(
        &ctx,
        req,
        &service,
        PdpOp::Ingest,
        async |ctx, batch, submitted_before_decode| {
            service
                .create_usage_records_with_submitted_count(ctx, batch, submitted_before_decode)
                .await
        },
    )
    .await
}

/// `POST /usage-collector/v2/records/backfill`
///
/// Bulk historical import — same per-entry envelope and `200`/`207`
/// selection as [`handle_create_usage_records`], dispatching
/// [`Service::backfill_usage_records`] instead: `origin = backfill`, a
/// lifted past bound on the covered period, elevated authorization past the
/// backfill window.
///
/// TODO: the ADR's workload isolation is not implemented — see
/// [`Service::backfill_usage_records`].
pub async fn handle_backfill_usage_records(
    // @cpt-begin:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-algo-attrib-receive-ctx
    Extension(ctx): Extension<SecurityContext>,
    // @cpt-end:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-algo-attrib-receive-ctx
    Extension(service): Extension<Arc<Service>>,
    Json(req): Json<CreateUsageRecordsRequest>,
) -> ApiResult<impl IntoResponse> {
    dispatch_usage_record_batch(
        &ctx,
        req,
        &service,
        PdpOp::Backfill,
        async |ctx, batch, submitted_before_decode| {
            service
                .backfill_usage_records_with_submitted_count(ctx, batch, submitted_before_decode)
                .await
        },
    )
    .await
}

/// The batch-ingestion handler body, shared by `POST /records` and
/// `POST /records/backfill`. `dispatch` is the only variation: which
/// [`Service`] batch entry point eligible records go to, and therefore which
/// `RecordOrigin` the gateway stamps. The structural cap, the wire-to-domain
/// fold, the index-preserving dispatch, the re-sort and the `200`/`207`
/// selection live here once so the two routes cannot disagree about which
/// input a rejection belongs to.
// @cpt-begin:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-algo-attrib-batch-precondition
async fn dispatch_usage_record_batch(
    ctx: &SecurityContext,
    req: CreateUsageRecordsRequest,
    service: &Service,
    op: PdpOp,
    dispatch: impl AsyncFnOnce(
        &SecurityContext,
        Vec<CreateUsageRecord>,
        usize,
    ) -> Result<
        Vec<Result<UsageRecord, UsageCollectorError>>,
        UsageCollectorError,
    >,
) -> ApiResult<Response> {
    // Cap first, before any entry decodes (DESIGN §3.2): an empty or over-cap
    // submission is rejected whole, with no quota charge. Both cap arms route
    // through `record_structural_cap_rejection` so the metric/log pair can't
    // diverge; `op` names the route for the log only, not the counter.
    let max_batch_records = service.max_batch_records();
    let actual = req.records.len();
    if actual == 0 || actual > max_batch_records {
        service.record_structural_cap_rejection(ctx, op);
        return Err(usage_collector_error_to_canonical(
            UsageCollectorError::invalid_batch_size(actual, 1, max_batch_records),
        ));
    }

    let mut indexed_results: Vec<(usize, CreateUsageRecordResultDto)> =
        Vec::with_capacity(req.records.len());
    let mut eligible: Vec<(usize, CreateUsageRecord)> = Vec::new();

    for (index, raw) in req.records.into_iter().enumerate() {
        let decoded = decode_record_entry(raw).and_then(record_request_into_domain);
        match decoded {
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
    // @cpt-end:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-algo-attrib-batch-precondition

    // Dispatched unconditionally, including when every entry failed to decode
    // and `eligible` is empty — the service charges the ingestion quota, so a
    // submission that never reaches it never pays. `actual`, not
    // `batch.len()`, is threaded down as the quota-charged count, so a batch
    // with pre-decode rejections isn't billed as if it were smaller.
    let (indices, batch): (Vec<usize>, Vec<CreateUsageRecord>) = eligible.into_iter().unzip();

    // A batch-level dispatch failure (plugin resolution, SPI size mismatch,
    // exhausted quota) short-circuits as a whole-request envelope — it would
    // have hit every record identically.
    let per_record = match dispatch(ctx, batch, actual).await {
        Ok(per_record) => per_record,
        Err(err) => return lift_whole_request_ingestion_error(err),
    };
    for (index, outcome) in indices.into_iter().zip(per_record) {
        indexed_results.push((index, per_record_outcome(index, outcome)));
    }

    // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-return
    indexed_results.sort_by_key(|(idx, _)| *idx);
    let results: Vec<CreateUsageRecordResultDto> =
        indexed_results.into_iter().map(|(_, item)| item).collect();
    // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-return

    let any_rejected = results
        .iter()
        .any(|item| matches!(item, CreateUsageRecordResultDto::Rejected { .. }));

    // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-return-200
    // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-return-207
    let status = if any_rejected {
        StatusCode::MULTI_STATUS
    } else {
        StatusCode::OK
    };

    Ok((status, Json(CreateUsageRecordsResponse { results })).into_response())
    // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-return-207
    // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-return-200
}

/// Lift a whole-request ingestion failure, adding the one header this gear
/// sets by hand: `Retry-After` on a 429, since `toolkit-canonical-errors`
/// only derives it for `ServiceUnavailable` (DESIGN §3.2). Sourced from the
/// same `retry_after_seconds` as the body's `violations[0]`, off one
/// `UsageCollectorError`, so the header and body can't drift apart.
// @cpt-dod:cpt-cf-usage-collector-dod-quota-whole-rejection:p2
fn lift_whole_request_ingestion_error(err: UsageCollectorError) -> ApiResult<Response> {
    let retry_after_seconds = match &err {
        UsageCollectorError::ResourceExhausted {
            retry_after_seconds,
            ..
        } => *retry_after_seconds,
        _ => return Err(usage_collector_error_to_canonical(err)),
    };
    let mut response = usage_collector_error_to_canonical(err).into_response();
    response.headers_mut().insert(
        axum::http::header::RETRY_AFTER,
        axum::http::HeaderValue::from(retry_after_seconds),
    );
    Ok(response)
}

/// `GET /usage-collector/v2/records/{id}`
///
/// Read a single usage record by `uuid`. A malformed `uuid` path segment
/// surfaces as the canonical `InvalidArgument` problem; a missing record
/// surfaces as the canonical `NotFound` problem; a PDP denial surfaces as
/// `Forbidden`; a Plugin SPI transport / readiness / persistence fault
/// surfaces as `ServiceUnavailable`. On success the response is HTTP 200
/// with the wire-projected [`UsageRecordDto`] body.
// @cpt-flow:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1
pub async fn handle_get_usage_record(
    // @cpt-begin:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-get-record-missing-ctx
    Extension(ctx): Extension<SecurityContext>,
    // @cpt-end:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-get-record-missing-ctx
    Extension(service): Extension<Arc<Service>>,
    Path(uuid_raw): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = parse_record_id(&uuid_raw)?;
    // @cpt-begin:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-get-record-spi-fail
    let record = service
        .get_usage_record(&ctx, id)
        .await
        .map_err(usage_collector_error_to_canonical)?;
    // @cpt-end:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-get-record-spi-fail
    // @cpt-begin:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-point-return
    Ok((StatusCode::OK, Json(UsageRecordDto::from(record))))
    // @cpt-end:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-point-return
}

/// `GET /usage-collector/v2/records`
///
/// Keyset-paginated raw read over the persisted usage records.
///
/// `gts_type_id`, `from` and `to` are mandatory, typed, non-OData query
/// parameters; selection reads the period end alone
/// ([`TimeRange::contains_window_end`]). The range never travels in
/// `$filter` — see [`crate::v2::domain::query`] for that gate and for the
/// cursor lifecycle a present `cursor` goes through.
///
/// Gateway-side guards applied before the service is invoked: `$top` is
/// capped at [`MAX_PAGE_SIZE`] (never silently clamped); an `$orderby` is
/// refused or completed into a keyset by [`establish_keyset_order`]
/// (skipped when a cursor is present, since the order then comes from the
/// token instead).
///
/// Per-key metadata filtering is the typed side-channel [`MetadataFilter`],
/// wire-encoded as repeated `metadata.<key>=<value>` query parameters
/// (OR within a key, AND across keys) — `toolkit-odata` has no surface for
/// dynamic JSON-map keys.
///
/// PDP authorization, PDP-constraint composition and the plugin SPI
/// dispatch all happen inside [`Service::list_usage_records`].
// @cpt-flow:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1
// @cpt-dod:cpt-cf-usage-collector-fr-query-raw:p1
// @cpt-dod:cpt-cf-usage-collector-constraint-nfr-thresholds:p1
// @cpt-dod:cpt-cf-usage-collector-dod-usage-query-cursor-v1-toolkit-adoption:p1
pub async fn handle_list_usage_records(
    // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-missing-ctx
    Extension(ctx): Extension<SecurityContext>,
    // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-missing-ctx
    Extension(service): Extension<Arc<Service>>,
    Query(params): Query<Vec<(String, String)>>,
    OData(query): OData,
) -> ApiResult<Json<ODataPage<UsageRecordDto>>> {
    // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-submit
    let PreparedListRequest {
        gts_type_id,
        time_range,
        metadata_filter,
        query,
    } = prepare_list_request(&params, query)?;
    // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-submit

    let page = service
        .list_usage_records(&ctx, gts_type_id, time_range, &query, &metadata_filter)
        .await
        .map_err(usage_collector_error_to_canonical)?;

    // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-return
    Ok(Json(page.map_items(UsageRecordDto::from)))
    // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-return
}

/// `POST /usage-collector/v2/records/aggregate`
///
/// Server-side aggregated read over the persisted usage records.
/// `gts_type_id` and `$filter` are query parameters as on the raw path; the
/// JSON body carries `time_range` and `group_by` only — there is no
/// aggregation-kind parameter, since the fold is resolved from the queried
/// type's declaration.
///
/// `from`/`to`, `$orderby`, `$top`/`limit` and `cursor` are NOT accepted
/// here: the range lives in the body, and the result is not paginated.
///
/// PDP authorization, declaration resolution, PDP-constraint composition and
/// the plugin SPI dispatch all happen inside
/// [`Service::query_aggregated_usage_records`].
// @cpt-flow:cpt-cf-usage-collector-flow-query-aggregated-usage:p1
// @cpt-dod:cpt-cf-usage-collector-fr-query-aggregation:p1
pub async fn handle_query_aggregated_usage_records(
    // @cpt-begin:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-missing-ctx
    Extension(ctx): Extension<SecurityContext>,
    // @cpt-end:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-missing-ctx
    Extension(service): Extension<Arc<Service>>,
    Query(params): Query<Vec<(String, String)>>,
    OData(query): OData,
    Json(req): Json<AggregationRequest>,
) -> ApiResult<Json<AggregationResultDto>> {
    // @cpt-begin:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-agg-submit
    let PreparedAggregateRequest {
        gts_type_id,
        time_range,
        metadata_filter,
        query,
        group_by,
    } = prepare_aggregate_request(&params, query, req)?;
    // @cpt-end:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-agg-submit

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

    // @cpt-begin:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-agg-return
    Ok(Json(AggregationResultDto::from(result)))
    // @cpt-end:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-agg-return
}

/// Everything the aggregate path's pre-service validators produce. A struct,
/// not a tuple, so a future same-typed field can't be silently transposed —
/// most importantly `time_range`, where a crossed parameter is a
/// wrong-window scan that still answers `200`.
struct PreparedAggregateRequest {
    gts_type_id: MeterTypeId,
    time_range: TimeRange,
    metadata_filter: Vec<MetadataFilter>,
    query: ODataQuery,
    group_by: Vec<AggregationDimension>,
}

/// Bundle of every aggregate-path pre-service validator: parameter
/// allowlist, typed `gts_type_id`, metadata filters, and the body-shape
/// projection into [`TimeRange`]/[`AggregationDimension`]s. Propagates the
/// canonical envelope verbatim on the first failing validator.
fn prepare_aggregate_request(
    params: &[(String, String)],
    query: ODataQuery,
    req: AggregationRequest,
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
/// declared aggregate-OData set, `gts_type_id`, or a `metadata.<key>` entry
/// — silent drop of unrecognised parameters is a documented contract-drift
/// surface. Checks [`TYPED_AGGREGATE_PARAMS`], not [`TYPED_LIST_PARAMS`], so
/// `from`/`to` 400 here rather than being accepted and ignored.
// @cpt-algo:cpt-cf-usage-collector-algo-query-request-admission:p1
// @cpt-dod:cpt-cf-usage-collector-dod-mandatory-type-and-range:p1
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

/// Maximum page size the gateway will request from the plugin
/// (`cpt-cf-usage-collector-constraint-nfr-thresholds`). A caller-supplied
/// `$top`/`limit` above this is rejected with 400, never silently clamped.
/// Domain-owned (`domain::service::MAX_PAGE_SIZE`) so
/// `Service::list_usage_records` enforces the same ceiling for the
/// in-process caller this REST guard doesn't run in front of.
#[allow(
    clippy::redundant_pub_crate,
    reason = "plain `pub` fails E0364: the re-exported item is `pub(crate)` at its \
              definition in domain::service, and a `use` re-export may not exceed the \
              visibility of what it imports, even though `usage_records` is itself a \
              private module where `pub` and `pub(crate)` would otherwise be equivalent"
)]
pub(crate) use crate::v2::domain::service::MAX_PAGE_SIZE;

/// `$`-prefixed `OData` parameters, plus the toolkit's non-`OData` aliases
/// (`$top`/`limit` fold onto one `ODataQuery.limit` slot; sending both is a
/// duplicate-field rejection upstream of this allowlist).
///
/// `$select` is deliberately absent: this gear never applies the
/// projection, so admitting it would silently return every field under a
/// `200` that reads as the requested projection.
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

/// Apply gateway-side guards on the parsed [`ODataQuery`]: reject `$top`
/// over [`MAX_PAGE_SIZE`] rather than clamp; run the domain's keyset floor
/// [`establish_keyset_order`] on the caller's `$orderby` so a bad order is
/// blamed here, before authorization or plugin work (idempotent — the
/// service re-running it before dispatch is a no-op); decode a present
/// cursor's signed tokens and materialize their order onto the query,
/// mirroring the domain's own binding.
// @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-odata-parse
// @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-cursor-in
fn prepare_list_query(mut query: ODataQuery) -> Result<ODataQuery, CanonicalError> {
    // $top cap: violation names `$top` (the parsed query doesn't record
    // which spelling arrived) but the detail mentions `limit` too.
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

    // $orderby admissibility + canonical keyset fields (domain's first-page
    // floor). Skipped with a cursor present: the extractor rejects
    // `$orderby`+`cursor` together and leaves `order` empty, so there's
    // nothing to floor yet — step 3 reconstructs it from the token instead.
    if query.cursor.is_none() {
        establish_keyset_order(&mut query).map_err(usage_collector_error_to_canonical)?;
    }

    // Cursor validation + order materialization: the effective keyset order
    // lives in the cursor's signed-token payload (`cursor.s`), not
    // `query.order`, so write it back — the plugin reads that slot for both
    // `ORDER BY` and the continuation predicate.
    if let Some(cursor) = query.cursor.as_ref() {
        // Decoding only — the fingerprint/order comparisons that actually
        // bind a continuation to its query live behind the service
        // (`read_fingerprint`, `bind_continuation_order`), not at this edge.
        query.order = toolkit_odata::ODataOrderBy::from_signed_tokens(&cursor.s)
            .map_err(CanonicalError::from)?;
    }

    Ok(query)
}
// @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-cursor-in
// @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-odata-parse

/// Reject any query parameter that is not a declared `OData` parameter, a
/// typed list parameter, or a `metadata.<key>` entry — silent drop of
/// unrecognised parameters is a documented contract-drift surface.
/// [`TYPED_LIST_PARAMS`] admits no numeric row offset, so `offset`/`skip`
/// is refused here as unrecognised.
// @cpt-algo:cpt-cf-usage-collector-algo-query-request-admission:p1
// @cpt-dod:cpt-cf-usage-collector-dod-mandatory-type-and-range:p1
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
/// through [`MeterTypeId::new`]. Missing surfaces as `InvalidArgument` on
/// `gts_type_id`; a duplicate occurrence is rejected rather than taking
/// last-wins.
// @cpt-algo:cpt-cf-usage-collector-algo-query-request-admission:p1
// @cpt-dod:cpt-cf-usage-collector-dod-mandatory-type-and-range:p1
fn parse_required_gts_type_id(params: &[(String, String)]) -> Result<MeterTypeId, CanonicalError> {
    let raw = require_single_value(params, "gts_type_id")?;
    MeterTypeId::new(raw.clone()).map_err(usage_collector_error_to_canonical)
}

/// Extract the mandatory `from`/`to` query parameters into a validated
/// [`TimeRange`]. Parsed as RFC 3339, which rejects an offset-less
/// timestamp (`docs/usage-collector-v2.yaml`'s `Timestamp` requires one —
/// a bare local time would silently attribute usage to the server's
/// assumed offset). Runs pre-PDP: a request whose typed parameters don't
/// parse has no shape to authorize.
// @cpt-algo:cpt-cf-usage-collector-algo-query-request-admission:p1
// @cpt-dod:cpt-cf-usage-collector-dod-mandatory-type-and-range:p1
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

/// Group `metadata.<key>=<value>` entries into a `Vec<MetadataFilter>` —
/// one filter per distinct key, OR-ing its (possibly duplicated) values.
/// An empty key is rejected; an empty value is admitted verbatim. Caps
/// nothing — the metadata caps live in
/// [`crate::v2::domain::query::require_metadata_filter_within_caps`], the one
/// point every surface passes through.
// @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-metadata-filter-parse
fn parse_metadata_filters(
    params: &[(String, String)],
) -> Result<Vec<MetadataFilter>, CanonicalError> {
    // BTreeMap so the filter vector is in deterministic key order regardless
    // of query-string ordering, keeping plugin request shapes idempotent
    // across equivalent inputs.
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
    groups
        .into_iter()
        .map(|(key, values)| {
            MetadataFilter::new(key, values).map_err(usage_collector_error_to_canonical)
        })
        .collect()
}
// @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-metadata-filter-parse

/// Every occurrence of `key` in `params`, values only, in wire order.
///
/// Shared by [`require_single_value`] and [`single_param`], which differ only
/// in what zero occurrences means.
fn param_occurrences<'a>(
    params: &'a [(String, String)],
    key: &'static str,
) -> impl Iterator<Item = &'a String> {
    params.iter().filter(move |(k, _)| k == key).map(|(_, v)| v)
}

/// The canonical `InvalidArgument` for a query parameter sent more than
/// once. Shared by [`require_single_value`] and [`single_param`]: a
/// mandatory and an optional parameter refuse a duplicate on identical terms.
fn duplicate_param_violation(key: &'static str) -> CanonicalError {
    UsageRecordResource::invalid_argument()
        .with_field_violation(
            key,
            format!("query parameter `{key}` must appear at most once"),
            "VALIDATION",
        )
        .create()
}

/// Find the unique value for `key`, rejecting both absence and
/// duplicates with a canonical `InvalidArgument` envelope.
fn require_single_value<'a>(
    params: &'a [(String, String)],
    key: &'static str,
) -> Result<&'a String, CanonicalError> {
    let mut iter = param_occurrences(params, key);
    let Some(value) = iter.next() else {
        return Err(UsageRecordResource::invalid_argument()
            .with_field_violation(
                key,
                format!("missing required query parameter `{key}`"),
                "VALIDATION",
            )
            .create());
    };
    if iter.next().is_some() {
        return Err(duplicate_param_violation(key));
    }
    Ok(value)
}

/// Read at most one value for `key`; `None` when it is absent, but a
/// repeated occurrence is still an error — unlike [`require_single_value`],
/// absence alone is not. Used by the feed's optional `limit`/`cursor`/`until`.
pub(super) fn single_param<'a>(
    params: &'a [(String, String)],
    key: &'static str,
) -> Result<Option<&'a str>, CanonicalError> {
    let mut iter = param_occurrences(params, key);
    let Some(value) = iter.next() else {
        return Ok(None);
    };
    if iter.next().is_some() {
        return Err(duplicate_param_violation(key));
    }
    Ok(Some(value.as_str()))
}

/// Decodes one raw batch entry into the request DTO, or the `Problem` a
/// single-entry submission with the same body would receive.
///
/// The violation names the offending property when serde reports one
/// (unknown or missing field) and `records` otherwise.
#[allow(
    clippy::result_large_err,
    reason = "the large Err here is toolkit_canonical_errors::Problem (208 bytes), not \
              UsageCollectorError; it is this crate's wire-error type and boxing it is \
              the toolkit's call to make, not this handler's"
)]
fn decode_record_entry(raw: serde_json::Value) -> Result<CreateUsageRecordRequest, Problem> {
    if let Some(problem) = explicit_null_idempotency_key(&raw) {
        return Err(problem);
    }
    serde_json::from_value::<CreateUsageRecordRequest>(raw).map_err(|err| {
        let message = err.to_string();
        let field = serde_field_name(&message).unwrap_or("records");
        Problem::from(
            UsageRecordResource::invalid_argument()
                .with_field_violation(field, message.as_str(), "VALIDATION")
                .create(),
        )
    })
}

/// An explicit `"idempotency_key": null` is refused as a missing key. Needed
/// because the DTO types the property `String`, so `null` fails to decode as
/// an untraceable `invalid type` error that `serde_field_name` can't attribute
/// to a property; running pre-decode keeps the violation on
/// `idempotency_key`. One answer for both entry kinds — a withdrawal's key
/// locates its target (DESIGN §3.1), so it's required there too.
fn explicit_null_idempotency_key(raw: &serde_json::Value) -> Option<Problem> {
    if !raw
        .get("idempotency_key")
        .is_some_and(serde_json::Value::is_null)
    {
        return None;
    }
    Some(Problem::from(usage_collector_error_to_canonical(
        UsageCollectorError::missing_idempotency_key(),
    )))
}

/// The property a serde error names: the backticked token after
/// "unknown field " or "missing field ".
fn serde_field_name(message: &str) -> Option<&str> {
    ["unknown field `", "missing field `"]
        .iter()
        .find_map(|marker| message.split_once(marker))
        .and_then(|(_, rest)| rest.split_once('`'))
        .map(|(field, _)| field)
}

/// Parse the wire `entry_type` into the SDK's closed [`EntryType`], through
/// its own `Deserialize` (`#[serde(rename_all = "lowercase")]`) rather than
/// matched literals, so the accepted vocabulary has one definition and
/// serde's own message carries the rejected spelling and the expected set.
/// Never inferred from `quantity`'s value or sign.
// @cpt-dod:cpt-cf-usage-collector-dod-explicit-entry-type:p1
#[allow(
    clippy::result_large_err,
    reason = "the large Err here is toolkit_canonical_errors::Problem (208 bytes), not \
              UsageCollectorError; it is this crate's wire-error type and boxing it is \
              the toolkit's call to make, not this handler's"
)]
fn entry_type_from_wire(raw: &str) -> Result<EntryType, Problem> {
    serde_json::from_value::<EntryType>(serde_json::Value::String(raw.to_owned())).map_err(|err| {
        Problem::from(
            UsageRecordResource::invalid_argument()
                .with_field_violation("entry_type", err.to_string(), "VALIDATION")
                .create(),
        )
    })
}

/// Convert one per-record submission into the identity-free domain create
/// input. Per-field validation failures (`entry_type`, `gts_type_id`,
/// attribution, `idempotency_key`, `reason_code`, metadata, quantity) become
/// per-record `Problem`s rather than failing the whole batch; the covered
/// period is forwarded verbatim and the record `id` is derived later, inside
/// the SDK projection
/// ([`usage_collector_sdk::v2::CreateUsageRecord::try_into_usage_record`] /
/// `try_into_invalidation_record`), which is also where `entry_type`/
/// `reason_code` agreement is checked.
///
/// `tenant_id` carries no non-nil check here — unlike `resource_ref`/
/// `subject_ref`, which are constructor-guarded on every caller — relying on
/// the PDP decision instead, since that check belongs inside the owning
/// domain trait rather than a REST handler. `quantity` IS fully guarded here:
/// [`UsageQuantity::parse`] is the only public constructor, so an
/// out-of-range, non-finite or negative-zero value rejects its own entry on
/// every caller.
// @cpt-algo:cpt-cf-usage-collector-algo-quantity-validation:p1
// @cpt-dod:cpt-cf-usage-collector-dod-quantity-contract:p1
#[allow(
    clippy::result_large_err,
    reason = "the large Err here is toolkit_canonical_errors::Problem (208 bytes), not \
              UsageCollectorError; it is this crate's wire-error type and boxing it is \
              the toolkit's call to make, not this handler's"
)]
fn record_request_into_domain(req: CreateUsageRecordRequest) -> Result<CreateUsageRecord, Problem> {
    let entry_type = entry_type_from_wire(&req.entry_type)?;

    let gts_type_id = MeterTypeId::new(req.gts_type_id)
        .map_err(|err| Problem::from(usage_collector_error_to_canonical(err)))?;

    let resource_ref = ResourceRef::try_from(req.resource_ref)
        .map_err(|err| Problem::from(usage_collector_error_to_canonical(err)))?;

    let subject_ref = req
        .subject_ref
        .map(SubjectRef::try_from)
        .transpose()
        .map_err(|err| Problem::from(usage_collector_error_to_canonical(err)))?;

    let quantity = UsageQuantity::parse(&req.quantity)
        .map_err(|err| Problem::from(usage_collector_error_to_canonical(err)))?;

    let idempotency_key = IdempotencyKey::new(req.idempotency_key)
        .map_err(|err| Problem::from(usage_collector_error_to_canonical(err)))?;

    let metadata = metadata_from_wire(req.metadata)
        .map_err(|err| Problem::from(usage_collector_error_to_canonical(err)))?;

    // Carried across as the caller stated it, agreement with `entry_type`
    // left to the projection. A `reason_code` on a `record` is refused
    // there rather than dropped here: dropping it would admit a
    // self-contradicting submission as an ordinary measurement.
    let invalidation = req
        .reason_code
        .map(ReasonCode::new)
        .transpose()
        .map_err(|err| Problem::from(usage_collector_error_to_canonical(err)))?;

    Ok(CreateUsageRecord {
        entry_type,
        gts_type_id,
        tenant_id: req.tenant_id,
        resource_ref,
        subject_ref,
        metadata,
        quantity,
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
/// `metadata_fields` and the size cap remain a service-layer check
/// (`validate_submit_record_metadata`) run after this conversion.
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
// @cpt-begin:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-algo-attrib-return
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
// @cpt-end:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-algo-attrib-return

/// Parse the URL path `{uuid}` segment the GET single-record handler
/// takes. A malformed input surfaces as the canonical `InvalidArgument`
/// `Problem` with a field violation on `id`; this error shape is
/// host-private (it cannot originate inside the transport-agnostic SDK).
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
