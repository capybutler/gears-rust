//! REST handler for the Feed Gateway's `GET /usage-collector/v1/feed`
//! (DESIGN §3.2). A thin pass-through, the same shape as the handlers in
//! `usage_records.rs`: it pulls the gateway-resolved `SecurityContext`,
//! decodes the wire parameters into the domain / SDK types
//! [`Service::read_usage_feed`] takes, dispatches to it, and lifts
//! `UsageCollectorError` through the host-owned canonical mapping.
//! Authorization runs inside the service, not here.

use std::sync::Arc;

use axum::extract::{Extension, Query};
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;
use usage_collector_sdk::{CursorField, FeedStart};

use crate::api::rest::dto::FeedPageDto;
use crate::api::rest::handlers::usage_records::single_param;
use crate::domain::Service;
use crate::domain::feed;
use crate::infra::sdk_error_mapping::usage_collector_error_to_canonical_for_usage_record as usage_collector_error_to_canonical;

/// `GET /usage-collector/v1/feed`
///
/// Replay-safe, snapshot-consistent read path for charging consumers
/// (DESIGN §3.2, Feed Gateway).
///
/// A request carrying no `cursor` compiles to [`FeedStart::Oldest`] rather
/// than passing an absent position down: "oldest retained" and "no
/// position supplied" are different instructions and only one of them is
/// a start (DESIGN §3.1). There is no head-start option.
///
/// `limit`, `cursor` and `until` are each optional and each read through
/// [`single_param`], which refuses a repeated occurrence as a caller
/// error (`400`) rather than resolving it last-one-wins: a caller who
/// sent a parameter twice most likely made a mistake worth surfacing, not
/// a preference between the two values worth guessing at silently.
///
/// `cursor` and `until` are each decoded on their own field name
/// ([`feed::decode_cursor_token`], ruling E7), so a defect in either
/// names the parameter it came from rather than always naming `cursor`.
/// Decoding here is syntactic only — the subscription-binding guard is
/// [`Service::read_usage_feed`]'s, so an in-process SDK caller who
/// supplies an already-decoded `CursorV1` and never reaches this handler
/// meets it too.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
// @cpt-flow:cpt-cf-usage-collector-component-feed-gateway:p1
// @cpt-flow:cpt-cf-usage-collector-seq-read-feed:p1
// @cpt-dod:cpt-cf-usage-collector-dod-feed-oldest-start:p1
pub async fn handle_read_usage_feed(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<Service>>,
    Query(params): Query<Vec<(String, String)>>,
) -> ApiResult<Json<FeedPageDto>> {
    // @cpt-begin:cpt-cf-usage-collector-seq-read-feed:p1:inst-feed-handler-decode
    let raw_types: Vec<String> = params
        .iter()
        .filter(|(k, _)| k == "gts_type_id")
        .map(|(_, v)| v.clone())
        .collect();
    let subscription =
        feed::build_subscription(&raw_types).map_err(usage_collector_error_to_canonical)?;

    // `parse_limit` both parses and bounds-checks, so this handler invents
    // no second error shape for a condition `domain::feed` already owns.
    let limit = single_param(&params, "limit")?
        .map(feed::parse_limit)
        .transpose()
        .map_err(usage_collector_error_to_canonical)?;

    let cursor = single_param(&params, "cursor")?
        .map(|t| feed::decode_cursor_token(t, CursorField::Cursor))
        .transpose()
        .map_err(usage_collector_error_to_canonical)?;
    let until = single_param(&params, "until")?
        .map(|t| feed::decode_cursor_token(t, CursorField::Until))
        .transpose()
        .map_err(usage_collector_error_to_canonical)?;

    // @cpt-begin:cpt-cf-usage-collector-dod-feed-oldest-start:p1:inst-feed-handler-compile-oldest
    let start = match cursor.as_ref() {
        Some(c) => FeedStart::After(c),
        None => FeedStart::Oldest,
    };
    // @cpt-end:cpt-cf-usage-collector-dod-feed-oldest-start:p1:inst-feed-handler-compile-oldest
    // @cpt-end:cpt-cf-usage-collector-seq-read-feed:p1:inst-feed-handler-decode

    // @cpt-begin:cpt-cf-usage-collector-seq-read-feed:p1:inst-feed-handler-dispatch
    let page = service
        .read_usage_feed(&ctx, &subscription, start, until.as_ref(), limit)
        .await
        .map_err(usage_collector_error_to_canonical)?;
    // @cpt-end:cpt-cf-usage-collector-seq-read-feed:p1:inst-feed-handler-dispatch

    // @cpt-begin:cpt-cf-usage-collector-seq-read-feed:p1:inst-feed-handler-return
    // `FeedPage` carries no `limit` of its own — see `FeedPageDto`'s doc —
    // so `page_info.limit` is derived here, through the same
    // `resolve_limit` the service just applied. Idempotent given a `limit`
    // already bound-checked above, same arrangement as
    // `establish_keyset_order` running once at this edge and again,
    // harmlessly, behind the raw path's own service call.
    let effective_limit = feed::resolve_limit(limit).map_err(usage_collector_error_to_canonical)?;

    let dto = FeedPageDto::try_from_feed_page(page, effective_limit)
        .map_err(usage_collector_error_to_canonical)?;

    Ok(Json(dto))
    // @cpt-end:cpt-cf-usage-collector-seq-read-feed:p1:inst-feed-handler-return
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "usage_feed_tests.rs"]
mod usage_feed_tests;
