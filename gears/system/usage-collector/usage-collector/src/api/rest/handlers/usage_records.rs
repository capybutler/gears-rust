//! REST handlers for the foundation `/usage-collector/v1/records`
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
use usage_collector_sdk::{
    AggregationDimension, CreateUsageRecord, EntryType, IdempotencyKey, MetadataFilter,
    MetadataKey, MeterTypeId, ReasonCode, ResourceRef, SubjectRef, TimeRange, UsageCollectorError,
    UsageQuantity, UsageRecord,
};
use uuid::Uuid;

use crate::api::rest::dto::{
    AggregationRequest, AggregationResultDto, CreateUsageRecordRequest, CreateUsageRecordResultDto,
    CreateUsageRecordsRequest, CreateUsageRecordsResponse, UsageRecordDto,
};
use crate::domain::Service;
use crate::domain::ports::metrics::PdpOp;
use crate::domain::query::establish_keyset_order;
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
// @cpt-flow:cpt-cf-usage-collector-flow-emit-usage-record:p1
// @cpt-dod:cpt-cf-usage-collector-entity-model:p1
// @cpt-dod:cpt-cf-usage-collector-principle-fail-closed:p2
pub async fn handle_create_usage_records(
    // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-submit
    // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-missing-ctx
    // The instruction names two sites: the `Extension<SecurityContext>` the
    // gateway middleware supplies on REST, marked here, and the fold of each
    // submission into its attribution tuple, marked on the shared body below.
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
    // `origin = live`. Everything around it is the shared body, because a
    // second copy of the per-index bookkeeping is how the two routes would
    // silently disagree about which input a rejection belongs to.
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

/// `POST /usage-collector/v1/records/backfill`
///
/// Bulk historical import. Same request shape, same per-entry response
/// envelope and same `200` / `207` selection as
/// [`handle_create_usage_records`] — the dispatched
/// [`Service::backfill_usage_records`] is the only difference, and with it
/// the `origin = backfill` marker the gateway stamps, the lifted past
/// bound on the covered period, and the elevated authorization an entry
/// older than the configured backfill window is judged against.
///
/// The ADR's *workload* isolation is not implemented — see
/// [`Service::backfill_usage_records`], which carries that TODO. Nothing on
/// this route bounds it apart from the live one, so the registered description
/// names fewer differences from `POST /records` than the published contract
/// enumerates.
///
/// The only `@cpt` marker here is the REST half of
/// `inst-algo-attrib-receive-ctx`, marked on every route taking an
/// `Extension<SecurityContext>`. The batch flow's own instructions stay on
/// [`handle_create_usage_records`]'s extractors and in the shared body; this
/// route's own obligation is the unimplemented workload isolation, which no
/// marker may claim.
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
/// `POST /records/backfill`.
///
/// `dispatch` is the only variation: which [`Service`] batch entry point the
/// eligible records go to, and therefore which
/// [`usage_collector_sdk::RecordOrigin`] the gateway stamps them with. The
/// structural batch cap, the fold of each wire record into its domain type, the
/// index-preserving dispatch, the re-sort and the `200` / `207` selection are
/// identical on both routes and live here once — mirroring
/// `Service::create_usage_records_for_origin` on the domain side. A rejection
/// carries the input index it belongs to, so a divergence between two copies of
/// that bookkeeping would misattribute rejections rather than fail loudly.
///
/// `service` is taken whole rather than as a bare cap: this body needs both the
/// cap value and the metrics point the cap rejection records.
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
    // Cap first, before any entry is decoded (DESIGN §3.2): an empty or over-cap submission is rejected whole.
    let max_batch_records = service.max_batch_records();
    let actual = req.records.len();
    if actual == 0 || actual > max_batch_records {
        // This arm returns before the service is entered, so without the call
        // a REST over-cap flood would move nothing on
        // `uc_ingestion_requests_total`. Both cap arms go through
        // `Service::record_structural_cap_rejection`, so they record one point
        // — and one structured log entry (`inst-log-one-per-operation`) — and
        // cannot diverge. `op` names the route for the log's `operation` field;
        // the counter itself carries no such label.
        //
        // Counting is telemetry, not the admission gate: the authoritative cap
        // lives in the domain service (DESIGN §3.2), and this edge copy exists
        // so an over-cap submission does not pay for a full decode pass first.
        // Nothing is charged against the ingestion quota here — §3.2 orders the
        // cap ahead of the charge.
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

    // Dispatched unconditionally, **including when every entry failed to
    // decode and `eligible` is empty.** The service is where the ingestion
    // quota is charged (DESIGN §3.2: "a handler-side check would miss every
    // in-process caller"), so a submission that never reaches it never pays —
    // and passing `batch.len()` instead would make a batch with one good entry
    // cost one token. Both are the escape §3.2 forbids in terms. `actual`, the
    // count this handler already capped on, is therefore threaded down and is
    // what the cap and the quota are both judged against; dropping it in favour
    // of `batch.len()` would also break the agreement between
    // `uc_ingestion_requests_total{outcome="partial"}` and the 207 below.
    //
    // The service answers an empty dispatch with an empty result set after
    // charging, so the zip below contributes nothing and the response is
    // composed from the per-entry rejections alone. Which of the ingestion
    // instruments that empty-dispatch branch records is
    // `Service::create_usage_records_for_origin`'s to state, not this
    // handler's: the handler only ever sees the `Ok(Vec::new())`.
    //
    // An entry refused by `decode_record_entry` / `record_request_into_domain`
    // is composed straight into the 207 envelope and never dispatched, so the
    // per-record counter cannot see it and by §3.11.5's scope rule must not.
    // It is not invisible: `submitted_before_decode` carries the count past
    // this line and `uc_ingestion_batch_size` samples it.
    let (indices, batch): (Vec<usize>, Vec<CreateUsageRecord>) = eligible.into_iter().unzip();

    // Batch-level dispatch failure (plugin resolution, SPI size mismatch,
    // an exhausted ingestion allowance) short-circuits as a whole-request
    // canonical envelope — the same failure would have hit every record
    // identically. Both dispatched entry points carry the same
    // post-condition: one result per dispatched record, in order.
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

/// Lift a whole-request ingestion failure, adding the one response header
/// this gear sets by hand: `Retry-After` on a quota rejection.
///
/// **Why this is not in the shared canonical lift.** `toolkit-canonical-errors`
/// derives `Retry-After` for the `service_unavailable` category and no other,
/// and `context.rs`'s `QuotaViolationV1` doc states that as policy rather than
/// an oversight, so a `resource_exhausted` 429 never reaches that branch.
/// DESIGN draws the conclusion in terms: "The canonical envelope derives a
/// `Retry-After` header for `ServiceUnavailable` only, so the gear sets that
/// header itself on a 429."
///
/// The header is built from the **same** `retry_after_seconds` the body's
/// `violations[0].retry_after_seconds` is, by taking both off one
/// `UsageCollectorError` — one quantity, two renderings, no second source to
/// drift.
///
/// Everything that is not a quota rejection takes the ordinary `Err` path. The
/// quota arm builds its response from the `CanonicalError` rather than from a
/// bare `Problem`, so it keeps the response extension the canonical error
/// middleware reads; the only difference is the extra header.
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

/// `GET /usage-collector/v1/records/{id}`
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

/// `GET /usage-collector/v1/records`
///
/// Keyset-paginated raw read over the persisted usage records.
///
/// `gts_type_id`, `from` and `to` are the mandatory non-OData query
/// parameters, all carrying typed values: `gts_type_id` is a [`MeterTypeId`]
/// and the `from` / `to` pair one validated [`TimeRange`]. Selection reads the
/// period end alone — see [`TimeRange::contains_window_end`]. The range never
/// travels inside `$filter`, which the query surface gate in
/// [`crate::domain::query`] states and owns; a missing, duplicated,
/// offset-less or inverted bound is a `400 InvalidArgument` raised where the
/// parameter is parsed, before the PDP call.
///
/// Gateway-side guards applied before the service is invoked:
///
/// * **`$top` cap** — `ODataQuery.limit` is bounded by [`MAX_PAGE_SIZE`], so a
///   caller cannot silently misinterpret a clamped page as complete.
/// * **Cursor decoding** — a present `cursor` has its signed keys decoded into
///   the effective `$orderby`; a malformed token surfaces as the canonical
///   `cursor_decode` `Problem`, and the decoded `CursorV1` flows to the plugin
///   via `ODataQuery.cursor` unchanged. Both bindings a cursor needs happen
///   behind the service — the order is overwritten from the token, and whether
///   the token was minted over *this* query is compared — per the cursor
///   lifecycle in [`crate::domain::query`]. The gear originates no cursor code
///   of its own (Spec §3.13); `FILTER_MISMATCH` and `INVALID_CURSOR` on the
///   wire are upstream's errors lifted by the host.
/// * **`$orderby` admissibility and normalization** — a caller order is refused
///   here, naming `$orderby`, when it mixes sort directions or names a key that
///   is not a mandatory record attribute; an admissible one then gains whichever
///   keyset key it does not already name, in its own sort direction. Both halves
///   are [`establish_keyset_order`]: owned in the domain so an in-process caller
///   gets it too, mirrored here so the caller's own input is blamed by name. A
///   continuation has no caller order to normalize.
///
/// Per-key metadata filtering is the typed side-channel [`MetadataFilter`] —
/// `toolkit-odata` has no surface for filtering on dynamic JSON-map keys. The
/// wire encoding is **repeated query parameters `metadata.<key>=<value>`**,
/// OR-ing within a key and AND-ing across keys.
///
/// PDP authorization, PDP-constraint composition into the `OData` filter, and
/// the plugin SPI dispatch all happen inside [`Service::list_usage_records`].
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

/// `POST /usage-collector/v1/records/aggregate`
///
/// Server-side aggregated read over the persisted usage records.
///
/// The wire shape mirrors `GET /usage-collector/v1/records` apart from where
/// the range travels: `gts_type_id` and the `OData` `$filter` are query
/// parameters, as on the raw path; the `metadata.<key>=<value>` side-channel
/// is a query parameter too, described in `docs/usage-collector-v1.yaml`'s
/// operation `description` rather than declared, since `OpenAPI` cannot name a
/// parameter whose name carries a placeholder. The JSON body carries
/// `time_range` and `group_by` and nothing else — there is no aggregation
/// parameter, because the fold is resolved from the queried type's declaration.
///
/// `from` / `to` are deliberately NOT accepted in the query string here: the
/// contract puts the range in the declared body, and admitting a second
/// spelling would accept a parameter nothing reads. Selection reads the period
/// end alone either way ([`TimeRange::contains_window_end`]), and the range is
/// never a `$filter` conjunct ([`crate::domain::query`]).
///
/// `$orderby`, `$top` / `limit`, and `cursor` are likewise not accepted — the
/// aggregation result is not paginated.
///
/// PDP authorization, declaration resolution, PDP-constraint composition into
/// the `OData` filter, and the plugin SPI dispatch all happen inside
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

/// Everything the aggregate path's pre-service validators produce.
///
/// A struct rather than a tuple: the element types happen to be distinct
/// today, so a positional swap would not compile, but the first field added
/// alongside a same-typed sibling takes that accident away silently. It matters
/// most for `time_range` — a range crossed with another parameter is an
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
/// The range comes out of the body here, not the query string, so
/// `deny_unknown_fields` on the DTO has already refused a body carrying
/// anything else and serde has already refused one omitting `time_range`; what
/// is left to check is the ordering, which [`TimeRange::new`] owns.
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
/// declared aggregate-OData set, the typed aggregate parameters
/// (`gts_type_id`), or a `metadata.<key>` entry. Silent drop of
/// unrecognised parameters is a documented contract-drift surface,
/// mirroring `list_usage_records`.
///
/// Checks [`TYPED_AGGREGATE_PARAMS`] rather than [`TYPED_LIST_PARAMS`], so
/// `from` / `to` are named in a `400` here instead of being accepted and
/// ignored: the aggregate path reads its range from the request body.
///
/// Traceability: half of `cpt-cf-usage-collector-algo-query-request-admission`
/// / `cpt-cf-usage-collector-dod-mandatory-type-and-range`'s offset rejection
/// ("reject any request carrying a numeric row offset") — this allowlist
/// admits no such parameter, so an `offset`/`skip` query parameter is
/// refused here as unrecognised.
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

/// Maximum number of records the gateway will request from the plugin
/// in a single page per
/// `cpt-cf-usage-collector-constraint-nfr-thresholds`.
/// A caller-supplied `$top` / `limit` above this ceiling is rejected with
/// 400 `InvalidArgument` (never silently clamped) so the plugin cannot be
/// coaxed into unbounded reads.
///
/// Domain-owned (`crate::domain::service::MAX_PAGE_SIZE`), like
/// `crate::domain::feed::MAX_FEED_LIMIT`: `Service::list_usage_records` clamps
/// `PageInfo.limit` to it too, for the in-process caller REST's own
/// `prepare_list_query` does not run in front of, so one constant bounds both
/// surfaces.
#[allow(
    clippy::redundant_pub_crate,
    reason = "plain `pub` fails E0364: the re-exported item is `pub(crate)` at its \
              definition in domain::service, and a `use` re-export may not exceed the \
              visibility of what it imports, even though `usage_records` is itself a \
              private module where `pub` and `pub(crate)` would otherwise be equivalent"
)]
pub(crate) use crate::domain::service::MAX_PAGE_SIZE;

/// `$`-prefixed `OData` parameters (parsed by the toolkit `OData`
/// extractor) and the non-`OData` scalars the toolkit also accepts.
///
/// Both `$top` (canonical `OData`) and `limit` (toolkit alias) are admitted:
/// [`toolkit::api::odata::ODataParams`] declares `limit` with
/// `#[serde(alias = "$top")]`, so both spellings fold onto one
/// `ODataQuery.limit` slot, and sending both is rejected as a duplicate field
/// before this allowlist runs.
///
/// `$select` is absent on purpose: the extractor parses it, but this gear never
/// applies the projection, so admitting it would discard a caller's field list
/// and return every field under a `200` that reads as the requested
/// projection.
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
/// 1. reject `$top > MAX_PAGE_SIZE` as `InvalidArgument`, never a silent clamp;
/// 2. run the domain's keyset floor [`establish_keyset_order`] on the caller's
///    `$orderby` (see [`crate::domain::query`] for the floor itself);
/// 3. decode the optional cursor's signed tokens, refusing a malformed token,
///    and materialize the order they carry onto the query — a mirror of the
///    domain's own binding, which is the authority.
///
/// Step 2 exists here, and not only behind the service, so a caller's own input
/// is refused where it is parsed and the `400` blames `$orderby` before any
/// authorization or plugin work happens. The floor is idempotent, so the
/// service re-applying it before dispatch is a no-op on this path.
// @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-odata-parse
// @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-cursor-in
fn prepare_list_query(mut query: ODataQuery) -> Result<ODataQuery, CanonicalError> {
    // 1. $top cap. The violation names `$top`, the canonical spelling,
    //    because the parsed `ODataQuery` does not record which accepted
    //    spelling arrived on the wire; the detail names the alias so a `limit`
    //    caller still recognises their own parameter. Same convention as
    //    `toolkit_odata`'s own `InvalidLimit` mapping.
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

    // 2. $orderby admissibility + the canonical keyset fields, both from the
    // domain's first-page floor (`crate::domain::query`).
    //
    // Skipped when a cursor is present: the toolkit OData extractor leaves
    // `order` empty on a cursor request (and rejects `$orderby` + `cursor`
    // together), so there is no caller order to floor yet — the effective one
    // is reconstructed from the token's signed keys in step 3, and the service
    // then puts it through `require_continuation_keyset`, which checks rather
    // than extends.
    if query.cursor.is_none() {
        establish_keyset_order(&mut query).map_err(usage_collector_error_to_canonical)?;
    }

    // 3. Cursor validation + order materialization. When a cursor is present,
    // `query.order` is empty by toolkit convention and the effective keyset
    // order lives in the cursor's signed-token payload (`cursor.s`). Derive it
    // and write it back into `query.order`, because the plugin reads that slot
    // to build both the `ORDER BY` and the keyset continuation predicate and
    // has no access to the token derivation.
    if let Some(cursor) = query.cursor.as_ref() {
        // Decoding only: a token whose signed keys are malformed is refused
        // where it arrives, in wire vocabulary. Neither comparison
        // `toolkit_odata::validate_cursor_against` would make belongs at this
        // edge. The filter hash does not, because a continuation is bound to
        // the caller's `$filter` AND the typed parameters and the service owns
        // that fingerprint end to end (`read_fingerprint`), while the
        // extractor's `filter_hash` covers `$filter` alone and is `None` for an
        // in-process caller. The order does not, because comparing a value
        // derived from `cursor.s` against `cursor.s` is vacuous; the comparison
        // that is not vacuous lives behind the service in
        // `bind_continuation_order`. The assignment below mirrors that binding
        // rather than owning it — same source, same result.
        query.order = toolkit_odata::ODataOrderBy::from_signed_tokens(&cursor.s)
            .map_err(CanonicalError::from)?;
    }

    Ok(query)
}
// @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-cursor-in
// @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-odata-parse

/// Reject any query parameter that is not one of the declared `OData`
/// parameters, one of the typed list parameters, or a
/// `metadata.<key>` entry — silent drop of unrecognised parameters is a
/// documented contract-drift surface.
///
/// Traceability: half of `cpt-cf-usage-collector-algo-query-request-admission`
/// / `cpt-cf-usage-collector-dod-mandatory-type-and-range`'s offset
/// rejection — [`TYPED_LIST_PARAMS`] admits no numeric row offset, so an
/// `offset`/`skip` query parameter is refused here as unrecognised.
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
/// through [`MeterTypeId::new`]. A missing value surfaces as the
/// canonical `InvalidArgument` `Problem` with a field violation on
/// `gts_type_id`; a malformed value lifts through the SDK's
/// [`UsageCollectorError::InvalidArgument`] mapping. A duplicate
/// occurrence is rejected so silent last-wins ambiguity cannot mask a
/// caller bug.
///
/// Traceability: the "exactly one GTS type reference" half of
/// `cpt-cf-usage-collector-algo-query-request-admission` /
/// `cpt-cf-usage-collector-dod-mandatory-type-and-range`, on the REST
/// surface — the in-process surface gets the same property structurally,
/// from `MeterTypeId` being a mandatory, singular typed parameter on every
/// `Service` read method.
// @cpt-algo:cpt-cf-usage-collector-algo-query-request-admission:p1
// @cpt-dod:cpt-cf-usage-collector-dod-mandatory-type-and-range:p1
fn parse_required_gts_type_id(params: &[(String, String)]) -> Result<MeterTypeId, CanonicalError> {
    let raw = require_single_value(params, "gts_type_id")?;
    MeterTypeId::new(raw.clone()).map_err(usage_collector_error_to_canonical)
}

/// Extract the mandatory `from` / `to` query parameters into a validated
/// [`TimeRange`].
///
/// Both are parsed as RFC 3339, which rejects an offset-less timestamp —
/// `docs/usage-collector-v1.yaml`'s `Timestamp` requires an offset, and a bare
/// local time would silently attribute usage to whatever offset the server
/// happened to assume. Absence and duplicates go through
/// [`require_single_value`], so last-wins ambiguity cannot mask a caller
/// bug, and the ordering check is [`TimeRange::new`]'s.
///
/// Runs pre-PDP, alongside [`parse_required_gts_type_id`]: a request whose
/// typed parameters do not parse has no shape to authorize.
///
/// Traceability: the "exactly one time range, as a typed parameter never
/// expressible as a filter conjunct" half of
/// `cpt-cf-usage-collector-algo-query-request-admission` /
/// `cpt-cf-usage-collector-dod-mandatory-type-and-range`.
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

/// Group `metadata.<key>=<value>` entries into a `Vec<MetadataFilter>`
/// — one filter per distinct key, with that key's full ordered value
/// list (duplicates preserved verbatim — the OR semantics within a
/// single filter make them harmless).
///
/// An empty key (`metadata.=value`) is rejected as a canonical
/// `InvalidArgument` `Problem`; a `metadata.<key>` with an empty value
/// is admitted verbatim because the SDK does not constrain
/// [`MetadataFilter`] values beyond their `String` type.
///
/// Caps nothing: the metadata caps live in
/// [`crate::domain::query::require_metadata_filter_within_caps`], the one point
/// every surface passes through. This parser's job is the wire-to-typed
/// grouping; the empty-key check above stays here because an in-process caller
/// builds a `Vec<MetadataFilter>` directly and never reaches this function.
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
/// Factored out so [`require_single_value`] and [`single_param`] share one
/// definition of "how many times did the caller send this" rather than
/// each re-filtering `params` on its own — the two differ only in what
/// zero occurrences means.
fn param_occurrences<'a>(
    params: &'a [(String, String)],
    key: &'static str,
) -> impl Iterator<Item = &'a String> {
    params.iter().filter(move |(k, _)| k == key).map(|(_, v)| v)
}

/// The canonical `InvalidArgument` for a query parameter sent more than
/// once. Shared by [`require_single_value`] and [`single_param`]: a
/// mandatory and an optional parameter refuse a duplicate on identical
/// terms, whichever one `key` names.
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

/// Read at most one value for `key`; `None` when it is absent.
///
/// The feed's `limit`, `cursor` and `until` (`api/rest/handlers/usage_feed.rs`)
/// are optional, unlike [`require_single_value`]'s mandatory parameters, so
/// absence is not an error here — but a repeated occurrence still is.
/// Last-one-wins would silently accept a request whose two values just as
/// plausibly meant the caller sent the wrong one twice.
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
#[allow(clippy::result_large_err)]
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

/// An explicit `"idempotency_key": null` is refused as a missing key, and
/// this is the only thing that attributes it to the property it came from.
///
/// The DTO types the property `String` because the schema requires it, so a
/// body stating `null` fails to decode as ``invalid type: null, expected a
/// string``. That message names no property: `serde_field_name` reads only
/// ``unknown field `…` `` and ``missing field `…` ``, so without this
/// pre-check the violation lands on `records` and the caller is told an
/// entry is wrong without being told which half of it. Running before the
/// decode is what keeps the answer on `idempotency_key`, and it also puts
/// the answer ahead of any other fault the same entry carries.
///
/// One answer for both entry kinds: a key is required on a withdrawal exactly
/// as on a measurement, because a withdrawal repeats its target's key and that
/// repetition is what locates the target (DESIGN §3.1, "Target resolution").
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

/// Parse the wire `entry_type` into the SDK's closed [`EntryType`].
///
/// Decoded through the enum's own `Deserialize` rather than matched against
/// literals here, so the accepted vocabulary has exactly one definition —
/// `#[serde(rename_all = "lowercase")]` on [`EntryType`] — and a variant
/// added there needs no second edit to be parseable. serde's own message
/// carries both the rejected spelling and the expected set, so the violation
/// needs no curated text beside it.
///
/// The rejection shape is host-private: the transport-agnostic SDK never
/// sees the wire string and has no error for one. It names `entry_type`, so
/// a batch entry is rejected at its own index pointing at the property that
/// is wrong.
///
/// **Traceability: `cpt-cf-usage-collector-dod-explicit-entry-type`.** This
/// function is that definition of done's "closed pair" half, on both ingestion
/// routes at once — they share [`CreateUsageRecordRequest`] and this fold. Its
/// other halves:
/// * *Required, with no default.* Neither
///   [`CreateUsageRecordRequest::entry_type`] nor the SDK's own
///   `CreateUsageRecordWire` carries `serde(default)`, so a body omitting it
///   fails to decode as ``missing field `entry_type` `` — which
///   `serde_field_name` recognises, so the violation names the property.
/// * *A third value is rejected rather than extended*, as ``unknown variant
///   `…`, expected `record` or `invalidation` ``.
/// * *Never inferred, and in particular never from the quantity's value or
///   sign.* Nothing on this path or in the SDK projection branches on
///   `quantity`.
/// * *A copy declaring `record` is an ordinary record that withdraws nothing*,
///   because `entry_type` is a derivation component
///   ([`usage_collector_sdk::derive_usage_record_id`]) and a faithful copy
///   repeats the rest, so the store absorbs it as a retry.
// @cpt-dod:cpt-cf-usage-collector-dod-explicit-entry-type:p1
#[allow(clippy::result_large_err)]
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
/// input, lifting `entry_type`-, `gts_type_id`-, attribution-,
/// `idempotency_key`-, `reason_code`- and metadata-shape failures into
/// per-record `Problem` envelopes. The covered period is caller-supplied and
/// forwarded verbatim; it is validated — and rejected, never truncated —
/// where it is read, inside the SDK projection
/// ([`usage_collector_sdk::CreateUsageRecord::try_into_usage_record`] or
/// [`usage_collector_sdk::CreateUsageRecord::try_into_invalidation_record`]),
/// which is also where the entry's `id` is derived once, authoritatively,
/// inside [`Service::create_usage_records`]. An accepted entry carries no
/// lifecycle flag to stamp: it is never rewritten
/// (`cpt-cf-usage-collector-adr-append-only-invalidation`).
///
/// **The discriminator is read, not inferred.** `entry_type` is
/// caller-supplied and required on the wire (DESIGN §3.1, "Entry type and
/// reason code"), so this fold parses it into
/// [`usage_collector_sdk::EntryType`] rather than deducing a kind from some
/// other property's presence. An unrecognised spelling is refused here, naming
/// `entry_type`.
///
/// **It folds no reference.** `invalidates` is server-assigned (DESIGN §3.1,
/// "Field ownership") and the request DTO carries no field for it, so there is
/// no flat pair to rejoin and no half-shape to refuse. The domain's
/// `invalidation` is the reason code alone; the gateway derives the target from
/// the submission's own identity inputs and stamps it on projection. Whether
/// `entry_type` and `reason_code` agree is the SDK projection's check — the
/// same check on every submission path, so a second copy here is how the two
/// would come to disagree.
///
/// **Not a realization site for
/// `cpt-cf-usage-collector-algo-attribution-structural-validation` /
/// `cpt-cf-usage-collector-dod-attribution-structural-validation`.** That
/// algorithm requires its checks to run "inside the domain trait
/// implementation of the owning component, never in a REST handler," so
/// in-process and REST callers reach identical behaviour — and
/// `domain/local_client.rs`'s in-process entry point forwards a caller-built
/// `Vec<CreateUsageRecord>` straight through with no equivalent check.
///
/// What the field conversions here actually guard: `resource_ref` and
/// `subject_ref`'s `subject_id`-implies-presence shape are constructor-guarded
/// on **both** callers, since `CreateUsageRecord` cannot be built with an
/// invalid one. `tenant_id` is **not** — it is a bare `pub tenant_id: Uuid`
/// field set by struct literal below, with no non-nil check on either caller,
/// where the algorithm's `inst-attrval-tenant` asks for one "deliberately
/// redundant with the PDP decision".
///
/// **IS a realization site for
/// `cpt-cf-usage-collector-algo-quantity-validation` /
/// `cpt-cf-usage-collector-dod-quantity-contract`, on this surface.**
/// `CreateUsageRecord::quantity` is typed `UsageQuantity`, whose only public
/// constructors are validating, so the type cannot be built with an
/// out-of-range, non-finite or negative-zero value on **either** caller.
/// `UsageQuantity::parse(&req.quantity)` below is this surface's fold of the
/// wire string into that type, and an out-of-range value rejects its own entry
/// here; the in-process caller carries the same enforcement without a second
/// call, because its value was already refused wherever it was built.
// @cpt-algo:cpt-cf-usage-collector-algo-quantity-validation:p1
// @cpt-dod:cpt-cf-usage-collector-dod-quantity-contract:p1
#[allow(clippy::result_large_err)]
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

    let idempotency_key = Some(
        IdempotencyKey::new(req.idempotency_key)
            .map_err(|err| Problem::from(usage_collector_error_to_canonical(err)))?,
    );

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
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
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
