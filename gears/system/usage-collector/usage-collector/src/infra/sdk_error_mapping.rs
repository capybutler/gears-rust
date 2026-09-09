//! Host-crate boundary lift from the flat SDK error envelope
//! [`UsageCollectorError`] onto the AIP-193 canonical
//! [`toolkit_canonical_errors::CanonicalError`], which `IntoResponse`
//! renders as the RFC-9457 `Problem` body on the REST surface. The
//! SDK-category → AIP-193-category → HTTP-status mapping is documented in
//! DESIGN §3.3 "Error Envelopes".
//!
//! `usage-collector-sdk` deliberately carries no `toolkit-canonical-errors`
//! dependency, so the lift lives here. Both `UsageCollectorError` and
//! `CanonicalError` are foreign to this crate, so the lift is a free
//! `pub(crate)` function ([`usage_collector_error_to_canonical_for_usage_record`])
//! rather than an `impl From<…> for CanonicalError` (which would violate the
//! orphan rule). The gear registers only the ingestion REST surface now —
//! every type declaration is owned by `types-registry` — so there is no
//! catalog-shaped lift entry point any more.

use toolkit_canonical_errors::{
    CanonicalError, FieldViolation, InvalidArgument as InvalidArgumentCtx, Problem, resource_error,
};
use usage_collector_sdk::{USAGE_RECORD_RESOURCE, UsageCollectorError};

// Resource marker — the single GTS resource type this gear's canonical
// envelope names on `resource_type`, matching `domain/authz.rs`.

#[resource_error(gts_id!("cf.core.uc.usage_record.v1~"))]
pub(crate) struct UsageRecordResource;

/// Lift the SDK error envelope onto the AIP-193 canonical shape for the
/// usage-record REST surface — the five routes this gear registers:
/// `POST /usage-collector/v1/records` (single and batch alike),
/// `POST /usage-collector/v1/records/backfill`,
/// `GET /usage-collector/v1/records`,
/// `POST /usage-collector/v1/records/aggregate` and
/// `GET /usage-collector/v1/records/{id}`. Use this from
/// handlers in `api/rest/handlers/usage_records.rs`. The cross-cutting
/// `PermissionDenied` variant resolves to a UsageRecord-shaped envelope;
/// every other variant carries its own `resource_type` and routes through
/// [`lift_common`].
#[must_use]
pub(crate) fn usage_collector_error_to_canonical_for_usage_record(
    err: UsageCollectorError,
) -> CanonicalError {
    match err {
        // @cpt-begin:cpt-cf-usage-collector-dod-usage-emission-fr-ingestion-authorization:p1:inst-dod-authz-deny
        // @cpt-begin:cpt-cf-usage-collector-dod-usage-emission-principle-fail-closed:p1:inst-dod-fail-closed-authz
        UsageCollectorError::PermissionDenied { detail } => authz_denial(&detail),
        // @cpt-end:cpt-cf-usage-collector-dod-usage-emission-principle-fail-closed:p1:inst-dod-fail-closed-authz
        // @cpt-end:cpt-cf-usage-collector-dod-usage-emission-fr-ingestion-authorization:p1:inst-dod-authz-deny
        other => lift_common(other),
    }
}

/// Fallback for a `resource_type` other than [`USAGE_RECORD_RESOURCE`] — the
/// only GTS resource this gear's flat SDK contract ever sets now that the
/// usage-type catalog is gone. An unrecognized value is a host-side breach —
/// assert in debug, surface a redacted `internal` on the wire rather than
/// mislabel a resource.
fn unrecognized_resource(resource_type: &str) -> CanonicalError {
    debug_assert!(
        false,
        "unrecognized resource_type on SDK error: {resource_type}"
    );
    CanonicalError::internal(format!("unrecognized resource_type: {resource_type}")).create()
}

/// The `field_violations` a canonical `InvalidArgument` carries, or `None`
/// when the error is any other shape. Borrowing rather than destructuring at
/// the call site keeps the cursor arm free to hand back the untouched
/// upstream value on the paths it cannot project.
fn upstream_field_violations(err: &CanonicalError) -> Option<&[FieldViolation]> {
    match err {
        CanonicalError::InvalidArgument {
            ctx: InvalidArgumentCtx::FieldViolations { field_violations },
            ..
        } => Some(field_violations),
        _ => None,
    }
}

/// Surface-independent lift for every non-`PermissionDenied` category. The
/// `resource_type` carried on the variant selects the GTS resource marker;
/// the typed `reason` / `field` ride straight onto the canonical envelope
/// (`field_violations[0].reason` for 400s, `context.reason` for 409
/// `Aborted`s). 503 `ServiceUnavailable` carries no `context.reason` — the
/// canonical `ServiceUnavailable` context has no such slot, so operator
/// triage reads the curated `detail` string.
fn lift_common(err: UsageCollectorError) -> CanonicalError {
    use UsageCollectorError as E;
    match err {
        // ---- 400 InvalidArgument ----
        // The validating-newtype rejects, the metadata-shape / size checks,
        // and the bad-prefix gts_type_id parse all route here; `field` +
        // typed `reason` carry the discriminator and `resource_type` names
        // the violated resource (a `gts_type_id`-shaped violation attributes
        // to the referenced meter even on the ingestion surface).
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
                unrecognized_resource(&resource_type)
            }
        }

        // ---- 400 InvalidArgument, upstream-coded ----
        // The wire `field` and `reason` come from `toolkit_odata`'s own
        // mapping and are never spelled here (Spec §3.13). This arm reads
        // them off upstream's canonical error and rebuilds through the same
        // `UsageRecordResource::invalid_argument()` builder every other 400
        // here goes through, rather than editing the value upstream built:
        // the `USAGE_RECORD_RESOURCE` scope is then enforced in one place,
        // by construction, instead of being written back afterwards.
        //
        // Two halves stay the gear's:
        //
        // * `description` — upstream's descriptions name the condition but
        //   not the recovery. `FilterMismatch` renders as "Filter mismatch
        //   between cursor and query", which tells a caller nothing about
        //   resending the query; the gear's names which check refused and
        //   how to get moving again.
        // * the resource scope — `resource_type` names which *entity* the
        //   error is about, and this one is about a usage record whichever
        //   crate detected the defect. §3.13 takes the cursor codes away
        //   from this gear; it says nothing about its resource identity.
        //   Inheriting upstream's would advertise `cf.core.odata.query.v1~`
        //   on one error class of an endpoint whose every other error
        //   advertises `cf.core.uc.usage_record.v1~` — a documented
        //   discrimination layer (`docs/usage-collector-v1.yaml`, "which
        //   entity") quietly changing value.
        E::CursorRejected { source, detail } => {
            let upstream = CanonicalError::from(source);
            match upstream_field_violations(&upstream) {
                Some([violation]) => UsageRecordResource::invalid_argument()
                    .with_field_violation(violation.field.clone(), detail, violation.reason.clone())
                    .create(),

                // Upstream maps `OrderWithCursor` to *two* violations
                // (`$orderby` and `cursor`), deliberately, so a client
                // rendering UI hints sees both halves of the conflict. No
                // constructor reaches that today, but the variant's own doc
                // names `ORDER_WITH_CURSOR` as one of the three §3.13
                // assigns upstream, so a third condition must be handled
                // deliberately rather than silently acquire one half of a
                // description. An empty list lands here too, with its
                // count in the message.
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

                // Same posture as `unrecognized_resource`: silently
                // shipping upstream's description in place of the gear's
                // recovery guidance, under upstream's resource type, is a
                // regression no wire assertion downstream would catch, so
                // break loudly in debug rather than degrade in the dark.
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
            detail,
        } => {
            if resource_type == USAGE_RECORD_RESOURCE {
                UsageRecordResource::not_found(detail)
                    .with_resource(name)
                    .create()
            } else {
                unrecognized_resource(&resource_type)
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
                unrecognized_resource(&resource_type)
            }
        }

        // ---- 409 Aborted (Conflict) ----
        // The idempotency conflict and the store's at-most-one-invalidation
        // rejection both collapse here; the typed `ConflictReason` rides on
        // `context.reason`.
        // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-fr-idempotency:p1
        // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-principle-idempotency-by-key:p1
        E::Conflict {
            resource_type,
            name,
            reason,
            detail,
        } => {
            let wire_reason = reason.as_wire();
            if resource_type == USAGE_RECORD_RESOURCE {
                UsageRecordResource::aborted(detail)
                    .with_resource(name)
                    .with_reason(wire_reason)
                    .create()
            } else {
                unrecognized_resource(&resource_type)
            }
        }

        // ---- 503 ServiceUnavailable (surface-less) ----
        // @cpt-begin:cpt-cf-usage-collector-dod-usage-emission-principle-pluggable-storage:p1:inst-dod-pluggable-storage-fail
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
        // @cpt-end:cpt-cf-usage-collector-dod-usage-emission-principle-pluggable-storage:p1:inst-dod-pluggable-storage-fail

        // ---- 500 Internal ----
        // `detail` is DSN-free and pre-redacted at the construction site by
        // the flat SDK error contract; carried as the internal diagnostic,
        // never leaked to the public wire body.
        E::Internal { detail } => CanonicalError::internal(detail).create(),

        // `UsageCollectorError` is `#[non_exhaustive]`, and `PermissionDenied`
        // is handled by the surface entry points; fail closed to a generic
        // 500 rather than leak an unmapped wire shape.
        other => {
            debug_assert!(false, "lift_common missing arm for variant: {other:?}");
            CanonicalError::internal("unmapped usage-collector error variant").create()
        }
    }
}

/// PDP denial envelope. PDP-supplied detail is intentionally dropped from
/// the wire envelope (never paraphrased to callers), but kept in operator
/// logs so denial triage doesn't collapse to a bare "AUTHZ".
fn authz_denial(detail: &str) -> CanonicalError {
    tracing::warn!(deny_reason = %detail, "PDP denied request");
    UsageRecordResource::permission_denied()
        .with_reason("AUTHZ")
        .create()
}

/// Lift a per-record [`UsageCollectorError`] onto an RFC-9457 [`Problem`] for
/// the ingestion REST surface. Used by the batch handler in
/// [`crate::api::rest::handlers::usage_records`]; whole-request rejections go
/// through [`usage_collector_error_to_canonical_for_usage_record`] directly
/// via the normal `IntoResponse` path. Both paths now share the same lift, so
/// a per-record and a whole-request rejection of the same error are
/// byte-identical on the wire.
#[must_use]
pub(crate) fn usage_record_error_to_problem(err: UsageCollectorError) -> Problem {
    Problem::from(usage_collector_error_to_canonical_for_usage_record(err))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "sdk_error_mapping_tests.rs"]
mod sdk_error_mapping_tests;
