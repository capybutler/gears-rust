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
//! (`PLUGIN_READINESS` / `PLUGIN_TRANSIENT` / `AUTHZ_UNAVAILABLE`) and every
//! 404 `context.reason` are no longer emitted — the canonical
//! `ServiceUnavailable` / `NotFound` contexts have no reason slot, so those
//! were batch-only JSON post-injections that the compaction removed. Operator
//! triage for 503s reads the curated `detail` string instead.

use toolkit_canonical_errors::{CanonicalError, FieldViolation, InvalidArgument, Problem};
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
fn already_invalidated_maps_to_409_aborted_with_already_invalidated_reason() {
    // The store's at-most-one-invalidation rejection
    // (`cpt-cf-usage-collector-adr-append-only-invalidation`) names the
    // *target* on `resource.name`, so a caller can look the pair up, and
    // rides its typed reason on `context.reason`.
    let target = Uuid::from_u128(0xCAFE_BABE);
    let invalidated_by = Uuid::from_u128(0xFEED);
    let c = lift_record(UsageCollectorError::already_invalidated(
        target,
        invalidated_by,
    ));
    assert_eq!(c.status_code(), 409);
    assert_eq!(c.resource_type(), Some(USAGE_RECORD_RESOURCE));
    assert_eq!(c.resource_name(), Some(target.to_string().as_str()));

    let problem = Problem::from(c);
    assert_eq!(
        problem_context_string(&problem, "reason").as_deref(),
        Some("ALREADY_INVALIDATED"),
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
fn invalidation_target_not_found_maps_to_404_naming_the_target_uuid() {
    // An unresolvable `invalidates` collapses into the plain record
    // `NotFound` (404), which carries no machine reason — the canonical
    // `NotFound` context has no reason slot, so the `detail` text carries
    // the human distinction and `resource.name` carries the target uuid.
    // The uuid on `name` is load-bearing beyond diagnostics: the service's
    // `classify_record_error` tells this apart from an unresolvable meter
    // by whether `name` parses as a `Uuid`.
    let target = Uuid::from_u128(0xDEAD_BEEF);
    assert_eq!(
        lift_record(UsageCollectorError::invalidation_target_not_found(target)).resource_name(),
        Some(target.to_string().as_str()),
        "the target uuid on `name` is what `classify_record_error` parses",
    );
    let problem =
        usage_record_error_to_problem(UsageCollectorError::invalidation_target_not_found(target));
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
fn invalidation_target_not_record_maps_to_400_attributing_the_reference_field() {
    let target = Uuid::from_u128(7);
    let c = lift_record(UsageCollectorError::invalidation_target_not_record(target));
    assert_eq!(c.status_code(), 400);
    assert_eq!(c.resource_type(), Some(USAGE_RECORD_RESOURCE));
    let problem = Problem::from(c);
    assert_eq!(
        first_field_violation_string(&problem, "reason").as_deref(),
        Some("INVALIDATION_TARGET_NOT_RECORD"),
    );
    assert_eq!(
        first_field_violation_string(&problem, "field").as_deref(),
        Some("invalidates"),
        "an unwithdrawable target is attributed to the reference that named it",
    );
    assert_eq!(
        lift_record(UsageCollectorError::invalidation_target_not_record(target)).resource_name(),
        Some(target.to_string().as_str()),
        "the target uuid is the only identifier a submission-time rejection has \
         to name: the submitted entry has no id yet",
    );
}

#[test]
fn invalidation_field_mismatch_attributes_the_field_that_differs() {
    // The diagnostic's whole value is naming *which* field departed from
    // the target: an invalidation is a faithful copy in every
    // caller-supplied field, so a rejection that only said "mismatch"
    // would leave the emitter diffing two payloads by hand. Pin that the
    // caller-supplied field name reaches `field_violations[0].field`
    // rather than a constant.
    let target = Uuid::from_u128(8);
    let c = lift_record(UsageCollectorError::invalidation_field_mismatch(
        "value", target,
    ));
    assert_eq!(c.status_code(), 400);
    let problem = Problem::from(c);
    assert_eq!(
        first_field_violation_string(&problem, "reason").as_deref(),
        Some("INVALIDATION_FIELD_MISMATCH"),
    );
    assert_eq!(
        first_field_violation_string(&problem, "field").as_deref(),
        Some("value"),
    );
    assert_eq!(
        lift_record(UsageCollectorError::invalidation_field_mismatch(
            "value", target
        ))
        .resource_name(),
        Some(target.to_string().as_str()),
        "the target uuid is the only identifier a submission-time rejection has \
         to name: the submitted entry has no id yet",
    );
}

#[test]
fn invalidation_reference_incomplete_attributes_the_missing_half() {
    // Both-or-neither is rejected at the REST fold point naming the half
    // the caller has to add — the domain carries the pair as one
    // `Invalidation`, so nothing downstream can raise this.
    let c = lift_record(UsageCollectorError::invalidation_reference_incomplete(
        "reason_code",
    ));
    assert_eq!(c.status_code(), 400);
    let problem = Problem::from(c);
    assert_eq!(
        first_field_violation_string(&problem, "reason").as_deref(),
        Some("INVALIDATION_REFERENCE_INCOMPLETE"),
    );
    assert_eq!(
        first_field_violation_string(&problem, "field").as_deref(),
        Some("reason_code"),
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
    let gts_type_id = sample_meter_id();
    let uuid = Uuid::new_v4();
    vec![
        UsageCollectorError::permission_denied("denied"),
        UsageCollectorError::invalid_batch_size(0, 1, 100),
        UsageCollectorError::metadata_size_exceeded(9000, 8192),
        UsageCollectorError::invalid_metadata_key("r"),
        UsageCollectorError::invalid_metadata_filter("r"),
        UsageCollectorError::invalid_resource_ref("r"),
        UsageCollectorError::invalid_subject_ref("r"),
        UsageCollectorError::invalid_idempotency_key("r"),
        UsageCollectorError::unknown_metadata_key(&gts_type_id, "k"),
        UsageCollectorError::usage_record_not_found(uuid),
        UsageCollectorError::idempotency_conflict("idem-fence", uuid),
        UsageCollectorError::invalidation_reference_incomplete("reason_code"),
        UsageCollectorError::invalidation_target_not_found(uuid),
        UsageCollectorError::invalidation_target_not_record(uuid),
        UsageCollectorError::invalidation_field_mismatch("value", uuid),
        UsageCollectorError::already_invalidated(uuid, Uuid::new_v4()),
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

/// The single `field_violations` entry on an `InvalidArgument` canonical
/// error. Panics loudly on any other shape — a cursor rejection that stopped
/// being a field violation is the thing under test, not a reason to skip.
fn first_field_violation(err: &CanonicalError) -> &FieldViolation {
    match err {
        CanonicalError::InvalidArgument {
            ctx: InvalidArgument::FieldViolations { field_violations },
            ..
        } => field_violations
            .first()
            .expect("a cursor rejection carries one field violation"),
        other => panic!("expected an InvalidArgument field violation, got {other:?}"),
    }
}

/// The wire code on a cursor rejection is `toolkit_odata`'s, read from
/// `toolkit_odata`.
///
/// Spec §3.13 gives the cursor codes to `toolkit_odata` because a second
/// declaration is a second place the same code can be read and disagree.
/// Asserting against a `"INVALID_CURSOR"` literal here would *be* that
/// second place — the test would keep passing if upstream renamed the code,
/// which is the exact failure the rule exists to prevent. So both sides of
/// this assertion come from upstream, and the gear's side has to travel
/// through the gear's own lift to get there.
#[test]
fn a_cursor_rejection_carries_the_upstream_wire_code() {
    for (upstream, gear) in [
        (
            toolkit_odata::Error::InvalidCursor,
            UsageCollectorError::inadmissible_cursor_keyset("mixed directions"),
        ),
        (
            toolkit_odata::Error::FilterMismatch,
            UsageCollectorError::cursor_query_mismatch(),
        ),
    ] {
        let expected_owned = CanonicalError::from(upstream);
        let expected = first_field_violation(&expected_owned);
        let actual_owned = lift_record(gear);
        let actual = first_field_violation(&actual_owned);

        assert_eq!(
            actual.reason, expected.reason,
            "the gear must surface `toolkit_odata`'s reason code verbatim",
        );
        assert_eq!(
            actual.field, expected.field,
            "the gear must attribute to the same request field as upstream",
        );

        // The code and the field are upstream's; the resource scope is
        // not, and the projection would surrender it by default —
        // `toolkit_odata` scopes its own `InvalidArgument` to
        // `cf.core.odata.query.v1~`. `resource_type` is the "which entity"
        // discrimination layer `docs/usage-collector-v1.yaml` documents,
        // and the entity here is a usage record whichever crate detected
        // the defect. Asserted as a positive value: "not upstream's" would
        // pass against `None`.
        assert_eq!(
            actual_owned.resource_type(),
            Some(USAGE_RECORD_RESOURCE),
            "a cursor rejection must keep the gear's resource identity, not \
             inherit upstream's",
        );
        assert_ne!(
            actual_owned.resource_type(),
            expected_owned.resource_type(),
            "this assertion is only meaningful while upstream scopes to a \
             different resource; if upstream's scope changed, re-derive what \
             the gear should advertise rather than deleting this",
        );
    }
}

/// The gear's own caller guidance survives the projection.
///
/// The point of carrying `detail` alongside the upstream error is that a
/// caller is told how to recover; upstream's description is "invalid
/// cursor". A positive anchor, not just a `!=`: an assertion that the
/// description merely *differs* from upstream's would pass against an
/// empty string.
#[test]
fn a_cursor_rejection_keeps_the_gear_s_recovery_guidance() {
    let lifted = lift_record(UsageCollectorError::inadmissible_cursor_keyset(
        "it carries no cursor",
    ));
    let violation = first_field_violation(&lifted);

    assert!(
        violation.description.contains("it carries no cursor"),
        "the defect must reach the caller: got {:?}",
        violation.description,
    );
    assert!(
        violation
            .description
            .contains("restart pagination without a cursor"),
        "the recovery must reach the caller: got {:?}",
        violation.description,
    );
}
