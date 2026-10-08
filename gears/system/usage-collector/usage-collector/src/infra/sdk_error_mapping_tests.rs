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

use toolkit_canonical_errors::{
    CanonicalError, FieldViolation, InvalidArgument as InvalidArgumentCtx, Problem,
};
use toolkit_gts::gts_id;
use usage_collector_sdk::{
    AlreadyInvalidatedArgs, IngestionQuotaExceededArgs, MeterTypeId, USAGE_RECORD_RESOURCE,
    UsageCollectorError,
};
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
fn already_invalidated_problem_names_the_invalidation_in_place_and_its_reason_code() {
    let target = Uuid::from_u128(0xCAFE_BABE);
    let invalidated_by = Uuid::from_u128(0xFEED);
    let already = || {
        UsageCollectorError::already_invalidated(AlreadyInvalidatedArgs {
            target,
            invalidated_by,
            reason_code: usage_collector_sdk::ReasonCode::new("emitter_defect")
                .expect("valid reason code"),
        })
    };

    let c = lift_record(already());
    assert_eq!(c.status_code(), 409);
    assert_eq!(c.resource_type(), Some(USAGE_RECORD_RESOURCE));
    assert_eq!(c.resource_name(), Some(target.to_string().as_str()));

    let problem = usage_record_error_to_problem(already());
    assert_eq!(
        problem_context_string(&problem, "reason").as_deref(),
        Some("ALREADY_INVALIDATED"),
    );
    assert_eq!(
        problem_context_string(&problem, "invalidated_by").as_deref(),
        Some(invalidated_by.to_string().as_str()),
    );
    assert_eq!(
        problem_context_string(&problem, "reason_code").as_deref(),
        Some("emitter_defect"),
    );
    assert_eq!(problem.context.get("retryable"), None, "not retryable");
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
    // A target identifier that resolves to nothing collapses into the plain
    // record `NotFound` (404). The identifier is derived from the
    // withdrawal's own fields, not sent (DESIGN §3.1, Target resolution),
    // so the 404 reports a derivation that matched no stored entry. The
    // variant now carries a typed `NotFoundReason`, but the canonical 404
    // context has no reason slot, so the lift drops it deliberately and
    // nothing machine-readable distinguishes the three 404 cases on the
    // wire — the `detail` text carries the human distinction and
    // `resource.name` carries the target uuid. That the reason is absent
    // from the `Problem` is asserted below.
    let target = Uuid::from_u128(0xDEAD_BEEF);
    assert_eq!(
        lift_record(UsageCollectorError::invalidation_target_not_found(target)).resource_name(),
        Some(target.to_string().as_str()),
        "the 404 names the derived target uuid on `resource.name`",
    );
    let problem =
        usage_record_error_to_problem(UsageCollectorError::invalidation_target_not_found(target));
    assert_eq!(problem.status, Some(404));
    assert_eq!(problem_context_string(&problem, "reason"), None);
}

#[test]
fn plugin_unavailable_per_record_problem_is_503_without_reason() {
    let problem = usage_record_error_to_problem(UsageCollectorError::plugin_unavailable(None));
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
    let problem =
        usage_record_error_to_problem(UsageCollectorError::types_registry_unavailable(None));
    assert_eq!(problem.status, Some(503));
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
        "quantity", target,
    ));
    assert_eq!(c.status_code(), 400);
    let problem = Problem::from(c);
    assert_eq!(
        first_field_violation_string(&problem, "reason").as_deref(),
        Some("INVALIDATION_FIELD_MISMATCH"),
    );
    assert_eq!(
        first_field_violation_string(&problem, "field").as_deref(),
        Some("quantity"),
    );
    assert_eq!(
        lift_record(UsageCollectorError::invalidation_field_mismatch(
            "quantity", target
        ))
        .resource_name(),
        Some(target.to_string().as_str()),
        "the target uuid is the only identifier a submission-time rejection has \
         to name: the submitted entry has no id yet",
    );
}

#[test]
fn plugin_unavailable_maps_to_503() {
    let c = lift_record(UsageCollectorError::plugin_unavailable(None));
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
    let c = lift_record(UsageCollectorError::types_registry_unavailable(None));
    assert_eq!(c.status_code(), 503);
}

#[test]
fn internal_maps_to_500_and_carries_diagnostic() {
    let c = lift_record(UsageCollectorError::internal("secret diag"));
    assert_eq!(c.status_code(), 500);
    assert_eq!(c.diagnostic(), Some("secret diag"));
}

// Register-flow Problem envelope tests.

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

// Exhaustiveness fence: drive every variant that can fire on the (sole
// remaining) ingestion surface through its lift and assert an in-range
// status. A future variant added without a corresponding `lift_common` arm
// trips the `debug_assert!`.

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
        // The two cursor rejections. They are 400s like every other entry,
        // but the only ones whose `field` and `reason` are read off a
        // `toolkit_odata` error instead of spelled here, so the fence is
        // also what catches upstream's mapping ceasing to produce the
        // single field violation the lift projects.
        UsageCollectorError::inadmissible_cursor_keyset("mixed directions"),
        UsageCollectorError::cursor_query_mismatch(),
        UsageCollectorError::usage_record_not_found(uuid),
        UsageCollectorError::idempotency_conflict("idem-fence", uuid),
        UsageCollectorError::invalidation_target_not_found(uuid),
        UsageCollectorError::invalidation_field_mismatch("quantity", uuid),
        UsageCollectorError::already_invalidated(AlreadyInvalidatedArgs {
            target: uuid,
            invalidated_by: Uuid::new_v4(),
            reason_code: usage_collector_sdk::ReasonCode::new("emitter_defect")
                .expect("valid reason code"),
        }),
        UsageCollectorError::target_not_converged(uuid, Some(1)),
        UsageCollectorError::ingestion_quota_exceeded(IngestionQuotaExceededArgs {
            allowance: 4000,
            submitted: 5000,
            retry_after_seconds: 7,
        }),
        UsageCollectorError::plugin_unavailable(Some(1)),
        UsageCollectorError::types_registry_unavailable(Some(1)),
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
            ctx: InvalidArgumentCtx::FieldViolations { field_violations },
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

/// A quota rejection is a 429 whose retry delay rides
/// `context.violations[0].retry_after_seconds` — the slot the `context`
/// property of `docs/usage-collector-v1.yaml`'s `Problem` schema names.
///
/// Red before the `ResourceExhausted` lift arm existed:
/// `lift_common`'s catch-all `other` arm trips
/// `debug_assert!(false, "lift_common missing arm for variant: {other:?}")`,
/// observed by running
/// `cargo test -p cf-gears-usage-collector --lib -- the_quota_problem_carries_its_retry_delay --nocapture`
/// before Step 6's match arm was added, which panics the test rather than
/// merely failing an assertion (debug assertions are enabled in test
/// builds).
///
/// One source value lands in two body fields (`detail` and
/// `violations[0].description`) plus the retry slot, so this also pins the
/// detail assertion (not a variant assertion, spec §7 / §15.4 rule 3): an
/// operator reading one envelope must be able to tell a burst from a
/// sustained overrun.
#[test]
fn the_quota_problem_carries_its_retry_delay_on_the_violations_slot_the_contract_names() {
    let problem = usage_record_error_to_problem(UsageCollectorError::ingestion_quota_exceeded(
        IngestionQuotaExceededArgs {
            allowance: 4000,
            submitted: 5000,
            retry_after_seconds: 7,
        },
    ));
    assert_eq!(problem.status, Some(429));
    assert!(
        problem.detail.contains("4000") && problem.detail.contains("5000"),
        "the allowance and the submitted count must both reach the wire detail: {:?}",
        problem.detail,
    );

    let violations = problem
        .context
        .get("violations")
        .and_then(serde_json::Value::as_array)
        .expect("a quota rejection carries a `violations` array");
    assert_eq!(violations.len(), 1);
    assert_eq!(
        violations[0].get("retry_after_seconds"),
        Some(&serde_json::Value::from(7)),
        "the `context` property of docs/usage-collector-v1.yaml's `Problem` schema \
         names `violations[0].retry_after_seconds` as the body slot for the quota \
         rejection's retry delay; got {:?}",
        problem.context,
    );
    assert_eq!(
        violations[0].get("subject"),
        Some(&serde_json::Value::String("ingestion_quota".to_owned())),
        "DESIGN \u{a7}3.3 Error Contract: the violation's subject is a fixed value \
         naming the ingestion quota",
    );
}

/// Pins a fact about **`ToolKit`'s shared-library mapping**, not about this
/// gear's end-to-end header delivery: `toolkit_canonical_errors::Problem`'s
/// generic `IntoResponse` derives the `Retry-After` header for the
/// `service_unavailable` category alone
/// (`toolkit-canonical-errors/src/problem.rs`'s
/// `service_unavailable_retry_after_seconds`, gated on
/// `category_from_problem_type(..) == Some("service_unavailable")`), and
/// `toolkit-canonical-errors/src/context.rs`'s `QuotaViolationV1` doc comment
/// states the same thing as policy: "the wire `Retry-After` header is for
/// `ServiceUnavailable`, not `ResourceExhausted`". That is a stable,
/// shared-library fact, independent of what this gear does.
///
/// **This test does NOT, and cannot, pin whether a real `429` response this
/// gear sends ever carries the header.** The gear does send it: the fix
/// lives at the REST handler layer, in
/// `api/rest/handlers/usage_records.rs`'s
/// `lift_whole_request_ingestion_error`, which sets the header on the
/// `axum::response::Response` *after* `Problem::into_response()` returns,
/// mirroring `oagw`'s `api/rest/error.rs::error_response`. That is a code
/// path this test, calling `Problem::into_response()` directly, never
/// reaches and never will. So do not read a green run of this exact
/// test as "the gear doesn't send the header": that question requires
/// a test against the REST handler/router, not this one —
/// `usage_records_tests.rs`'s
/// `the_429_carries_a_retry_after_header_equal_to_the_body_s_retry_delay`
/// carries it. This test only
/// stays meaningful as a pin on `ToolKit`'s own generic mapping; it would
/// need to be deleted (not flipped) if `ToolKit` ever changes that mapping.
///
/// Red would require `ToolKit`'s shared `IntoResponse` to already derive the
/// header for `resource_exhausted` — it measurably does not, confirmed by
/// printing the header off a real `Response` built from this exact
/// `Problem` (Task 2's report carries the `None` observed this way).
#[test]
fn toolkits_shared_problem_into_response_does_not_derive_retry_after_for_resource_exhausted() {
    use axum::response::IntoResponse;

    let problem = usage_record_error_to_problem(UsageCollectorError::ingestion_quota_exceeded(
        IngestionQuotaExceededArgs {
            allowance: 4000,
            submitted: 5000,
            retry_after_seconds: 7,
        },
    ));
    let response = problem.into_response();
    assert_eq!(
        response.headers().get(axum::http::header::RETRY_AFTER),
        None,
        "toolkit_canonical_errors::Problem's generic IntoResponse derives \
         Retry-After for the service_unavailable category only; this is a \
         fact about that shared library, not about whether this gear's real \
         REST handler ever sets the header (it does, via different code: \
         api/rest/handlers/usage_records.rs's \
         lift_whole_request_ingestion_error) \u{2014} see this test's doc \
         comment before touching it",
    );
}

#[test]
fn target_not_converged_problem_is_409_and_marked_retryable() {
    let target = Uuid::from_u128(0xC2);
    let problem =
        usage_record_error_to_problem(UsageCollectorError::target_not_converged(target, None));
    assert_eq!(problem.status, Some(409));
    assert_eq!(
        problem_context_string(&problem, "reason").as_deref(),
        Some("TARGET_NOT_CONVERGED"),
    );
    assert_eq!(
        problem.context.get("retryable"),
        Some(&serde_json::Value::Bool(true))
    );
    assert_eq!(
        problem.context.get("retry_after_seconds"),
        None,
        "no delay was supplied at construction, so none should appear on the wire"
    );
}

/// `conflict_context_extras` stamps the configured
/// `target_not_converged_retry_after_secs` delay onto the wire
/// `context.retry_after_seconds`, alongside `context.retryable = true`
/// (DESIGN §3.8, §3.3 Error Contract: "the gear stamps
/// `target_not_converged_retry_after_secs` ... which a deployment sets from
/// that published sum"). The delay is already resolved by the time the
/// `UsageCollectorError` reaches this lift — it is host-supplied config,
/// applied at `domain::error::lift_domain_error` — so this test supplies it
/// directly at construction, the same way the host does.
#[test]
fn target_not_converged_problem_carries_the_supplied_retry_delay() {
    let target = Uuid::from_u128(0xC3);
    let problem =
        usage_record_error_to_problem(UsageCollectorError::target_not_converged(target, Some(5)));
    assert_eq!(problem.status, Some(409));
    assert_eq!(
        problem.context.get("retryable"),
        Some(&serde_json::Value::Bool(true))
    );
    assert_eq!(
        problem.context.get("retry_after_seconds"),
        Some(&serde_json::Value::from(5_u64))
    );
}

#[test]
fn an_idempotency_conflict_problem_carries_no_retryable_hint() {
    let problem = usage_record_error_to_problem(UsageCollectorError::idempotency_conflict(
        "k",
        Uuid::from_u128(1),
    ));
    assert_eq!(problem.context.get("retryable"), None);
}

/// An `until` defect is reported on `until`, not on `cursor`.
///
/// `usage-collector-v1.yaml`'s `Until` parameter and DESIGN §3.3 both
/// require a defect in the feed's replay bound to be attributed to its own
/// parameter. Before `CursorField`, `until_query_mismatch` did not exist and
/// every `CursorRejected` field violation named `cursor` regardless of which
/// of the two cursor-bearing parameters was actually rejected.
#[test]
fn an_until_defect_reports_its_violation_on_until_not_on_cursor() {
    let err = UsageCollectorError::until_query_mismatch();
    let canonical = lift_record(err);
    let violations = super::upstream_field_violations(&canonical)
        .expect("a cursor rejection lifts to an InvalidArgument with field violations");
    assert_eq!(violations.len(), 1);
    assert_eq!(
        violations[0].field, "until",
        "usage-collector-v1.yaml's `Until` parameter and DESIGN section 3.3 both require a \
         defect in the replay bound to be reported on `until`; reporting it on `cursor` \
         tells a caller to fix the wrong parameter"
    );
    assert_eq!(
        violations[0].reason, "FILTER_MISMATCH",
        "the REASON still comes from the upstream toolkit_odata error - `until` is refused \
         on the same terms as `cursor`, which is what the yaml requires; only the FIELD is \
         the gear's to choose"
    );
}
