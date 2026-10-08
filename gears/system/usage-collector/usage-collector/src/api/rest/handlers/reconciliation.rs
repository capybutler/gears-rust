//! REST handler for the Query Gateway's fourth read path,
//! `GET /usage-collector/v1/reconciliation` (DESIGN §3.2). A thin
//! pass-through, the same shape as the handlers in `usage_records.rs` and
//! `usage_feed.rs`: it pulls the gateway-resolved `SecurityContext`, decodes
//! the wire parameters into the typed arguments
//! [`Service::get_reconciliation_metadata`] takes, dispatches to it, and
//! lifts `UsageCollectorError` through the host-owned canonical mapping.
//! Authorization runs inside the service, not here.
//!
//! All five parameters are required
//! (`cpt-cf-usage-collector-dod-reconciliation-single-scope`): the reporting
//! granularity (`scope`), the target GTS type, the target tenant, and the
//! `from` / `to` range. Each is read through [`required_param`], built on
//! [`single_param`] — a repeated occurrence is refused the same way an
//! optional feed parameter's is (`single_param`'s duplicate rejection is
//! reused unchanged), and absence is refused too, naming the parameter,
//! which `single_param` alone does not do for a mandatory input.
//!
//! **The naming hazard.** The wire parameter `scope` is the reporting
//! *granularity*; `domain::reconciliation::parse_granularity` admits it.
//! The response body's `scope` field is a different thing again — the
//! tenant-and-GTS-type target, built here via
//! [`crate::domain::reconciliation::build_scope`] from this request's own
//! `tenant_id` / `gts_type_id`, since [`ReconciliationMetadata`] itself
//! carries no scope. Neither is the gear's compiled PDP scope, which never
//! reaches this handler at all. `domain::reconciliation`'s module doc
//! carries the full warning.

use std::sync::Arc;

use axum::extract::{Extension, Query};
use time::OffsetDateTime;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;
use usage_collector_sdk::{MeterTypeId, ReconciliationMetadata, TimeRange};
use uuid::Uuid;

use crate::api::rest::dto::ReconciliationMetadataDto;
use crate::api::rest::handlers::usage_records::single_param;
use crate::domain::Service;
use crate::domain::reconciliation::{build_scope, parse_granularity};
use crate::infra::sdk_error_mapping::UsageRecordResource;
use crate::infra::sdk_error_mapping::usage_collector_error_to_canonical_for_usage_record as usage_collector_error_to_canonical;

/// `GET /usage-collector/v1/reconciliation`
///
/// Per-scope ingestion counters and watermarks, sufficient for an external
/// reconciliation job to compare gear-side accepted totals against a
/// consumer's processed totals for a range without a full raw scan. Stall
/// detection is consumer-side: this path evaluates nothing it returns.
///
/// No `$filter`, no grouping dimension, no ordering and no paging parameter
/// — the scope parameters and the range are the whole request, and the
/// response is one typed body rather than a page
/// (`cpt-cf-usage-collector-dod-canonical-page-envelope` deliberately does
/// not reach this path).
// @cpt-flow:cpt-cf-usage-collector-flow-reconciliation-compare-totals:p2
// @cpt-dod:cpt-cf-usage-collector-dod-reconciliation-single-scope:p2
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub async fn handle_get_reconciliation_metadata(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<Service>>,
    Query(params): Query<Vec<(String, String)>>,
) -> ApiResult<Json<ReconciliationMetadataDto>> {
    // The granularity is admitted here and carries nothing past admission —
    // v1 serves exactly one (`Service::get_reconciliation_metadata`'s own
    // doc), so `parse_granularity` returns `()` rather than a value this
    // handler would otherwise have nowhere to put.
    let granularity = required_param(&params, "scope")?;
    parse_granularity(granularity).map_err(usage_collector_error_to_canonical)?;

    let gts_type_id_raw = required_param(&params, "gts_type_id")?;
    let gts_type_id =
        MeterTypeId::new(gts_type_id_raw).map_err(usage_collector_error_to_canonical)?;

    let tenant_id_raw = required_param(&params, "tenant_id")?;
    let tenant_id = parse_tenant_id(tenant_id_raw)?;

    let from = parse_range_bound(&params, "from")?;
    let to = parse_range_bound(&params, "to")?;
    let time_range = TimeRange::new(from, to).map_err(usage_collector_error_to_canonical)?;

    // Built from the request's own parameters, not from the service's
    // answer — see the module doc's naming hazard.
    let scope = build_scope(tenant_id, gts_type_id.clone());

    let metadata: ReconciliationMetadata = service
        .get_reconciliation_metadata(&ctx, tenant_id, gts_type_id, time_range)
        .await
        .map_err(usage_collector_error_to_canonical)?;

    Ok(Json(ReconciliationMetadataDto::from_metadata(
        scope, metadata,
    )))
}

/// Read exactly one occurrence of `key`, refusing both absence and a
/// duplicate.
///
/// Built on [`single_param`], whose duplicate rejection is reused unchanged
/// — a repeated required parameter and a repeated optional one are refused
/// on identical terms (Review Focus 4: `?tenant_id=a&tenant_id=b` must never
/// resolve last-one-wins on the surface whose whole purpose is deciding
/// whether an invoice is right). `single_param` itself returns `Option` and
/// has no opinion on absence, since the feed parameters it was built for are
/// all optional; every reconciliation parameter is mandatory, so this adds
/// the missing-parameter violation `single_param` does not need.
fn required_param<'a>(
    params: &'a [(String, String)],
    key: &'static str,
) -> Result<&'a str, CanonicalError> {
    single_param(params, key)?.ok_or_else(|| missing_param_violation(key))
}

/// The canonical `InvalidArgument` for an absent mandatory query parameter.
fn missing_param_violation(key: &'static str) -> CanonicalError {
    UsageRecordResource::invalid_argument()
        .with_field_violation(
            key,
            format!("missing required query parameter `{key}`"),
            "VALIDATION",
        )
        .create()
}

/// Parse `raw` as a `uuid`, blaming `tenant_id` on failure.
fn parse_tenant_id(raw: &str) -> Result<Uuid, CanonicalError> {
    Uuid::parse_str(raw).map_err(|_| {
        UsageRecordResource::invalid_argument()
            .with_field_violation(
                "tenant_id",
                format!("query parameter `tenant_id` is not a valid UUID: `{raw}`"),
                "VALIDATION",
            )
            .create()
    })
}

/// Parse one RFC 3339 range bound out of `params`, blaming `key` on failure
/// so the caller learns which of the two parameters is wrong. Absence and
/// duplicates go through [`required_param`]; ordering is
/// [`TimeRange::new`]'s.
fn parse_range_bound(
    params: &[(String, String)],
    key: &'static str,
) -> Result<OffsetDateTime, CanonicalError> {
    let raw = required_param(params, key)?;
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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "reconciliation_tests.rs"]
mod reconciliation_tests;
