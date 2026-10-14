//! Host-crate lift from [`UsageCollectorError`] to canonical REST errors.

use toolkit_canonical_errors::{
    CanonicalError, FieldViolation, InvalidArgument as InvalidArgumentCtx, Problem, resource_error,
};
use usage_collector_sdk::v2::{ConflictOutcome, USAGE_RECORD_RESOURCE, UsageCollectorError};

#[resource_error(gts_id!("cf.core.uc.usage_record.v2~"))]
pub(crate) struct UsageRecordResource;

/// Fixed `violations[0].subject` for quota rejections.
const INGESTION_QUOTA_SUBJECT: &str = "ingestion_quota";

/// Lift the SDK error envelope onto the canonical REST shape.
#[must_use]
pub(crate) fn usage_collector_error_to_canonical_for_usage_record(
    err: UsageCollectorError,
) -> CanonicalError {
    match err {
        // @cpt-begin:cpt-cf-usage-collector-fr-ingestion-authorization:p1:inst-dod-authz-deny
        // @cpt-begin:cpt-cf-usage-collector-principle-fail-closed:p1:inst-dod-fail-closed-authz
        UsageCollectorError::PermissionDenied { detail } => authz_denial(&detail),
        // @cpt-end:cpt-cf-usage-collector-principle-fail-closed:p1:inst-dod-fail-closed-authz
        // @cpt-end:cpt-cf-usage-collector-fr-ingestion-authorization:p1:inst-dod-authz-deny
        other => lift_common(other),
    }
}

/// Fallback for an unexpected SDK `resource_type`.
fn unrecognized_resource(resource_type: &str) -> CanonicalError {
    debug_assert!(
        false,
        "unrecognized resource_type on SDK error: {resource_type}"
    );
    CanonicalError::internal(format!("unrecognized resource_type: {resource_type}")).create()
}

/// The `field_violations` a canonical `InvalidArgument` carries.
fn upstream_field_violations(err: &CanonicalError) -> Option<&[FieldViolation]> {
    match err {
        CanonicalError::InvalidArgument {
            ctx: InvalidArgumentCtx::FieldViolations { field_violations },
            ..
        } => Some(field_violations),
        _ => None,
    }
}

/// Surface-independent lift for every non-`PermissionDenied` category.
fn lift_common(err: UsageCollectorError) -> CanonicalError {
    use UsageCollectorError as E;
    match err {
        // ---- 400 InvalidArgument ----
        E::InvalidArgument {
            resource_type,
            resource_name,
            field,
            reason,
            detail,
        } => {
            let wire_reason = reason.as_wire();
            if resource_type == USAGE_RECORD_RESOURCE {
                let b = UsageRecordResource::invalid_argument().with_field_violation(
                    field,
                    detail,
                    wire_reason,
                );
                match resource_name {
                    Some(name) => b.with_resource(name).create(),
                    None => b.create(),
                }
            } else {
                unrecognized_resource(resource_type)
            }
        }

        // ---- 400 InvalidArgument, upstream-coded ----
        E::CursorRejected {
            source,
            detail,
            field,
        } => {
            let upstream = CanonicalError::from(source);
            match upstream_field_violations(&upstream) {
                Some([violation]) => UsageRecordResource::invalid_argument()
                    .with_field_violation(
                        field.as_wire().to_owned(),
                        detail,
                        violation.reason.clone(),
                    )
                    .create(),

                // Multi-violation cursor errors need deliberate projection.
                Some(violations) => {
                    debug_assert!(
                        false,
                        "a toolkit_odata cursor error lifted to {} field violations; \
                         the gear's description fits exactly one, so a multi-violation \
                         condition has to be projected deliberately",
                        violations.len()
                    );
                    upstream
                }

                // Break loudly in debug rather than drop the gear's recovery guidance.
                None => {
                    debug_assert!(
                        false,
                        "toolkit_odata cursor error no longer lifts to an InvalidArgument \
                         field violation; the gear's description and resource scope were \
                         dropped"
                    );
                    upstream
                }
            }
        }

        // ---- 404 NotFound ----
        E::NotFound {
            resource_type,
            name,
            reason: _,
            detail,
        } => {
            if resource_type == USAGE_RECORD_RESOURCE {
                UsageRecordResource::not_found(detail)
                    .with_resource(name)
                    .create()
            } else {
                unrecognized_resource(resource_type)
            }
        }

        // ---- 409 AlreadyExists ----
        E::AlreadyExists {
            resource_type,
            name,
            detail,
        } => {
            if resource_type == USAGE_RECORD_RESOURCE {
                UsageRecordResource::already_exists(detail)
                    .with_resource(name)
                    .create()
            } else {
                unrecognized_resource(resource_type)
            }
        }

        // ---- 409 Aborted (Conflict) ----
        // @cpt-dod:cpt-cf-usage-collector-fr-idempotency:p1
        // @cpt-dod:cpt-cf-usage-collector-principle-idempotency-by-key:p1
        E::Conflict {
            resource_type,
            name,
            outcome,
            detail,
            ..
        } => {
            let reason = outcome.reason();
            let wire_reason = reason.as_wire();
            if resource_type == USAGE_RECORD_RESOURCE {
                UsageRecordResource::aborted(detail)
                    .with_resource(name)
                    .with_reason(wire_reason)
                    .create()
            } else {
                unrecognized_resource(resource_type)
            }
        }

        // ---- 503 ServiceUnavailable (surface-less) ----
        // @cpt-begin:cpt-cf-usage-collector-principle-pluggable-storage:p1:inst-dod-pluggable-storage-fail
        E::ServiceUnavailable {
            retry_after_seconds,
            detail,
        } => {
            let mut builder = CanonicalError::service_unavailable().with_detail(detail);
            if let Some(after) = retry_after_seconds {
                builder = builder.with_retry_after_seconds(after);
            }
            builder.create()
        }
        // @cpt-end:cpt-cf-usage-collector-principle-pluggable-storage:p1:inst-dod-pluggable-storage-fail

        // ---- 429 ResourceExhausted ----
        // @cpt-algo:cpt-cf-usage-collector-algo-quota-throttle-outcome:p2
        // @cpt-dod:cpt-cf-usage-collector-dod-quota-whole-rejection:p2
        E::ResourceExhausted {
            retry_after_seconds,
            detail,
        } => UsageRecordResource::resource_exhausted(detail.clone())
            .with_quota_violation(INGESTION_QUOTA_SUBJECT, detail)
            .with_quota_violation_retry_after_seconds(retry_after_seconds)
            .create(),

        // ---- 500 Internal ----
        E::Internal { detail } => CanonicalError::internal(detail).create(),

        // Fail closed rather than leak an unmapped wire shape.
        other => {
            debug_assert!(false, "lift_common missing arm for variant: {other:?}");
            CanonicalError::internal("unmapped usage-collector error variant").create()
        }
    }
}

/// PDP denial envelope.
fn authz_denial(detail: &str) -> CanonicalError {
    tracing::warn!(deny_reason = %detail, "PDP denied request");
    UsageRecordResource::permission_denied()
        .with_reason("AUTHZ")
        .create()
}

/// Lift a per-record [`UsageCollectorError`] onto an RFC-9457 [`Problem`].
#[must_use]
pub(crate) fn usage_record_error_to_problem(err: UsageCollectorError) -> Problem {
    let extra = conflict_context_extras(&err);
    let mut problem = Problem::from(usage_collector_error_to_canonical_for_usage_record(err));
    if let Some(context) = problem.context.as_object_mut() {
        context.extend(extra);
    }
    problem
}

/// Extra conflict context keys for rejected batch records.
fn conflict_context_extras(
    err: &UsageCollectorError,
) -> serde_json::Map<String, serde_json::Value> {
    let mut extra = serde_json::Map::new();
    if let UsageCollectorError::Conflict { outcome, .. } = err {
        match outcome {
            ConflictOutcome::TargetNotConverged {
                retry_after_seconds,
            } => {
                extra.insert("retryable".to_owned(), serde_json::Value::Bool(true));
                if let Some(seconds) = retry_after_seconds {
                    extra.insert(
                        "retry_after_seconds".to_owned(),
                        serde_json::Value::from(*seconds),
                    );
                }
            }
            ConflictOutcome::AlreadyInvalidated {
                invalidated_by,
                reason_code,
            } => {
                extra.insert(
                    "invalidated_by".to_owned(),
                    serde_json::Value::String(invalidated_by.to_string()),
                );
                extra.insert(
                    "reason_code".to_owned(),
                    serde_json::Value::String(reason_code.as_str().to_owned()),
                );
            }
            // `IdempotencyConflict` and future `#[non_exhaustive]` outcomes
            // carry no extra context here.
            _ => {}
        }
    }
    extra
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "sdk_error_mapping_tests.rs"]
mod sdk_error_mapping_tests;
