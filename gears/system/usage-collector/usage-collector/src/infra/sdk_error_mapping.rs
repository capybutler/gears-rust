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
use usage_collector_sdk::{ConflictOutcome, USAGE_RECORD_RESOURCE, UsageCollectorError};

// Resource marker — the single GTS resource type this gear's canonical
// envelope names on `resource_type`, matching `domain/authz.rs`.

#[resource_error(gts_id!("cf.core.uc.usage_record.v1~"))]
pub(crate) struct UsageRecordResource;

/// Fixed `violations[0].subject` a quota rejection's canonical envelope
/// carries. DESIGN §3.3 Error Contract: "The violation's `subject` is a
/// fixed value naming the ingestion quota." Fixed rather than derived: there
/// is one bucket, shared by every subject and every route, so
/// there is exactly one quota for `subject` to name — never a per-tenant or
/// per-request string.
const INGESTION_QUOTA_SUBJECT: &str = "ingestion_quota";

/// Lift the SDK error envelope onto the AIP-193 canonical shape for every
/// REST route sharing the `usage_record` GTS resource type:
/// `POST /usage-collector/v1/records` (single and batch alike),
/// `POST /usage-collector/v1/records/backfill`,
/// `GET /usage-collector/v1/records`,
/// `POST /usage-collector/v1/records/aggregate`,
/// `GET /usage-collector/v1/records/{id}`, and
/// `GET /usage-collector/v1/feed`. Used from handlers in
/// `api/rest/handlers/usage_records.rs` and
/// `api/rest/handlers/usage_feed.rs`. The cross-cutting `PermissionDenied`
/// variant resolves to a UsageRecord-shaped envelope; every other variant
/// carries its own `resource_type` and routes through [`lift_common`].
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
        // The wire `reason` comes from `toolkit_odata`'s own mapping and is
        // never spelled here (Spec §3.13). The wire `field` is not
        // upstream's to give: the feed carries two cursor-bearing
        // parameters, `cursor` and `until`, and upstream only ever knows
        // `cursor`, so `field` names which one per the gear's own
        // `CursorField` (DESIGN §3.3). This arm reads the reason off
        // upstream's canonical error and rebuilds through the same
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
        // `reason` is dropped, by design. `toolkit_canonical_errors`'
        // `NotFoundV1` context is an empty, platform-shared struct with no
        // reason path, and the canonical builder constructs it with none —
        // every gear's 404 is in the same position, so there is nothing
        // here to project onto. A reader looking for the reason on the
        // `Problem` body will not find it: a REST client separates a
        // missing declaration, a missing entry and an unresolvable
        // `invalidates` by `detail` alone. In-process consumers read the
        // typed `NotFoundReason` off the SDK error instead.
        //
        // Changing that starts upstream, in `NotFoundV1`, not here and not
        // in the gear's YAML — whose 404 `context` is already
        // `additionalProperties: true`, so it looks permissive while the
        // slot it would carry does not exist. The gear's half is the
        // second half: the spec prose enumerating which categories carry a
        // reason would have to name 404 as well.
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
        // The idempotency conflict and the dedup conflict of an invalidation
        // both collapse here; the typed `ConflictReason` rides on
        // `context.reason`.
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
                unrecognized_resource(&resource_type)
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
        // Per-subject ingestion allowance exhausted. The single
        // `retry_after_seconds` hint is forwarded through
        // `with_quota_violation_retry_after_seconds`, landing on
        // `violations[0].retry_after_seconds` — the slot the `context`
        // property of `docs/usage-collector-v1.yaml`'s `Problem` schema
        // names ("A quota rejection carries its retry delay on
        // `violations[0].retry_after_seconds`"), confirmed by
        // `sdk_error_mapping_tests.rs`'s serialized-body assertion.
        //
        // Unlike `ServiceUnavailable`'s `with_retry_after_seconds`, this does
        // NOT make the `Retry-After` header "free": ToolKit's
        // `Problem`/`CanonicalError` `IntoResponse`
        // (`toolkit-canonical-errors/src/problem.rs`'s
        // `service_unavailable_retry_after_seconds`) derives that header for
        // the `service_unavailable` category alone, by the shared library's
        // own stated design
        // (`toolkit-canonical-errors/src/context.rs`'s `QuotaViolationV1` doc:
        // "the wire `Retry-After` header is for `ServiceUnavailable`, not
        // `ResourceExhausted`"). The REST call site therefore sets the header
        // itself, and does: `api/rest/handlers/usage_records.rs`'s
        // `lift_whole_request_ingestion_error` builds the response from this
        // `CanonicalError` and inserts `RETRY_AFTER` from the same
        // `retry_after_seconds` this arm puts in the body, the way `oagw`'s
        // `api/rest/error.rs::error_response` does for its own 429s. The hook
        // it needs is `dispatch_usage_record_batch`'s
        // `ApiResult<Response>` return type, which is what lets a handler
        // touch the response rather than hand back a body alone.
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
/// via the normal `IntoResponse` path. Both paths share the canonical lift. This
/// one also adds the conflict context keys the batch envelope publishes
/// (`retryable`, `invalidated_by` and `reason_code`). A
/// whole-request rejection never carries a `Conflict`.
#[must_use]
pub(crate) fn usage_record_error_to_problem(err: UsageCollectorError) -> Problem {
    let extra = conflict_context_extras(&err);
    let mut problem = Problem::from(usage_collector_error_to_canonical_for_usage_record(err));
    if let Some(context) = problem.context.as_object_mut() {
        context.extend(extra);
    }
    problem
}

/// The `context` keys a conflict carries beyond the canonical `reason`, which
/// the platform `Aborted` context has no slot for (usage-collector-v1.yaml
/// `RejectedUsageRecord`).
///
/// **Gap closed (usage-collector slice 8).** `ConflictReason::TargetNotConverged`
/// gets `retryable = true` below, and, where the `UsageCollectorError` already
/// carries one, `retry_after_seconds` too. DESIGN:1483-1484 says the gear
/// "stamps `target_not_converged_retry_after_secs` (§3.8), which a deployment
/// sets from that published sum" — `crate::config::UsageCollectorConfig` now
/// declares that key, and the host's `domain::error::lift_domain_error`
/// resolves it onto `UsageCollectorError::Conflict`'s `retry_after_seconds`
/// field before the error ever reaches this lift, so this function only has
/// to forward whatever is already there. (Previously: `grep -rn
/// 'target_not_converged_retry_after_secs'` over `usage-collector/src/` and
/// `usage-collector-sdk/src/` returned prose only — zero functional
/// occurrences — which is exactly the measurement this comment now
/// falsifies by its own existence, since it now names the key itself.)
///
/// Four feature-2.5 identifiers named this delay and were left unticked for
/// it: `cpt-cf-usage-collector-algo-target-resolution` (step
/// `inst-resolve-unconverged-return`, "carrying the configured retry
/// delay"), `cpt-cf-usage-collector-dod-converged-target-resolution`,
/// `cpt-cf-usage-collector-dod-typed-invalidation-outcomes`, and
/// `cpt-cf-usage-collector-flow-withdraw-usage-record` (error scenario, "The
/// rejection is retryable and carries a delay"). This task does not tick
/// them: they are feature 2.5's own traceability, scoped to that feature's
/// task, and verifying their other clauses is out of this task's scope —
/// recorded here as a pointer for whoever picks that up next, not closed by
/// this comment.
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
            ConflictOutcome::IdempotencyConflict => {}
            // `ConflictOutcome` is `#[non_exhaustive]` and foreign here; a
            // future reason with no extra context carries nothing to add.
            _ => {}
        }
    }
    extra
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "sdk_error_mapping_tests.rs"]
mod sdk_error_mapping_tests;
