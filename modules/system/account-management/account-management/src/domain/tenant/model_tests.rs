use super::*;

#[test]
fn smallint_round_trip_is_total_over_known_values() {
    for s in [
        TenantStatus::Provisioning,
        TenantStatus::Active,
        TenantStatus::Suspended,
        TenantStatus::Deleted,
    ] {
        let v = s.as_smallint();
        assert_eq!(TenantStatus::from_smallint(v), Some(s));
    }
}

#[test]
fn from_smallint_rejects_unknown_values() {
    assert_eq!(TenantStatus::from_smallint(-1), None);
    assert_eq!(TenantStatus::from_smallint(4), None);
    assert_eq!(TenantStatus::from_smallint(42), None);
}

#[test]
fn is_sdk_visible_excludes_provisioning_only() {
    assert!(!TenantStatus::Provisioning.is_sdk_visible());
    assert!(TenantStatus::Active.is_sdk_visible());
    assert!(TenantStatus::Suspended.is_sdk_visible());
    assert!(TenantStatus::Deleted.is_sdk_visible());
}

#[test]
fn empty_update_is_empty() {
    assert!(TenantUpdate::default().is_empty());
    assert!(
        !TenantUpdate {
            name: Some("x".into()),
            ..Default::default()
        }
        .is_empty()
    );
    assert!(
        !TenantUpdate {
            status: Some(TenantStatus::Active),
            ..Default::default()
        }
        .is_empty()
    );
}

#[test]
fn status_transition_active_suspended_allowed() {
    TenantUpdate::validate_status_transition(TenantStatus::Active, TenantStatus::Suspended)
        .expect("active -> suspended ok");
    TenantUpdate::validate_status_transition(TenantStatus::Suspended, TenantStatus::Active)
        .expect("suspended -> active ok");
}

#[test]
fn status_transition_no_op_rejected() {
    // Strict contract: PATCH only permits the cross-flip; resending
    // the current status is a no-op that would still trigger a
    // wasted closure-rewrite, so it surfaces as `Conflict`.
    let active_active =
        TenantUpdate::validate_status_transition(TenantStatus::Active, TenantStatus::Active)
            .expect_err("A->A must reject");
    assert!(matches!(active_active, DomainError::Conflict { .. }));
    let suspended_suspended =
        TenantUpdate::validate_status_transition(TenantStatus::Suspended, TenantStatus::Suspended)
            .expect_err("S->S must reject");
    assert!(matches!(suspended_suspended, DomainError::Conflict { .. }));
}

#[test]
fn status_transition_to_deleted_rejected() {
    let err = TenantUpdate::validate_status_transition(TenantStatus::Active, TenantStatus::Deleted)
        .expect_err("reject");
    assert!(matches!(err, DomainError::Conflict { .. }));
}

#[test]
fn status_transition_from_provisioning_rejected() {
    let err =
        TenantUpdate::validate_status_transition(TenantStatus::Provisioning, TenantStatus::Active)
            .expect_err("reject");
    assert!(matches!(err, DomainError::Conflict { .. }));
}

#[test]
fn status_transition_from_deleted_rejected() {
    let err = TenantUpdate::validate_status_transition(TenantStatus::Deleted, TenantStatus::Active)
        .expect_err("reject");
    assert!(matches!(err, DomainError::Conflict { .. }));
}

#[test]
fn name_length_validation_rejects_empty_and_oversized() {
    assert!(TenantUpdate::validate_name("a").is_ok());
    assert!(TenantUpdate::validate_name(&"x".repeat(255)).is_ok());
    assert!(matches!(
        TenantUpdate::validate_name("").expect_err("empty rejected"),
        DomainError::Validation { .. }
    ));
    assert!(matches!(
        TenantUpdate::validate_name(&"x".repeat(256)).expect_err("too long rejected"),
        DomainError::Validation { .. }
    ));
}

#[test]
fn list_children_query_rejects_provisioning_in_status_filter() {
    // Provisioning rows are SDK-invisible; the constructor must
    // reject any filter that names them so a bogus internal caller
    // cannot leak them via list_children.
    let err = ListChildrenQuery::new(
        Uuid::nil(),
        Some(vec![TenantStatus::Active, TenantStatus::Provisioning]),
        10,
        0,
    )
    .expect_err("provisioning must be rejected");
    assert!(matches!(err, DomainError::Validation { .. }));
}

#[test]
fn list_children_query_accepts_sdk_visible_filters() {
    let q = ListChildrenQuery::new(
        Uuid::nil(),
        Some(vec![
            TenantStatus::Active,
            TenantStatus::Suspended,
            TenantStatus::Deleted,
        ]),
        10,
        0,
    )
    .expect("sdk-visible filter accepted");
    assert_eq!(q.status_filter().expect("filter").len(), 3);
}

#[test]
fn list_children_query_accepts_none_filter() {
    let q = ListChildrenQuery::new(Uuid::nil(), None, 10, 0).expect("none accepted");
    assert!(q.status_filter().is_none());
}

#[test]
fn list_children_query_rejects_zero_top() {
    // The public OpenAPI contract sets `$top` minimum to 1.
    // Accepting 0 here would silently turn an invalid request
    // into an empty page rather than surfacing a validation
    // error to the caller.
    let err = ListChildrenQuery::new(Uuid::nil(), None, 0, 0).expect_err("top=0 must be rejected");
    assert!(matches!(err, DomainError::Validation { .. }));
}
