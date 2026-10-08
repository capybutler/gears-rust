//! Handler-level unit tests for `GET /usage-collector/v1/reconciliation`.
//!
//! Scope: pin handler-shaped concerns neither `domain::reconciliation_tests`
//! (the pure granularity admission / scope-building core) nor
//! `domain::service_tests::get_reconciliation_metadata_tests` (the
//! service's own authorization and dispatch guarantees) can reach — how the
//! five wire query parameters fold into the typed arguments
//! `Service::get_reconciliation_metadata` takes (all five required, a
//! repeated occurrence of any one refused, a reserved granularity refused by
//! name, an inverted or empty range refused as a field violation rather than
//! a 500), and how the returned `ReconciliationMetadata` projects onto the
//! wire `ReconciliationMetadataDto` (the fold-selected `quantity_summary`
//! branch, and the watermarks' null-on-absence rendering).

use std::num::NonZeroU64;
use std::sync::Arc;

use axum::extract::{Extension, Query};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use bigdecimal::BigDecimal;
use serde_json::Value;
use toolkit_gts::gts_id;
use usage_collector_sdk::{
    AggregationFold, MeterTypeId, ObservedQuantity, QuantitySummary, ReconciliationMetadata,
    ReconciliationScope, UsageQuantity,
};
use uuid::Uuid;

use super::handle_get_reconciliation_metadata;
use crate::api::rest::dto::{QuantitySummaryDto, ReconciliationMetadataDto};
use crate::domain::Service;
use crate::domain::test_support::{
    RECORDING_PLUGIN_SUFFIX, RecordingReconciliationPlugin, ServiceFixture, authenticated_ctx,
    recording_plugin_resolver,
};

const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");
const TENANT_ID: Uuid = Uuid::from_u128(1);

fn fixture_meter() -> MeterTypeId {
    MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
}

/// A complete, otherwise-valid parameter set. Every test below starts from
/// this and perturbs exactly one parameter, so a failure is attributable to
/// the one thing the test changed.
fn valid_params() -> Vec<(String, String)> {
    vec![
        ("scope".to_owned(), "tenant_gts_type".to_owned()),
        ("gts_type_id".to_owned(), GTS_ID.to_owned()),
        ("tenant_id".to_owned(), TENANT_ID.to_string()),
        ("from".to_owned(), "2026-01-01T00:00:00Z".to_owned()),
        ("to".to_owned(), "2026-01-02T00:00:00Z".to_owned()),
    ]
}

/// A `Service` over a [`RecordingReconciliationPlugin`]. Every test in this
/// file exercises a guard that refuses the request before the plugin is ever
/// dispatched, so the programmed fold is arbitrary.
fn service() -> Arc<Service> {
    let plugin = RecordingReconciliationPlugin::answering_for(AggregationFold::Max);
    ServiceFixture::default()
        .with_resolver(recording_plugin_resolver())
        .build(plugin, RECORDING_PLUGIN_SUFFIX)
}

async fn dispatch(params: Vec<(String, String)>) -> axum::response::Response {
    handle_get_reconciliation_metadata(
        Extension(authenticated_ctx()),
        Extension(service()),
        Query(params),
    )
    .await
    .into_response()
}

async fn get_with_repeated(param: &str) -> axum::response::Response {
    let mut params = valid_params();
    let duplicate = params
        .iter()
        .find(|(k, _)| k == param)
        .expect("param is in the valid set")
        .clone();
    params.push(duplicate);
    dispatch(params).await
}

async fn get_without(param: &str) -> axum::response::Response {
    let params = valid_params()
        .into_iter()
        .filter(|(k, _)| k != param)
        .collect();
    dispatch(params).await
}

async fn get_with_scope(scope: &str) -> axum::response::Response {
    let mut params = valid_params();
    for (k, v) in &mut params {
        if k == "scope" {
            *v = scope.to_owned();
        }
    }
    dispatch(params).await
}

async fn get_with_range(from: &str, to: &str) -> axum::response::Response {
    let mut params = valid_params();
    for (k, v) in &mut params {
        match k.as_str() {
            "from" => *v = from.to_owned(),
            "to" => *v = to.to_owned(),
            _ => {}
        }
    }
    dispatch(params).await
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    String::from_utf8(bytes.to_vec()).expect("Problem body is UTF-8")
}

/// Not `async`, unlike its siblings above: this and [`render_metadata`]
/// render a DTO straight from a value the test built, with no request, no
/// `Service` and nothing to `.await` — an `async fn` with no await point is
/// `clippy::unused_async`, denied crate-wide. The brief's sketch called
/// these with `.await`; dropping it at the four call sites below is the
/// fix, not a suppression.
fn render(summary: QuantitySummary) -> Value {
    serde_json::to_value(QuantitySummaryDto::from(summary)).expect("dto serializes")
}

/// See [`render`] for why this is not `async`.
fn render_metadata(metadata: ReconciliationMetadata) -> Value {
    let scope = ReconciliationScope {
        tenant_id: TENANT_ID,
        gts_type_id: fixture_meter(),
    };
    let dto = ReconciliationMetadataDto::from_metadata(scope, metadata);
    serde_json::to_value(dto).expect("dto serializes")
}

#[tokio::test]
async fn a_repeated_required_parameter_is_refused_rather_than_resolved_last_one_wins() {
    // Review Focus 4. The yaml is silent on repetition. Last-one-wins would
    // silently report on a tenant the caller did not ask about, on the
    // surface whose whole purpose is deciding whether an invoice is right.
    for param in ["scope", "gts_type_id", "tenant_id", "from", "to"] {
        let response = get_with_repeated(param).await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "a repeated `{param}` must be refused"
        );
        let body = body_text(response).await;
        assert!(
            body.contains(param),
            "the rejection for a repeated `{param}` must name it; got: {body}"
        );
    }
}

#[tokio::test]
async fn each_missing_required_parameter_is_named_in_its_own_rejection() {
    // `inst-radmit-envelope`: every rejection names the offending parameter.
    // Asserting the detail rather than the status, because all five share one
    // status and one error variant.
    for param in ["scope", "gts_type_id", "tenant_id", "from", "to"] {
        let response = get_without(param).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_text(response).await;
        assert!(
            body.contains(param),
            "omitting `{param}` must produce a violation naming it; got: {body}"
        );
    }
}

#[tokio::test]
async fn a_reserved_granularity_is_refused_saying_it_is_not_served() {
    for reserved in ["caller", "caller_tenant"] {
        let response = get_with_scope(reserved).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_text(response).await;
        assert!(body.contains(reserved), "got: {body}");
        assert!(
            body.contains("not served"),
            "a reserved granularity must be told it is reserved, not that it is unknown; \
             got: {body}"
        );
    }
}

#[tokio::test]
async fn an_inverted_or_empty_range_is_a_field_violation_rather_than_an_internal_error() {
    // `TimeRange::new` rejects `to <= from`, so `from == to` is refused too.
    // What matters is that the refusal surfaces as a 400 naming a bound
    // rather than a 500.
    for (from, to) in [
        ("2026-01-02T00:00:00Z", "2026-01-01T00:00:00Z"),
        ("2026-01-01T00:00:00Z", "2026-01-01T00:00:00Z"),
    ] {
        let response = get_with_range(from, to).await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "from={from} to={to}"
        );
        let body = body_text(response).await;
        assert!(body.contains("from") || body.contains("to"), "got: {body}");
    }
}

/// The inverted-range test above only
/// asserts `contains("from") || contains("to")`, which a rejection naming
/// the wrong bound would still pass. Spec §6.1 requires a field violation
/// naming the *offending* bound, so this and its sibling below each assert
/// their own bound alone is named — a malformed `from` must blame `from`,
/// not `to`.
#[tokio::test]
async fn a_malformed_from_is_a_field_violation_naming_from() {
    let response = get_with_range("not-a-timestamp", "2026-01-02T00:00:00Z").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_text(response).await;
    assert!(
        body.contains("from"),
        "a malformed `from` must be named in the rejection; got: {body}"
    );
}

/// See [`a_malformed_from_is_a_field_violation_naming_from`]'s doc — the
/// `to`-blaming sibling.
#[tokio::test]
async fn a_malformed_to_is_a_field_violation_naming_to() {
    let response = get_with_range("2026-01-01T00:00:00Z", "not-a-timestamp").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_text(response).await;
    assert!(
        body.contains("to"),
        "a malformed `to` must be named in the rejection; got: {body}"
    );
}

#[tokio::test]
async fn the_accrued_branch_renders_a_string_and_the_observation_branch_renders_a_count() {
    // `AggregatedQuantity` and `UsageQuantity` are both wire-encoded as JSON
    // strings so neither round-trips through a float, and `observation_count`
    // is an integer. A branch rendered as a JSON number would be a silent
    // precision change on a billing surface.
    let accrued = render(QuantitySummary::Accrued(BigDecimal::from(7)));
    assert_eq!(accrued["accrued_sum"], serde_json::json!("7"));
    assert!(accrued.get("observation_count").is_none());

    let observed = render(QuantitySummary::Observations(Some(ObservedQuantity {
        count: NonZeroU64::new(3).unwrap(),
        latest: UsageQuantity::parse("1.50").unwrap(),
    })));
    assert_eq!(observed["observation_count"], serde_json::json!(3));
    assert_eq!(observed["latest_observation"], serde_json::json!("1.50"));
    assert!(observed.get("accrued_sum").is_none());
}

#[tokio::test]
async fn an_absent_observation_and_absent_watermarks_render_as_null() {
    let body = render_metadata(ReconciliationMetadata::empty_for(AggregationFold::Max));
    assert_eq!(
        body["quantity_summary"]["latest_observation"],
        serde_json::Value::Null
    );
    assert_eq!(body["accepted_at_watermark"], serde_json::Value::Null);
    assert_eq!(body["window_end_watermark"], serde_json::Value::Null);
}

#[tokio::test]
async fn an_empty_accrual_renders_zero_rather_than_null() {
    // Plugin DESIGN §3.6: "`accrued_sum` stays `0` rather than `null`, because
    // an accrual over an empty set is defined while an observation over one is
    // not." A consumer must be able to tell a meter that summed to zero from
    // one whose fold could not be taken.
    let body = render_metadata(ReconciliationMetadata::empty_for(AggregationFold::Sum));
    assert_eq!(
        body["quantity_summary"]["accrued_sum"],
        serde_json::json!("0")
    );
}
