//! Unit tests for the host `DomainError` -> SDK error bridge.
//!
//! Coverage mirrors the canonical mapping: plugin variants lift into
//! `DomainError`, and `DomainError` projects forward onto the compacted
//! seven-category `UsageCollectorError` envelope. The reverse
//! (`UsageCollectorError -> DomainError`) bridge was removed in the
//! error-envelope compaction — the public envelope is terminal — so these
//! tests exercise the plugin->domain and domain->SDK directions only.

use usage_collector_sdk::{
    ConflictReason, MeterTypeId, NotFoundReason, USAGE_RECORD_RESOURCE, UsageCollectorError,
    UsageCollectorPluginError, ValidationReason,
};

use super::*;

#[test]
fn plugin_transient_maps_to_service_unavailable() {
    let domain: DomainError =
        UsageCollectorPluginError::transient("downstream connection reset").into();
    assert!(matches!(
        &domain,
        DomainError::PluginTransient { detail, retry_after_seconds: None }
            if detail == "downstream connection reset"
    ));
    let sdk: UsageCollectorError = domain.into();
    match sdk {
        UsageCollectorError::ServiceUnavailable {
            retry_after_seconds,
            detail,
        } => {
            assert_eq!(detail, "downstream connection reset");
            assert_eq!(retry_after_seconds, None);
        }
        other => panic!("expected ServiceUnavailable, got {other:?}"),
    }
}

#[test]
fn plugin_internal_maps_to_internal() {
    let domain: DomainError = UsageCollectorPluginError::internal("invariant violation").into();
    let sdk: UsageCollectorError = domain.into();
    match sdk {
        UsageCollectorError::Internal { detail } => assert_eq!(detail, "invariant violation"),
        other => panic!("expected Internal, got {other:?}"),
    }
}

#[test]
fn authorization_denied_lifts_to_permission_denied_preserving_reason() {
    let domain = DomainError::AuthorizationDenied {
        reason: Some("denied by policy".to_owned()),
    };
    let sdk: UsageCollectorError = domain.into();
    match sdk {
        UsageCollectorError::PermissionDenied { detail } => {
            assert_eq!(detail, "denied by policy");
        }
        other => panic!("expected PermissionDenied, got {other:?}"),
    }
}

#[test]
fn enforcer_denied_extracts_deny_reason_fields_into_reason_string() {
    use authz_resolver_sdk::EnforcerError;
    use authz_resolver_sdk::models::DenyReason;

    let with_details: DomainError = EnforcerError::Denied {
        deny_reason: Some(DenyReason {
            error_code: "TENANT_BARRIER".to_owned(),
            details: Some("subject home tenant != context".to_owned()),
        }),
    }
    .into();
    match with_details {
        DomainError::AuthorizationDenied { reason } => assert_eq!(
            reason.as_deref(),
            Some("TENANT_BARRIER: subject home tenant != context"),
        ),
        other => panic!("expected AuthorizationDenied, got {other:?}"),
    }

    let bare: DomainError = EnforcerError::Denied {
        deny_reason: Some(DenyReason {
            error_code: "FORBIDDEN".to_owned(),
            details: None,
        }),
    }
    .into();
    match bare {
        DomainError::AuthorizationDenied { reason } => {
            assert_eq!(reason.as_deref(), Some("FORBIDDEN"));
        }
        other => panic!("expected AuthorizationDenied, got {other:?}"),
    }

    let missing: DomainError = EnforcerError::Denied { deny_reason: None }.into();
    match missing {
        DomainError::AuthorizationDenied { reason } => assert!(reason.is_none()),
        other => panic!("expected AuthorizationDenied, got {other:?}"),
    }
}

#[test]
fn plugin_not_found_maps_to_service_unavailable() {
    let domain = DomainError::PluginNotFound {
        vendor: "acme".to_owned(),
    };
    let sdk: UsageCollectorError = domain.into();
    assert!(matches!(
        sdk,
        UsageCollectorError::ServiceUnavailable { .. }
    ));
}

#[test]
fn domain_unknown_metadata_key_lifts_to_invalid_argument() {
    let gts_type_id = sample_meter_id();
    let key = "unexpected_field".to_owned();
    let domain = DomainError::UnknownMetadataKey {
        gts_type_id: gts_type_id.clone(),
        key: key.clone(),
    };
    let sdk: UsageCollectorError = domain.into();
    match sdk {
        UsageCollectorError::InvalidArgument {
            resource_type,
            resource_name,
            reason,
            detail,
            ..
        } => {
            assert_eq!(resource_type, USAGE_RECORD_RESOURCE);
            assert_eq!(resource_name.as_deref(), Some(gts_type_id.as_ref()));
            assert_eq!(reason, ValidationReason::UnknownMetadataKey);
            assert!(detail.contains(&key));
        }
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

// ── Entry-lookup and invalidation error variants ────────────────────

#[test]
fn plugin_usage_record_not_found_lifts_to_sdk_not_found() {
    let id = uuid::Uuid::from_u128(0xDEAD_BEEF);
    let domain: DomainError = UsageCollectorPluginError::UsageRecordNotFound { id }.into();
    assert!(matches!(domain, DomainError::UsageRecordNotFound { id: d } if d == id));
    let sdk: UsageCollectorError = domain.into();
    match sdk {
        UsageCollectorError::NotFound {
            resource_type,
            name,
            reason,
            ..
        } => {
            assert_eq!(resource_type, USAGE_RECORD_RESOURCE);
            assert_eq!(name, id.to_string());
            assert_eq!(reason, NotFoundReason::UsageRecordNotFound);
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn sdk_already_invalidated_is_not_retryable() {
    let err = UsageCollectorError::already_invalidated(
        uuid::Uuid::nil(),
        uuid::Uuid::nil(),
        usage_collector_sdk::ReasonCode::new("emitter_defect").expect("valid reason code"),
    );
    assert!(!err.is_retryable(), "AlreadyInvalidated is not retryable");
}

// ---------------------------------------------------------------------------
// DeclarationNotFound — Type Resolver's fail-closed "does not resolve" case.
// Both constructors build this one variant: a genuine not-found
// answer from the registry and an incomplete declaration collapse to the
// identical wire failure, per DESIGN §3.2/§3.3 (see the variant's doc
// comment).
// ---------------------------------------------------------------------------

fn sample_meter_id() -> MeterTypeId {
    MeterTypeId::new("gts.cf.core.uc.usage_record.v1~example.metering._.stored_volume.v1~")
        .expect("valid usage_record-derived meter type id")
}

#[test]
fn declaration_not_found_names_the_identifier_and_says_not_declared() {
    let id = sample_meter_id();
    let err = DomainError::declaration_not_found(&id);
    assert!(matches!(
        &err,
        DomainError::DeclarationNotFound { gts_type_id, reason }
            if gts_type_id == id.as_str() && reason == "is not declared"
    ));
    assert!(err.is_declaration_not_found());
}

#[test]
fn declaration_incomplete_names_the_identifier_and_carries_the_reason() {
    let id = sample_meter_id();
    let err = DomainError::declaration_incomplete(&id, "declares no `canonical_unit`");
    assert!(matches!(
        &err,
        DomainError::DeclarationNotFound { gts_type_id, reason }
            if gts_type_id == id.as_str() && reason == "declares no `canonical_unit`"
    ));
    assert!(
        err.to_string().contains("canonical_unit"),
        "diagnostic must name the offending trait, got: {err}"
    );
    // Same predicate as a genuine not-found: the Type Resolver's cache must
    // not treat an incomplete declaration as more resolvable than an absent
    // one.
    assert!(err.is_declaration_not_found());
}

#[test]
fn other_domain_errors_are_not_declaration_not_found() {
    assert!(!DomainError::Internal("boom".to_owned()).is_declaration_not_found());
}

#[test]
fn declaration_not_found_lifts_to_sdk_not_found_naming_the_usage_record_resource() {
    let id = sample_meter_id();
    let domain = DomainError::declaration_not_found(&id);
    let sdk: UsageCollectorError = domain.into();
    match sdk {
        UsageCollectorError::NotFound {
            resource_type,
            name,
            reason,
            detail,
        } => {
            assert_eq!(resource_type, USAGE_RECORD_RESOURCE);
            assert_eq!(name, id.as_str());
            assert_eq!(reason, NotFoundReason::DeclarationNotFound);
            assert!(detail.contains(id.as_str()));
            assert!(detail.contains("is not declared"));
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// DomainError::invalid_metadata / InvalidMetadata: the closed
// metadata surface a meter declares. `CompiledMetadataSchema::validate` has
// no `gts_type_id` in scope (only the entry's own metadata map), so — unlike
// `UnknownMetadataKey` above — this lifts attributed to the record surface,
// not a specific resource name.
// ---------------------------------------------------------------------------

#[test]
fn invalid_metadata_carries_the_joined_detail() {
    let err = DomainError::invalid_metadata("'tier' was unexpected; \"\" is too short");
    assert!(matches!(&err, DomainError::InvalidMetadata(detail) if detail.contains("tier")));
    assert!(err.to_string().contains("tier"));
}

#[test]
fn invalid_metadata_lifts_to_invalid_argument_on_the_record_resource() {
    let domain = DomainError::invalid_metadata("'tier' was unexpected");
    let sdk: UsageCollectorError = domain.into();
    match sdk {
        UsageCollectorError::InvalidArgument {
            resource_type,
            resource_name,
            field,
            reason,
            detail,
        } => {
            assert_eq!(resource_type, USAGE_RECORD_RESOURCE);
            assert_eq!(resource_name, None);
            assert_eq!(field, "metadata");
            assert_eq!(reason, ValidationReason::MetadataValidation);
            assert!(detail.contains("tier"));
        }
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

#[test]
fn a_not_converged_answer_outside_a_target_lookup_is_internal() {
    let domain: DomainError = UsageCollectorPluginError::UsageRecordNotConverged {
        id: uuid::Uuid::from_u128(0xC0),
    }
    .into();
    assert!(matches!(domain, DomainError::Internal(_)), "got {domain:?}");
}

#[test]
fn target_not_converged_lifts_to_a_conflict_naming_the_target() {
    let target = uuid::Uuid::from_u128(0xC1);
    let sdk: UsageCollectorError = DomainError::TargetNotConverged { target }.into();
    match sdk {
        UsageCollectorError::Conflict { name, reason, .. } => {
            assert_eq!(name, target.to_string());
            assert_eq!(reason, ConflictReason::TargetNotConverged);
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
    assert!(
        !UsageCollectorError::target_not_converged(target).is_retryable(),
        "retryability rides the wire context, not is_retryable (DESIGN \u{a7}3.3)"
    );
}

// ── Lifting a store conflict by the dispatched entry's kind ─────────

fn stored_entry(
    invalidation: Option<usage_collector_sdk::Invalidation>,
) -> usage_collector_sdk::UsageRecord {
    let key = invalidation
        .is_none()
        .then(|| usage_collector_sdk::IdempotencyKey::new("idem-stored").expect("valid key"));
    usage_collector_sdk::CreateUsageRecord {
        gts_type_id: MeterTypeId::new("gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1~")
            .expect("valid meter id"),
        tenant_id: uuid::Uuid::from_u128(0x7E57),
        resource_ref: usage_collector_sdk::ResourceRef::new("res-1", "compute.vm")
            .expect("valid resource ref"),
        subject_ref: None,
        metadata: std::collections::BTreeMap::new(),
        quantity: usage_collector_sdk::UsageQuantity::parse("1").expect("valid quantity"),
        idempotency_key: key,
        invalidation,
        window_start: time::OffsetDateTime::UNIX_EPOCH,
        window_end: time::OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
    }
    .try_into_usage_record(
        usage_collector_sdk::RecordOrigin::Live,
        time::OffsetDateTime::UNIX_EPOCH,
    )
    .expect("valid fixture")
}

#[test]
fn a_conflict_on_a_dispatched_record_is_an_idempotency_conflict_naming_the_stored_entry() {
    let existing = stored_entry(None);
    let err = UsageCollectorPluginError::idempotency_conflict("idem-stored", existing.clone());
    let domain = super::lift_dispatch_error(err, None);
    assert!(
        matches!(
            &domain,
            DomainError::IdempotencyConflict { idempotency_key, existing_id }
                if idempotency_key == "idem-stored" && *existing_id == existing.id
        ),
        "got {domain:?}"
    );
    match UsageCollectorError::from(domain) {
        UsageCollectorError::Conflict {
            name,
            reason,
            invalidated_by,
            reason_code,
            ..
        } => {
            assert_eq!(name, existing.id.to_string());
            assert_eq!(reason, ConflictReason::IdempotencyConflict);
            assert_eq!(invalidated_by, None);
            assert_eq!(reason_code, None);
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
}

#[test]
fn a_conflict_on_a_dispatched_invalidation_is_already_invalidated() {
    let target = uuid::Uuid::from_u128(0x7A);
    let stored_reason =
        usage_collector_sdk::ReasonCode::new("emitter_defect").expect("valid reason code");
    let existing = stored_entry(Some(usage_collector_sdk::Invalidation {
        target,
        reason: stored_reason.clone(),
    }));
    let dispatched = usage_collector_sdk::Invalidation {
        target,
        reason: usage_collector_sdk::ReasonCode::new("late_correction").expect("valid reason code"),
    };
    let err =
        UsageCollectorPluginError::idempotency_conflict(format!("inv:{target}"), existing.clone());
    match UsageCollectorError::from(super::lift_dispatch_error(err, Some(&dispatched))) {
        UsageCollectorError::Conflict {
            name,
            reason,
            invalidated_by,
            reason_code,
            ..
        } => {
            assert_eq!(name, target.to_string(), "the rejection names the target");
            assert_eq!(reason, ConflictReason::AlreadyInvalidated);
            assert_eq!(invalidated_by, Some(existing.id));
            assert_eq!(
                reason_code,
                Some(stored_reason),
                "the stored reason code, not the submitted one"
            );
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
}

#[test]
fn a_stored_entry_that_is_not_an_invalidation_is_an_invariant_breach() {
    let target = uuid::Uuid::from_u128(0x7B);
    let dispatched = usage_collector_sdk::Invalidation {
        target,
        reason: usage_collector_sdk::ReasonCode::new("emitter_defect").expect("valid reason code"),
    };
    let err = UsageCollectorPluginError::idempotency_conflict(
        format!("inv:{target}"),
        stored_entry(None),
    );
    assert!(matches!(
        super::lift_dispatch_error(err, Some(&dispatched)),
        DomainError::Internal(_)
    ));
}

#[test]
fn a_conflict_reaching_the_context_free_lift_is_internal() {
    let err = UsageCollectorPluginError::idempotency_conflict("idem-stored", stored_entry(None));
    assert!(matches!(DomainError::from(err), DomainError::Internal(_)));
}

#[test]
fn a_plugin_cursor_refusal_lifts_to_an_invalid_argument_on_the_cursor_field() {
    let domain: DomainError = UsageCollectorPluginError::CursorBeyondRetention.into();
    let public: UsageCollectorError = domain.into();

    match public {
        UsageCollectorError::InvalidArgument {
            field,
            reason,
            ref detail,
            ..
        } => {
            assert_eq!(field, "cursor");
            assert_eq!(reason, ValidationReason::CursorBeyondRetention);
            assert!(
                !detail.is_empty(),
                "the reason is typed but the description is the gateway's to author, so it must \
                 say something actionable"
            );
        }
        other => panic!(
            "DESIGN section 3.3 lifts CursorBeyondRetention to InvalidArgument(\
             CursorBeyondRetention); it must not reach CursorRejected, whose reason comes only \
             from toolkit_odata. Got: {other:?}"
        ),
    }
}
