//! `OperationBuilder` route registration for the Feed Gateway's
//! `GET /usage-collector/v1/feed` (DESIGN §3.2).

use axum::Router;
use toolkit::api::canonical_prelude::*;
use toolkit::api::{OpenApiRegistry, OperationBuilder};

use super::usage_records::CONSISTENCY_FLOOR_STATEMENT;
use super::{dto, handlers};

const USAGE_FEED_TAG: &str = "Usage Feed";

// @cpt-flow:cpt-cf-usage-collector-component-feed-gateway:p1
// @cpt-flow:cpt-cf-usage-collector-seq-read-feed:p1
pub(super) fn register_usage_feed_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
) -> Router {
    // No `.with_odata_filter()` / `.with_odata_orderby()`: the feed admits
    // no caller order (DESIGN §3.1, Order admissibility: "The feed admits
    // no caller order") and no caller filter (§3.2, Feed Gateway
    // responsibility scope: the subscription's GTS types are the only
    // admitted narrowing, and everything else is excluded from the pages
    // and the cursor). Adding either would advertise a parameter nothing
    // reads.
    // @cpt-begin:cpt-cf-usage-collector-seq-read-feed:p1:inst-register-route-read-feed
    router = OperationBuilder::get("/usage-collector/v1/feed")
        .operation_id("usage_collector.read_usage_feed")
        .summary("Read a page of the usage feed")
        .description(format!(
            "Replay-safe, snapshot-consistent, pull-based read path for charging \
             consumers. A request carrying no `cursor` begins at the oldest position \
             this subscription still serves, and no start begins at the head. The \
             snapshot guarantee bounds what one scan observes once an entry is \
             visible; it does not bound when that entry becomes visible. \
             {CONSISTENCY_FLOOR_STATEMENT}"
        ))
        .tag(USAGE_FEED_TAG)
        .query_param_array(
            "gts_type_id",
            true,
            "Subscription: the GTS types this consumer reads. Repeat for each type \
             (1..100).",
            "string",
        )
        .query_param_typed(
            "limit",
            false,
            "Entries per page (1..1000, default 100); refused as `400` if repeated",
            "integer",
        )
        .query_param(
            "cursor",
            false,
            "Opaque continuation token this feed issued; refused as `400` if repeated",
        )
        .query_param(
            "until",
            false,
            "A later feed cursor bounding a replay; refused as `400` if repeated",
        )
        .authenticated()
        .no_license_required()
        .handler(handlers::handle_read_usage_feed)
        .json_response_with_schema::<dto::FeedPageDto>(
            openapi,
            StatusCode::OK,
            "A page of feed entries with its cursor",
        )
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);
    // @cpt-end:cpt-cf-usage-collector-seq-read-feed:p1:inst-register-route-read-feed

    router
}
