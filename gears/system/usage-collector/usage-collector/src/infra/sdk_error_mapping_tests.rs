//! Unit tests for the host-owned `UsageCollectorError` -> `CanonicalError` lift.
//!
//! Each test asserts the DESIGN §3.3 AIP-193 mapping (category -> HTTP status)
//! plus the module-specific `context.reason` / resource-type carry. The lift is
//! exposed as a single free fn
//! ([`usage_collector_error_to_canonical_for_usage_record`]) — the gear
//! registers only the ingestion REST surface now that types-registry owns
//! every type declaration, so there is no catalog-shaped lift entry point to
//! exercise any more.
//!
//! After the error-envelope compaction, the 503 `context.reason` triage codes
//! (`PLUGIN_READINESS` / `PLUGIN_TRANSIENT` / `AUTHZ_UNAVAILABLE`) and the
//! corrects-id `CORRECTS_ID_NOT_FOUND` 404 reason are no longer emitted — the
//! canonical `ServiceUnavailable` / `NotFound` contexts have no reason slot, so
//! those were batch-only JSON post-injections that the compaction removed.
//! Operator triage for 503s reads the curated `detail` string instead.

use toolkit_canonical_errors::{CanonicalError, Problem};
use toolkit_gts::gts_id;
use usage_collector_sdk::{MeterTypeId, USAGE_RECORD_RESOURCE, UsageCollectorError};
use uuid::Uuid;

use super::{
    UsageRecordResource, usage_collector_error_to_canonical_for_usage_record as lift_record,
    usage_record_error_to_problem,
};

fn sample_meter_id() -> MeterTypeId {
    MeterTypeId::new(gts_id!(
        "cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~"
    ))
    .expect("valid usage_record-derived meter type id")
}

#[test]
fn usage_record_resource_type_is_record_sibling() {
    let err: CanonicalError = UsageRecordResource::not_found("r")
        .with_resource("r")
        .create();
    assert_eq!(err.resource_type(), Some(USAGE_RECORD_RESOURCE));
}

#[test]
fn authorization_from_usage_record_surface_uses_usage_record_resource() {
    let c = lift_record(UsageCollectorError::permission_denied("denied"));
    assert_eq!(c.status_code(), 403);
    assert_eq!(c.title(), "Permission Denied");
    assert_eq!(c.resource_type(), Some(USAGE_RECORD_RESOURCE));
}

#[test]
fn authorization_carries_native_authz_reason() {
    let problem = Problem::from(lift_record(UsageCollectorError::permission_denied(
        "tenant scope mismatch",
    )));
    assert_eq!(problem.status, Some(403));
    assert_eq!(
        problem_context_string(&problem, "reason").as_deref(),
        Some("AUTHZ")
    );
}

#[test]
fn invalid_resource_ref_maps_to_400_invalid_argument() {
    let c = lift_record(UsageCollectorError::invalid_resource_ref(
        "resource_id must not be empty",
    ));
    assert_eq!(c.status_code(), 400);
    assert_eq!(c.title(), "Invalid Argument");
    assert_eq!(c.resource_type(), Some(USAGE_RECORD_RESOURCE));
}

#[test]
fn metadata_size_exceeded_maps_to_400_invalid_argument_with_usage_record_resource() {
    let c = lift_record(UsageCollectorError::metadata_size_exceeded(9000, 8192));
    assert_eq!(c.status_code(), 400);
    assert_eq!(c.title(), "Invalid Argument");
    assert_eq!(c.resource_type(), Some(USAGE_RECORD_RESOURCE));
}

#[test]
fn invalid_batch_size_maps_to_400_invalid_argument() {
    let c = lift_record(UsageCollectorError::invalid_batch_size(0, 1, 100));
    assert_eq!(c.status_code(), 400);
    assert_eq!(c.title(), "Invalid Argument");
    assert_eq!(c.resource_type(), Some(USAGE_RECORD_RESOURCE));
}

/// `UnknownMetadataKey` lifts onto `InvalidArgument` (HTTP 400) and identifies
/// the meter whose closed shape was violated (`resource_type` +
/// `resource.name = gts_type_id`), even though the failing operation is record
/// submission — the variant's intrinsic resource is the type it references.
#[test]
fn unknown_metadata_key_maps_to_invalid_argument() {
    let gts_type_id = sample_meter_id();
    let c = lift_record(UsageCollectorError::unknown_metadata_key(
        &gts_type_id,
        "unexpected",
    ));
    assert_eq!(c.status_code(), 400);
    assert_eq!(c.resource_type(), Some(USAGE_RECORD_RESOURCE));
    assert_eq!(c.resource_name(), Some(gts_type_id.as_ref()));
}

#[test]
fn usage_record_not_found_maps_to_404() {
    let id = Uuid::from_u128(0x1234);
    let c = lift_record(UsageCollectorError::usage_record_not_found(id));
    assert_eq!(c.status_code(), 404);
    assert_eq!(c.resource_type(), Some(USAGE_RECORD_RESOURCE));
}

#[test]
fn declaration_not_found_maps_to_404_naming_the_gts_type_id() {
    // Pins DESIGN §3.3: an unresolvable GTS type is a 404 naming the
    // identifier. Exercises the full chain from `domain::DomainError` (task
    // 4's `DeclarationNotFound`) through the `UsageCollectorError` bridge
    // (`domain/error.rs`) to this crate's `CanonicalError` lift, rather than
    // hand-building the intermediate `UsageCollectorError::NotFound`.
    //
    // `resource_type` is `USAGE_RECORD_RESOURCE`: a `gts_type_id` derives from
    // the usage_record base and this gear declares no other GTS resource on
    // its wire surface now that types-registry owns the catalog.
    let id = usage_collector_sdk::MeterTypeId::new(
        "gts.cf.core.uc.usage_record.v1~example.metering._.stored_volume.v1~",
    )
    .expect("valid meter type id");
    let domain_err = crate::domain::DomainError::declaration_not_found(&id);
    let c = lift_record(UsageCollectorError::from(domain_err));
    assert_eq!(c.status_code(), 404);
    assert_eq!(c.resource_type(), Some(USAGE_RECORD_RESOURCE));
    assert_eq!(c.resource_name(), Some(id.as_str()));
}

#[test]
fn already_inactive_maps_to_409_aborted_with_already_inactive_reason() {
    let id = Uuid::from_u128(0xCAFE_BABE);
    let c = lift_record(UsageCollectorError::already_inactive(id));
    assert_eq!(c.status_code(), 409);
    assert_eq!(c.resource_type(), Some(USAGE_RECORD_RESOURCE));
    assert_eq!(c.resource_name(), Some(id.to_string().as_str()));

    let problem = Problem::from(c);
    assert_eq!(
        problem_context_string(&problem, "reason").as_deref(),
        Some("ALREADY_INACTIVE"),
        "AlreadyInactive MUST carry context.reason=ALREADY_INACTIVE per the feature spec",
    );
}

#[test]
fn idempotency_conflict_maps_to_409_aborted() {
    let existing_id = Uuid::from_u128(0xAABB);
    let c = lift_record(UsageCollectorError::idempotency_conflict(
        "key-1",
        existing_id,
    ));
    assert_eq!(c.status_code(), 409);
    assert_eq!(c.resource_type(), Some(USAGE_RECORD_RESOURCE));
    assert_eq!(c.resource_name(), Some(existing_id.to_string().as_str()));
}

#[test]
fn negative_counter_value_carries_semantics_violation_reason() {
    use rust_decimal::Decimal;
    let c = lift_record(UsageCollectorError::negative_counter_value(Decimal::from(
        -1,
    )));
    assert_eq!(c.status_code(), 400);
    let problem = Problem::from(c);
    assert_eq!(
        first_field_violation_string(&problem, "reason").as_deref(),
        Some("SEMANTICS_VIOLATION"),
        "NegativeCounterValue routes to the SEMANTICS_VIOLATION wire reason",
    );
}

#[test]
fn non_negative_counter_compensation_carries_semantics_violation_reason() {
    use rust_decimal::Decimal;
    let c = lift_record(UsageCollectorError::non_negative_counter_compensation(
        Decimal::ZERO,
    ));
    assert_eq!(c.status_code(), 400);
    let problem = Problem::from(c);
    assert_eq!(
        first_field_violation_string(&problem, "reason").as_deref(),
        Some("SEMANTICS_VIOLATION"),
        "NonNegativeCounterCompensation routes to the SEMANTICS_VIOLATION wire reason",
    );
}

#[test]
fn corrects_id_not_found_maps_to_404_without_reason() {
    // Post-injection removed: corrects-id-not-found now collapses into the
    // plain record `NotFound` (404). The `CORRECTS_ID_NOT_FOUND` machine
    // reason is no longer emitted (it was batch-only); the `detail` text
    // preserves the human distinction.
    let corrects_id = Uuid::from_u128(0xDEAD_BEEF);
    let problem =
        usage_record_error_to_problem(UsageCollectorError::corrects_id_not_found(corrects_id));
    assert_eq!(problem.status, Some(404));
    assert_eq!(problem_context_string(&problem, "reason"), None);
}

#[test]
fn plugin_unavailable_per_record_problem_is_503_without_reason() {
    let problem = usage_record_error_to_problem(UsageCollectorError::plugin_unavailable());
    assert_eq!(problem.status, Some(503));
    assert_eq!(problem_context_string(&problem, "reason"), None);
}

#[test]
fn service_unavailable_per_record_problem_is_503_without_reason() {
    let problem = usage_record_error_to_problem(UsageCollectorError::service_unavailable(
        "downstream connection reset",
        None,
    ));
    assert_eq!(problem.status, Some(503));
    assert_eq!(problem_context_string(&problem, "reason"), None);
}

#[test]
fn types_registry_unavailable_per_record_problem_is_503() {
    let problem = usage_record_error_to_problem(UsageCollectorError::types_registry_unavailable());
    assert_eq!(problem.status, Some(503));
}

#[test]
fn corrects_id_targets_compensation_maps_to_409_aborted_with_reason() {
    let corrects_id = Uuid::from_u128(7);
    let c = lift_record(UsageCollectorError::corrects_id_targets_compensation(
        corrects_id,
    ));
    assert_eq!(c.status_code(), 409);
    assert_eq!(c.resource_type(), Some(USAGE_RECORD_RESOURCE));
    let problem = Problem::from(c);
    assert_eq!(
        problem_context_string(&problem, "reason").as_deref(),
        Some("CORRECTS_ID_TARGETS_COMPENSATION"),
    );
}

#[test]
fn corrects_id_wrong_scope_maps_to_409_aborted_with_reason() {
    let corrects_id = Uuid::from_u128(8);
    let c = lift_record(UsageCollectorError::corrects_id_wrong_scope(corrects_id));
    assert_eq!(c.status_code(), 409);
    let problem = Problem::from(c);
    assert_eq!(
        problem_context_string(&problem, "reason").as_deref(),
        Some("CORRECTS_ID_WRONG_SCOPE"),
    );
}

#[test]
fn corrects_id_inactive_maps_to_409_aborted_with_reason() {
    let corrects_id = Uuid::from_u128(9);
    let c = lift_record(UsageCollectorError::corrects_id_inactive(corrects_id));
    assert_eq!(c.status_code(), 409);
    let problem = Problem::from(c);
    assert_eq!(
        problem_context_string(&problem, "reason").as_deref(),
        Some("CORRECTS_ID_INACTIVE"),
    );
}

#[test]
fn plugin_unavailable_maps_to_503() {
    let c = lift_record(UsageCollectorError::plugin_unavailable());
    assert_eq!(c.status_code(), 503);
}

#[test]
fn service_unavailable_maps_to_503() {
    let c = lift_record(UsageCollectorError::service_unavailable(
        "transient",
        Some(30),
    ));
    assert_eq!(c.status_code(), 503);
}

#[test]
fn types_registry_unavailable_maps_to_503() {
    let c = lift_record(UsageCollectorError::types_registry_unavailable());
    assert_eq!(c.status_code(), 503);
}

#[test]
fn internal_maps_to_500_and_carries_diagnostic() {
    let c = lift_record(UsageCollectorError::internal("secret diag"));
    assert_eq!(c.status_code(), 500);
    assert_eq!(c.diagnostic(), Some("secret diag"));
}

// ---------------------------------------------------------------------------
// Register-flow Problem envelope tests.
// ---------------------------------------------------------------------------

fn problem_context_string(problem: &Problem, key: &str) -> Option<String> {
    problem
        .context
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn first_field_violation_string(problem: &Problem, key: &str) -> Option<String> {
    problem
        .context
        .get("field_violations")
        .and_then(serde_json::Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|v| v.get(key))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

#[test]
fn invalid_metadata_key_uses_usage_record_resource() {
    let c = lift_record(UsageCollectorError::invalid_metadata_key(
        "metadata key must not be empty",
    ));
    assert_eq!(c.status_code(), 400);
    assert_eq!(c.resource_type(), Some(USAGE_RECORD_RESOURCE));
}

// ---------------------------------------------------------------------------
// Exhaustiveness fence: drive every variant that can fire on the (sole
// remaining) ingestion surface through its lift and assert an in-range
// status. A future variant added without a corresponding `lift_common` arm
// trips the `debug_assert!`.
// ---------------------------------------------------------------------------

fn every_usage_record_surface_variant() -> Vec<UsageCollectorError> {
    use rust_decimal::Decimal;

    let gts_type_id = sample_meter_id();
    let uuid = Uuid::new_v4();
    vec![
        UsageCollectorError::permission_denied("denied"),
        UsageCollectorError::negative_counter_value(Decimal::from(-1)),
        UsageCollectorError::non_negative_counter_compensation(Decimal::ZERO),
        UsageCollectorError::invalid_batch_size(0, 1, 100),
        UsageCollectorError::metadata_size_exceeded(9000, 8192),
        UsageCollectorError::invalid_metadata_key("r"),
        UsageCollectorError::invalid_metadata_filter("r"),
        UsageCollectorError::invalid_resource_ref("r"),
        UsageCollectorError::invalid_subject_ref("r"),
        UsageCollectorError::invalid_idempotency_key("r"),
        UsageCollectorError::unknown_metadata_key(&gts_type_id, "k"),
        UsageCollectorError::usage_record_not_found(uuid),
        UsageCollectorError::already_inactive(uuid),
        UsageCollectorError::idempotency_conflict("idem-fence", uuid),
        UsageCollectorError::corrects_id_not_found(uuid),
        UsageCollectorError::corrects_id_targets_compensation(uuid),
        UsageCollectorError::corrects_id_wrong_scope(uuid),
        UsageCollectorError::corrects_id_inactive(uuid),
        UsageCollectorError::plugin_unavailable(),
        UsageCollectorError::types_registry_unavailable(),
        UsageCollectorError::service_unavailable("x", None),
        UsageCollectorError::internal("x"),
    ]
}

#[test]
fn lift_record_covers_every_usage_record_surface_variant() {
    for err in every_usage_record_surface_variant() {
        let label = format!("{err:?}");
        let problem = Problem::from(lift_record(err));
        assert!(
            (400..=599).contains(&problem.status.unwrap_or(500)),
            "lift_record({label}) produced an out-of-range status {:?}",
            problem.status,
        );
    }
}
