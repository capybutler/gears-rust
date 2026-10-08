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

/// `DESIGN.md` Section 3.10's consistency floor, plus its coupling
/// obligation, carried verbatim and worded identically onto every read
/// surface's `.description()`: this module's
/// three query routes (directly), `usage_feed.rs`'s feed route and
/// `reconciliation.rs`'s route (both via `pub(super)`, referencing this
/// same constant rather than holding their own copy — fix round 1 folded
/// three independently-typed copies into one, closing both the
/// identical-wording risk and the marker-siting gap a prior round left: a
/// single realizing site now covers all three REST files), and
/// `UsageCollectorPluginV1`'s own trait doc in `usage-collector-sdk`, which
/// embeds this exact text (fix round 1: the two were not identical before;
/// `plugin_api.rs`'s own doc now quotes this string verbatim, modulo
/// Markdown code-span backticks the pin strips before comparing).
///
/// **Carries DESIGN §3.10's `eventual`-dedup qualifier**, added in fix
/// round 1: the ingestion-ack sentence alone, without it, told a reader
/// an acknowledged entry is durable with no caveat — a stronger guarantee
/// than `eventual` dedup actually gives, which is exactly the harm entry 38
/// exists to prevent. `DEDUP_HORIZON_NOTE` below carries the other half of
/// the same DESIGN bullet (the retention-tied visibility horizon); this is
/// the convergence-race half.
///
/// Pulled out to a constant rather than inlined at each call site because
/// inlining it pushed [`register_usage_record_routes`] over
/// `clippy::too_many_lines`.
///
/// **Realizes the REST half of `dod-consistency-floor-published` and of
/// `dod-staleness-coupling-recorded`, now for all three REST files sharing
/// this one constant; the SPI half of both is `UsageCollectorPluginV1`'s
/// own trait doc (`usage-collector-sdk/src/plugin_api.rs`, "Consistency
/// floor parity"), which disclaims this half in turn.** This crate's seam
/// with the SDK crate is what makes the two sides worth marking separately
/// rather than once: a reviewer of this file alone sees the REST surface
/// publish the floor and the coupling obligation, never the SPI; the SPI's
/// own also-required absence (no profile-advertisement method) is not
/// observable from here at all. Both halves are now pinned together by
/// `consistency_floor_tests::every_required_site_publishes_the_full_floor_statement`,
/// which reads `plugin_api.rs` out of the SDK crate the same way
/// `data_classification_tests.rs` already does.
// @cpt-dod:cpt-cf-usage-collector-dod-consistency-floor-published:p1
// @cpt-dod:cpt-cf-usage-collector-dod-staleness-coupling-recorded:p1
pub(super) const CONSISTENCY_FLOOR_STATEMENT: &str = "Consistency floor (DESIGN.md Section \
    3.10): after an ingestion call returns the persisted entry, that entry is durable. Under \
    eventual dedup level, an acknowledged entry can still lose a race before convergence \
    (DESIGN.md Section 3.1, Dedup level). This read surface is eventually consistent with no \
    upper bound relative to a same-tenant ingestion ack. No monotonic-reads guarantee at the \
    floor. The floor is per (tenant_id, gts_type_id). The floor claims no ordering of entries. \
    A consumer depending on a tighter bound than this floor must record that dependency in \
    its own design document, naming the plugin, the dimension, and the value; weakening a \
    published bound is a breaking change for every coupled consumer, and the Plugin SPI \
    publishes no runtime method for discovering a plugin's ceiling in v1.";

/// `DESIGN.md` Section 3.10's read-after-write consumer rule: a same-request
/// outcome is taken from the ingestion acknowledgement, never from a query
/// surface. Published on the ingestion endpoint and on each of the three
/// query endpoints (`cpt-cf-usage-collector-dod-consistency-read-after-write-rule`'s
/// own Assertion names all four) -- measured over this one module, unlike
/// [`CONSISTENCY_FLOOR_STATEMENT`], since the ingestion route this clause
/// also covers lives here rather than across the SDK-crate seam.
// @cpt-dod:cpt-cf-usage-collector-dod-consistency-read-after-write-rule:p1
const READ_AFTER_WRITE_NOTE: &str = "Read-after-write flows (admission control, post-emit \
    summary, immediate-readback dashboards) must consume the ingestion acknowledgement, \
    never a query surface.";

/// `DESIGN.md` Section 3.10's raw-tailing caveat: published only on the raw
/// path (`list_usage_records`), the one surface it describes.
const RAW_TAILING_NOTE: &str = "Raw tailing is best-effort, not a change feed; a consumer \
    that must not miss entries reads GET /usage-collector/v1/feed instead.";

/// `DESIGN.md` Section 3.10's dedup-identity-visibility horizon: published
/// only on the ingestion surface, where
/// `cpt-cf-usage-collector-dod-dedup-visibility-consistency-horizon`'s own
/// Assertion looks for it.
const DEDUP_HORIZON_NOTE: &str = "An accepted entry's dedup identity stays visible to \
    subsequent ingestion attempts for as long as the referenced type's retention policy \
    keeps it (a per-meter floor, not a gear-wide window); a retry arriving after the entry \
    has aged out of retention draws no guaranteed outcome.";

/// **Traceability (feature 2.5): `cpt-cf-usage-collector-dod-pair-remains-readable`
/// is realized here, and it is realized by an absence.** That definition of
/// done requires the ledger to stay append-only in the strict sense and
/// names the mechanism: "the absence of such an operation **MUST** be the
/// mechanism rather than a runtime guard". Measured over the three route
/// modules — `usage_records.rs`, `usage_feed.rs` and `reconciliation.rs` —
/// every registered builder is a `get` or a `post`, and **no mutating verb
/// appears at all**:
/// `grep -rn 'OperationBuilder::\(put\|patch\|delete\)' usage-collector/src/`
/// returns nothing. That grep is the one to re-run; it is stated without a
/// tally on purpose, because a verb-*counting* pattern matches its own
/// mention in a comment like this one, which is how the tally that used to
/// stand here came to disagree with the command beside it. No `post`
/// addresses an existing entry either: two are the ingestion routes, which
/// append, and the third is the aggregate read, a `post` only because it
/// carries a request body. `UsageCollectorPluginV1` is the same shape:
/// `create_usage_records`, `get_usage_record`,
/// `query_aggregated_usage_records`, `list_usage_records`, `read_feed_page`
/// and `get_reconciliation_metadata` — six calls, none of which updates,
/// deletes or flags a stored entry. A withdrawn entry therefore has no
/// surface that could rewrite it, carries no status field and no lifecycle
/// flag to flip, and both halves of a withdrawn pair come back from every
/// read path as persisted (`a_withdrawn_pair_folds_to_nothing_while_both_stay_readable`).
/// `cpt-cf-usage-collector-dod-no-invalidation-of-invalidation`'s last
/// sentence — "No surface **MUST** offer reversal of an accepted
/// invalidation" — is the same absence seen from the other side.
// @cpt-dod:cpt-cf-usage-collector-fr-ingestion:p1
// @cpt-dod:cpt-cf-usage-collector-component-ingestion-gateway:p1
// @cpt-dod:cpt-cf-usage-collector-dod-pair-remains-readable:p1
pub(super) fn register_usage_record_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
) -> Router {
    router = OperationBuilder::post("/usage-collector/v1/records")
        .operation_id("usage_collector.create_usage_records")
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
    // @cpt-dod:cpt-cf-usage-collector-dod-backfill-route:p2
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

    // @cpt-flow:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-query-raw:p1
    //
    // DE0802 requires every `$`-prefixed `OData` parameter to be declared
    // through `OperationBuilderODataExt`, so that the description and the
    // schema come from one place and gears cannot drift apart on them.
    // The trait offers `with_odata_filter`, `with_odata_orderby`, and
    // `with_odata_select` — there is no `$top` method. `$top` is bound on
    // the wire instead, by `ODataParams.limit`'s `#[serde(alias = "$top")]`,
    // and no builder method was added alongside it, so the rule has nothing
    // to satisfy it with here.
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
    router = OperationBuilder::post("/usage-collector/v1/records/aggregate")
        .operation_id("usage_collector.query_aggregated_usage_records")
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
    router = OperationBuilder::get("/usage-collector/v1/records/{id}")
        .operation_id("usage_collector.get_usage_record")
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
