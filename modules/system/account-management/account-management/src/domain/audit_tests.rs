use super::*;

/// Locks `AuditEventKind::as_str` to the serde wire form so the two
/// representations cannot drift silently.
#[test]
fn audit_event_kind_as_str_matches_serde() {
    let all = [
        AuditEventKind::BootstrapCompleted,
        AuditEventKind::BootstrapSkipped,
        AuditEventKind::BootstrapDeferredToReaper,
        AuditEventKind::BootstrapIdpTimeout,
        AuditEventKind::BootstrapInvariantViolation,
        AuditEventKind::BootstrapFinalizationFailed,
        AuditEventKind::ConversionExpired,
        AuditEventKind::ProvisioningReaperCompensated,
        AuditEventKind::HardDeleteCleanupCompleted,
        AuditEventKind::TenantDeprovisionCompleted,
        AuditEventKind::TenantStateChanged,
        AuditEventKind::ConversionStateChanged,
        AuditEventKind::MetadataWritten,
        AuditEventKind::HardDeleteRequested,
        AuditEventKind::CrossTenantDenialRecorded,
        AuditEventKind::IdpUnavailableRecorded,
    ];
    for kind in all {
        let json = serde_json::to_string(&kind).expect("serialize");
        let unquoted = json.trim_matches('"');
        assert_eq!(unquoted, kind.as_str(), "drift on {kind:?}");
    }
}

#[test]
fn audit_actor_serde_round_trip() {
    let cases = [
        AuditActor::System,
        AuditActor::TenantScoped {
            subject_id: Uuid::nil(),
            subject_tenant_id: Uuid::nil(),
        },
    ];
    for actor in cases {
        let json = serde_json::to_string(&actor).expect("serialize");
        let back: AuditActor = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(format!("{actor:?}"), format!("{back:?}"));
    }
}

#[test]
fn audit_actor_tenant_scoped_wire_format() {
    let actor = AuditActor::TenantScoped {
        subject_id: Uuid::nil(),
        subject_tenant_id: Uuid::nil(),
    };
    let json = serde_json::to_value(&actor).expect("serialize");
    assert_eq!(json["type"], "tenantScoped");
    assert!(json.get("subjectId").is_some(), "expected camelCase field");
    assert!(json.get("subjectTenantId").is_some());
}

#[test]
fn from_context_rejects_anonymous_security_context() {
    // `SecurityContext::anonymous()` sets both subject_id and
    // subject_tenant_id to `Uuid::nil()`. A `TenantScoped` actor
    // built from those would be a silent audit-trail corruption,
    // so AM refuses at the boundary.
    let ctx = SecurityContext::anonymous();
    let result = AuditEvent::from_context(
        AuditEventKind::TenantStateChanged,
        &ctx,
        Uuid::from_u128(0xAA),
        serde_json::json!({}),
    );
    assert_eq!(result.unwrap_err(), AnonymousActorNotEligible);
}

#[test]
fn from_context_accepts_validated_security_context() {
    let subject = Uuid::from_u128(0x01);
    let subject_tenant = Uuid::from_u128(0x02);
    let ctx = SecurityContext::builder()
        .subject_id(subject)
        .subject_tenant_id(subject_tenant)
        .build()
        .expect("builder");
    let tenant_id = Uuid::from_u128(0xAA);
    let event = AuditEvent::from_context(
        AuditEventKind::TenantStateChanged,
        &ctx,
        tenant_id,
        serde_json::json!({}),
    )
    .expect("validated context produces an event");
    assert_eq!(event.tenant_id, Some(tenant_id));
    match event.actor {
        AuditActor::TenantScoped {
            subject_id,
            subject_tenant_id,
        } => {
            assert_eq!(subject_id, subject);
            assert_eq!(subject_tenant_id, subject_tenant);
        }
        AuditActor::System => panic!("expected TenantScoped actor"),
    }
}

/// `system_no_tenant` is the legitimate way to emit `BootstrapIdpTimeout`
/// before saga step 1 establishes the root tenant id. It refuses to
/// emit any other kind, so a future caller cannot silently drop the
/// `tenant_id` from a non-bootstrap event.
#[test]
fn system_no_tenant_accepts_bootstrap_idp_timeout() {
    let event =
        AuditEvent::system_no_tenant(AuditEventKind::BootstrapIdpTimeout, serde_json::json!({}))
            .expect("BootstrapIdpTimeout is the canonical no-tenant kind");
    assert!(event.tenant_id.is_none());
    assert!(matches!(event.actor, AuditActor::System));
}

/// Nil `tenant_id` MUST be refused at the constructor — a `Some(Uuid::nil())`
/// audit row is silent forensics corruption (cannot be distinguished from
/// the legitimate `None` path that goes through `system_no_tenant`).
#[test]
fn from_context_rejects_nil_tenant_id() {
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::from_u128(0x01))
        .subject_tenant_id(Uuid::from_u128(0x02))
        .build()
        .expect("builder");
    let result = AuditEvent::from_context(
        AuditEventKind::TenantStateChanged,
        &ctx,
        Uuid::nil(),
        serde_json::json!({}),
    );
    assert_eq!(result.unwrap_err(), AnonymousActorNotEligible);
}

#[test]
fn system_rejects_nil_tenant_id() {
    let result = AuditEvent::system(
        AuditEventKind::BootstrapCompleted,
        Uuid::nil(),
        serde_json::json!({}),
    );
    assert_eq!(
        result.unwrap_err(),
        SystemActorNotEligible {
            kind: AuditEventKind::BootstrapCompleted
        }
    );
}

#[test]
fn system_no_tenant_rejects_other_kinds() {
    // BootstrapCompleted carries a tenant_id by construction (saga
    // step 3 has already established the root). Routing it through
    // `system_no_tenant` would silently strip the id — refuse loud.
    let result =
        AuditEvent::system_no_tenant(AuditEventKind::BootstrapCompleted, serde_json::json!({}));
    assert_eq!(
        result.unwrap_err(),
        SystemActorNotEligible {
            kind: AuditEventKind::BootstrapCompleted
        }
    );
}
