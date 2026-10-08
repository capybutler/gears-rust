//! Regression tests pinning the load-bearing invariant of
//! `Service::create_usage_records` PDP dedup:
//!
//! > **A record reaches the PDP only through
//! > [`AttributionTupleKey::from_record`], so two records that
//! > [`AttributionTupleKey`] groups together compose the same PDP request.**
//!
//! Break it and two records the key groups together could be judged
//! differently by the PDP — a bypass. The structural prevention is that the
//! composer takes only `&AttributionTupleKey`; these tests are the behavioural
//! pin, so a refactor re-introducing a `&UsageRecord` dependency there is
//! caught by the suite.

use std::collections::BTreeMap;
use std::sync::Arc;
use toolkit_gts::gts_id;

use authz_resolver_sdk::models::EvaluationRequest;
use time::OffsetDateTime;
use toolkit_security::{SecurityContext, pep_properties};
use usage_collector_sdk::{
    IdempotencyKey, Invalidation, MeterTypeId, ReasonCode, RecordOrigin, ResourceRef, SubjectRef,
    UsageRecord,
};
use uuid::Uuid;

use super::{
    AttributionTupleKey, authorize_attribution_tuple, authorize_get_reconciliation_metadata,
    authorize_get_usage_record_scope, authorize_list_usage_records, authorize_read_usage_feed,
    usage_record,
};
use crate::domain::ports::metrics::{NoopMetrics, PdpOp};
use crate::domain::test_support::{
    ActionRecordingPermitResolver, CapturingTenantPermitResolver, CountingPermitResolver,
    MeterNarrowingPermitResolver, PermitEmptyConstraintsResolver, enforcer_for, qty,
};

const SAMPLE_GTS_TYPE_ID: &str =
    gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

/// A second meter, for the `gts_type_id` tests whose whole oracle is that a
/// scope narrowed to one meter treats the other differently. Every such test
/// guards the premise with an `assert_ne!` rather than trusting the two
/// literals to stay distinct.
const OTHER_GTS_TYPE_ID: &str =
    gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.requests_served.v1~");

/// Arbitrary RG member-handle type for group-filter fixtures — usage-collector
/// rejects group predicates regardless of the type, so only the shape matters.
const SAMPLE_MEMBER_TYPE: &str = gts_id!("cf.core.rg.type.v1~example.core.uc.owner.v1~");

fn ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(0xA110))
        .subject_tenant_id(Uuid::from_u128(0xB220))
        .subject_type("user")
        .build()
        .expect("authenticated ctx")
}

fn record_with(subject: Option<SubjectRef>) -> UsageRecord {
    UsageRecord {
        id: Uuid::from_u128(0x0001),
        gts_type_id: MeterTypeId::new(SAMPLE_GTS_TYPE_ID).expect("valid gts_type_id"),
        tenant_id: Uuid::from_u128(0xC330),
        resource_ref: ResourceRef::new("rsc-eq", "compute.vm").expect("valid resource ref"),
        subject_ref: subject,
        metadata: BTreeMap::new(),
        quantity: qty("1"),
        idempotency_key: IdempotencyKey::new("idem-eq").expect("valid idempotency key"),
        accepted_at: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        origin: RecordOrigin::Live,
        invalidation: None,
        window_start: OffsetDateTime::UNIX_EPOCH,
        window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
    }
}

/// A record identical to [`record_with(None)`] but owned by `tenant_id`, for
/// driving the per-record tenant gate against a specific owning tenant.
fn record_with_tenant(tenant_id: Uuid) -> UsageRecord {
    UsageRecord {
        tenant_id,
        ..record_with(None)
    }
}

/// Compose the ingestion PDP request for `record` through the one live
/// composer and return the resource properties it actually carried.
async fn composed_properties_for(record: &UsageRecord) -> BTreeMap<String, serde_json::Value> {
    let resolver = CapturingTenantPermitResolver::new();
    let enforcer =
        enforcer_for(Arc::clone(&resolver) as Arc<dyn authz_resolver_sdk::AuthZResolverApi>);

    let key = AttributionTupleKey::from_record(record, usage_record::actions::CREATE);
    authorize_attribution_tuple(&enforcer, &NoopMetrics, PdpOp::Ingest, &ctx(), &key)
        .await
        .expect("permit");

    resolver
        .take_last_request()
        .expect("the composer issued a request")
        .resource
        .properties
        .into_iter()
        .collect()
}

fn json(req: &EvaluationRequest) -> serde_json::Value {
    serde_json::to_value(req).expect("EvaluationRequest serializes as JSON")
}

/// Subject-absent: the tuple key carries no subject fields, the request
/// MUST carry no subject `resource_property` either.
#[tokio::test]
async fn the_pdp_request_carries_no_subject_properties_without_a_subject() {
    let properties = composed_properties_for(&record_with(None)).await;
    assert!(
        !properties.contains_key(pep_properties::OWNER_ID)
            && !properties.contains_key(usage_record::PROP_SUBJECT_TYPE),
        "a subjectless record MUST contribute neither subject property: {properties:?}"
    );
}

/// Subject present without `subject_type`: only `OWNER_ID` is contributed.
#[tokio::test]
async fn the_pdp_request_carries_owner_id_alone_for_a_subject_without_a_type() {
    let properties = composed_properties_for(&record_with(Some(
        SubjectRef::new("subject-eq-1", None::<String>).expect("valid subject"),
    )))
    .await;
    assert_eq!(
        properties
            .get(pep_properties::OWNER_ID)
            .and_then(serde_json::Value::as_str),
        Some("subject-eq-1"),
        "the subject id MUST reach the PDP: {properties:?}"
    );
    assert!(
        !properties.contains_key(usage_record::PROP_SUBJECT_TYPE),
        "an untyped subject MUST contribute no subject_type: {properties:?}"
    );
}

/// Subject present WITH `subject_type`: both `OWNER_ID` and
/// `SUBJECT_TYPE` are contributed. This is the maximal-attribute path —
/// drift here would be the worst case.
#[tokio::test]
async fn the_pdp_request_carries_both_subject_properties_for_a_typed_subject() {
    let properties = composed_properties_for(&record_with(Some(
        SubjectRef::new("subject-eq-2", Some("service")).expect("valid subject"),
    )))
    .await;
    assert_eq!(
        (
            properties
                .get(pep_properties::OWNER_ID)
                .and_then(serde_json::Value::as_str),
            properties
                .get(usage_record::PROP_SUBJECT_TYPE)
                .and_then(serde_json::Value::as_str),
        ),
        (Some("subject-eq-2"), Some("service")),
        "a typed subject MUST contribute both subject properties: {properties:?}"
    );
}

/// Two records that hash-equal under `AttributionTupleKey` MUST always
/// produce equal PDP requests — even when their *non*-tuple fields (`id`,
/// `quantity`, `idempotency_key`, `accepted_at`, `origin`, `metadata`,
/// `invalidation`, and both covered-period bounds) differ wildly.
/// `gts_type_id` is **not** among them: the referenced GTS type is part of the
/// tuple, so the two records below carry the same one deliberately, and
/// [`a_pdp_permit_narrowed_to_one_meter_admits_it_and_denies_another`] is where
/// varying it is the point. This pins the dedup's projection-correctness
/// premise: share the tuple, share the PDP payload.
///
/// Both period bounds are varied independently, because the tuple key must be
/// blind to the covered period as a whole: a key that admitted either bound
/// would split one PDP decision into two and defeat the dedup.
#[tokio::test]
async fn equal_tuple_keys_produce_equal_pdp_requests_even_when_non_tuple_fields_differ() {
    let record_a = UsageRecord {
        id: Uuid::from_u128(0xAAAA),
        gts_type_id: MeterTypeId::new(SAMPLE_GTS_TYPE_ID).expect("valid gts_type_id"),
        tenant_id: Uuid::from_u128(0xDEAD),
        resource_ref: ResourceRef::new("rsc-shared", "compute.vm").expect("valid resource ref"),
        subject_ref: Some(SubjectRef::new("sub-shared", Some("user")).expect("valid subject")),
        metadata: BTreeMap::new(),
        quantity: qty("1"),
        idempotency_key: IdempotencyKey::new("idem-A").expect("valid idempotency key"),
        accepted_at: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        origin: RecordOrigin::Live,
        invalidation: None,
        window_start: OffsetDateTime::UNIX_EPOCH,
        window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
    };
    let record_b = UsageRecord {
        // Same tuple-key fields, `gts_type_id` among them:
        tenant_id: record_a.tenant_id,
        gts_type_id: record_a.gts_type_id.clone(),
        resource_ref: record_a.resource_ref.clone(),
        subject_ref: record_a.subject_ref.clone(),
        // … wildly different non-tuple fields:
        id: Uuid::from_u128(0xBBBB),
        metadata: BTreeMap::new(),
        quantity: qty("-999"),
        idempotency_key: IdempotencyKey::new("idem-B-different").expect("valid idempotency key"),
        accepted_at: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(96),
        // Admitted by the other ingestion path. The attribution tuple is
        // blind to which route admitted an entry, so an imported record and
        // a live one sharing the tuple must still collapse onto one PDP
        // decision.
        origin: RecordOrigin::Backfill,
        // The one axis that used to be two fields: record B is a withdrawal
        // and record A a measurement, and the tuple key must still collapse
        // them onto one PDP decision.
        invalidation: Some(Invalidation {
            target: Uuid::from_u128(0xCCCC),
            reason: ReasonCode::new("emitter_defect").expect("valid reason code"),
        }),
        window_start: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(24),
        window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(72),
    };

    let key_a = AttributionTupleKey::from_record(&record_a, usage_record::actions::CREATE);
    let key_b = AttributionTupleKey::from_record(&record_b, usage_record::actions::CREATE);
    assert_eq!(
        key_a, key_b,
        "test premise: records were constructed to share the attribution tuple",
    );

    let resolver = CapturingTenantPermitResolver::new();
    let enforcer =
        enforcer_for(Arc::clone(&resolver) as Arc<dyn authz_resolver_sdk::AuthZResolverApi>);

    authorize_attribution_tuple(&enforcer, &NoopMetrics, PdpOp::Ingest, &ctx(), &key_a)
        .await
        .expect("permit");
    let req_a = json(&resolver.take_last_request().expect("captured A"));

    authorize_attribution_tuple(&enforcer, &NoopMetrics, PdpOp::Ingest, &ctx(), &key_b)
        .await
        .expect("permit");
    let req_b = json(&resolver.take_last_request().expect("captured B"));

    assert_eq!(
        req_a, req_b,
        "two records that hash-equal under AttributionTupleKey MUST compose \
         identical PDP EvaluationRequests; any per-record field leaking into \
         the PDP payload defeats the dedup's safety property",
    );
}

/// Same attribution attributes, different `action` MUST NOT hash-equal.
///
/// `CREATE` is the only verb that composes a tuple key today, so this pins the
/// struct's identity contract rather than a request the gear can issue: a
/// second tuple-keyed verb added later cannot silently collapse onto `CREATE`'s
/// decision. Drop `action` from the hash and nothing fails until that verb
/// exists.
#[test]
fn different_actions_yield_distinct_tuple_keys_for_same_attribution() {
    let record = record_with(Some(
        SubjectRef::new("sub-action", Some("user")).expect("valid subject"),
    ));
    let create = AttributionTupleKey::from_record(&record, usage_record::actions::CREATE);
    let get = AttributionTupleKey::from_record(&record, usage_record::actions::GET);
    assert_ne!(
        create, get,
        "action MUST participate in AttributionTupleKey hash/eq; without it \
         a second tuple-keyed verb would share CREATE's PDP decision for the \
         same attribution tuple and silently bypass per-action policy",
    );
}

/// Integration pin: the per-record tenant gate is WIRED into
/// [`authorize_attribution_tuple`], not merely unit-tested via
/// `scope_admits_tenant`. The tenant-echoing happy-path fakes can never produce
/// a scope that fails the gate, so a regression dropping the gate call — a
/// cross-tenant bypass — would otherwise pass the whole suite. The PDP permits
/// but scopes the grant to ONE tenant: a record owned by another MUST be
/// denied, the granted tenant permitted. (Unit coverage of the full-tuple logic
/// is in `attribution_gate_tests`.)
#[tokio::test]
async fn authorize_attribution_tuple_denies_record_outside_granted_tenant() {
    use toolkit_security::pep_properties;

    use crate::domain::DomainError;
    use crate::domain::test_support::CountingPermitResolver;

    let granted = Uuid::from_u128(0x6001);
    let foreign = Uuid::from_u128(0x6002);
    // Permit, but scope the grant to exactly `granted` (independent of the
    // request) — models a `/tenants/{granted}`-scoped caller.
    let resolver =
        CountingPermitResolver::new(pep_properties::OWNER_TENANT_ID, granted.to_string());
    let enforcer = enforcer_for(resolver as Arc<dyn authz_resolver_sdk::AuthZResolverApi>);

    let foreign_key = AttributionTupleKey::from_record(
        &record_with_tenant(foreign),
        usage_record::actions::CREATE,
    );
    let denied =
        authorize_attribution_tuple(&enforcer, &NoopMetrics, PdpOp::Ingest, &ctx(), &foreign_key)
            .await;
    assert!(
        matches!(denied, Err(DomainError::AuthorizationDenied { .. })),
        "a record owned by a tenant outside the PDP-granted scope MUST be denied, got {denied:?}",
    );

    let granted_key = AttributionTupleKey::from_record(
        &record_with_tenant(granted),
        usage_record::actions::CREATE,
    );
    authorize_attribution_tuple(&enforcer, &NoopMetrics, PdpOp::Ingest, &ctx(), &granted_key)
        .await
        .expect("a record owned by the granted tenant is permitted");
}

/// Fails if the feed authorizes the raw path's verb. The reasoning is
/// `authorize_get_usage_record_scope`'s own doc comment, which keeps GET and
/// LIST apart on the ground that "a policy permitting one need not permit
/// the other" — it applies here for the same reason: fusing the charging
/// feed to the audit path means no deployment can grant one without the
/// other.
#[tokio::test]
async fn the_feed_authorizes_read_feed_and_not_list() {
    let resolver = ActionRecordingPermitResolver::new();
    let enforcer =
        enforcer_for(Arc::clone(&resolver) as Arc<dyn authz_resolver_sdk::AuthZResolverApi>);

    let outcome = authorize_read_usage_feed(&enforcer, &NoopMetrics, PdpOp::ReadFeed, &ctx()).await;

    // The deny is the interesting half of this fixture, not an accident. The
    // feed authorizes pre-row, so the PEP request carries no
    // `OWNER_TENANT_ID` for `ActionRecordingPermitResolver` to echo and it
    // falls back to an unconstrained permit, which the gear refuses.
    //
    // The assertion names the property — "an unconstrained permit is not
    // served" — rather than the mechanism, because the mechanism is upstream:
    // `authz_resolver_sdk`'s PEP compiler refuses first, inside
    // `PolicyEnforcer`, with `scope_to_odata_filter`'s `is_unconstrained()`
    // branch behind it as the gear's own second refusal.
    //
    // **Honest limitation:** no mutation confined to this gear flips this to
    // `Ok`, so it is an outcome assertion rather than a mechanism pin. It would
    // catch a future fake or call-site change that started producing an
    // unconstrained permit, but it guards neither local gate.
    assert!(
        matches!(
            outcome,
            Err(crate::domain::DomainError::AuthorizationDenied { .. })
        ),
        "an unconstrained permit must be refused on the feed, not served \
         unscoped across tenants; got {outcome:?}"
    );

    assert_eq!(
        resolver.actions_sorted(),
        vec![usage_record::actions::READ_FEED.to_owned()],
        "the feed must authorize `read_feed`; authorizing `list` would let a grant on the \
         raw audit path silently confer the charging feed"
    );
}

/// Fails if the reconciliation read authorizes the raw path's verb instead of
/// its own. `ActionRecordingPermitResolver` records the `action` every PEP
/// request carried, so this is the one assertion that can tell
/// `authorize_get_reconciliation_metadata` apart from a copy-paste authorizing
/// `list` — the only differentiator on this path, since the resource type,
/// `require_constraints(true)` and the projection gate are all shared with
/// `authorize_list_usage_records`.
#[tokio::test]
async fn the_reconciliation_read_authorizes_reconcile_and_not_list() {
    let resolver = ActionRecordingPermitResolver::new();
    let enforcer =
        enforcer_for(Arc::clone(&resolver) as Arc<dyn authz_resolver_sdk::AuthZResolverApi>);

    let outcome = authorize_get_reconciliation_metadata(
        &enforcer,
        &NoopMetrics,
        PdpOp::Reconciliation,
        &ctx(),
    )
    .await;

    // As in the feed's sibling test: pre-row, so the resolver has no
    // OWNER_TENANT_ID to echo and falls back to an unconstrained permit, which
    // the gear refuses. The denial is the interesting half here too.
    assert!(
        matches!(
            outcome,
            Err(crate::domain::DomainError::AuthorizationDenied { .. })
        ),
        "an unconstrained permit must be refused on the reconciliation path, \
         not served unscoped across tenants; got {outcome:?}"
    );

    assert_eq!(
        resolver.actions_sorted(),
        vec![usage_record::actions::RECONCILE.to_owned()],
        "reconciliation must authorize `reconcile`; authorizing `list` would let a grant \
         on the raw audit path silently confer the operator-only reconciliation surface"
    );
}

/// The `usage_record` action vocabulary, spelled out.
///
/// These strings are a contract against the PDP policy bundle and against
/// `gts/permissions.rs`, which derives one permission instance per action:
/// renaming a constant here silently re-points a deployed grant at a verb no
/// policy mentions, and the compiler cannot see it because both sides read the
/// same constant. Pinning the literals here makes a rename a diff a reviewer
/// must justify.
///
/// Distinctness follows from the literal list rather than a separate
/// assertion, but it is the property that matters: two constants sharing one
/// string would give two permissions the same `(resource_type, action)` pair,
/// and `AttributionTupleKey`'s action-aware hash would stop separating them
/// (pinned by
/// [`different_actions_yield_distinct_tuple_keys_for_same_attribution`]).
#[test]
fn the_usage_record_action_vocabulary_is_six_distinct_spellings() {
    let actions = [
        usage_record::actions::CREATE,
        usage_record::actions::GET,
        usage_record::actions::LIST,
        usage_record::actions::BACKFILL,
        usage_record::actions::READ_FEED,
        usage_record::actions::RECONCILE,
    ];
    assert_eq!(
        actions,
        [
            "create",
            "get",
            "list",
            "backfill",
            "read_feed",
            "reconcile"
        ]
    );
}

/// DESIGN §3.9.6: `reconcile` is a PEP verb distinct from `list`, so a
/// deployment can grant an operator the per-scope counters without
/// conferring the entry-level audit path. Sharing `list`'s string would
/// make the two grants one.
#[test]
fn the_reconcile_verb_is_its_own_action_string() {
    assert_eq!(usage_record::actions::RECONCILE, "reconcile");
    assert_ne!(
        usage_record::actions::RECONCILE,
        usage_record::actions::LIST
    );
}

/// A permit scoped to one tenant must compile to a filter that narrows on
/// that tenant, mirroring `authorize_read_usage_feed`'s pre-row shape.
#[tokio::test]
async fn a_reconciliation_permit_compiles_the_pdp_scope_to_a_narrowing_filter() {
    use toolkit_security::pep_properties;

    let tenant = Uuid::from_u128(0x7001);
    let resolver = CountingPermitResolver::new(pep_properties::OWNER_TENANT_ID, tenant.to_string());
    let enforcer = enforcer_for(resolver as Arc<dyn authz_resolver_sdk::AuthZResolverApi>);

    let scope = authorize_get_reconciliation_metadata(
        &enforcer,
        &NoopMetrics,
        PdpOp::Reconciliation,
        &ctx(),
    )
    .await
    .expect("a permit carrying a tenant constraint must compile");
    assert!(
        format!("{scope:?}").contains("tenant_id"),
        "the compiled scope must narrow on the tenant the PDP returned; a scope that \
         does not is what `require_constraints(true)` exists to refuse"
    );
}

/// `cpt-cf-usage-collector-dod-reconciliation-operator-surface`: "A denial,
/// and a permit that compiles to an empty scope, MUST each fail closed with
/// nothing dispatched to the storage plugin."
#[tokio::test]
async fn an_empty_constraint_set_fails_closed_on_the_reconciliation_path() {
    use crate::domain::DomainError;

    let enforcer =
        enforcer_for(Arc::new(PermitEmptyConstraintsResolver)
            as Arc<dyn authz_resolver_sdk::AuthZResolverApi>);

    let err = authorize_get_reconciliation_metadata(
        &enforcer,
        &NoopMetrics,
        PdpOp::Reconciliation,
        &ctx(),
    )
    .await
    .expect_err("an unconstrained permit must fail closed");
    assert!(
        matches!(err, DomainError::AuthorizationDenied { .. }),
        "got {err:?}"
    );
}

// gts_type_id — scopable on the attribution tuple, reserved on every
// projection of the scope (the four read paths AND the ingestion path's
// invalidation-target lookup)

/// The reason string of a fail-closed denial, or a panic naming what arrived
/// instead. The `gts_type_id` tests assert on the *wording* rather than the
/// variant, because every rejection on these paths is
/// [`DomainError::AuthorizationDenied`] and the variant alone cannot tell
/// "reserved on the scope projection" from "unknown property".
fn denial_reason(err: &crate::domain::DomainError) -> String {
    match err {
        crate::domain::DomainError::AuthorizationDenied {
            reason: Some(reason),
        } => reason.clone(),
        other => panic!("expected an AuthorizationDenied carrying a reason, got {other:?}"),
    }
}

/// Assert a denial names `gts_type_id` as *reserved on the scope projection*
/// and not as an unknown property. Shared by the read paths so they cannot
/// drift on the operator-facing wording. The projection is reached from the
/// ingestion path too — see `service_tests.rs`'s
/// `create_usage_record_under_a_meter_narrowed_grant_measures_but_cannot_withdraw`.
fn assert_denied_as_reserved_meter(err: &crate::domain::DomainError, path: &str) {
    let reason = denial_reason(err);
    assert!(
        reason.contains("reserved"),
        "the {path} denial must name the property as reserved on the scope \
         projection; got {reason:?}"
    );
    assert!(
        reason.contains(usage_record::PROP_GTS_TYPE_ID),
        "the {path} denial must name the offending property; got {reason:?}"
    );
    assert!(
        !reason.contains("unknown property"),
        "the {path} denial must be distinguishable from the unknown-property \
         denial — an operator reading the log has to tell a deliberate \
         reservation from a policy naming an attribute this gear never had; \
         got {reason:?}"
    );
}

/// The PDP request itself carries the meter — the one site no other in-gear
/// assertion can see.
///
/// The composer-comparison tests assert `from_record == from_key`, so an
/// attribute missing from **both** is invisible to them, and the attribution
/// gate reads `key.gts_type_id` rather than the request. Remove
/// `authorize_attribution_tuple`'s
/// `.resource_property(usage_record::PROP_GTS_TYPE_ID, ..)` and nothing else in
/// this crate notices, while the PDP can no longer condition a decision on an
/// attribute it is never sent. DESIGN §3.5's Platform PDP row names the meter
/// in the payload.
#[tokio::test]
async fn the_pdp_request_carries_the_referenced_gts_type() {
    let record = record_with(Some(
        SubjectRef::new("sub-meter", Some("user")).expect("valid subject"),
    ));
    let resolver = CapturingTenantPermitResolver::new();
    let enforcer =
        enforcer_for(Arc::clone(&resolver) as Arc<dyn authz_resolver_sdk::AuthZResolverApi>);

    let key = AttributionTupleKey::from_record(&record, usage_record::actions::CREATE);
    authorize_attribution_tuple(&enforcer, &NoopMetrics, PdpOp::Ingest, &ctx(), &key)
        .await
        .expect("permit");

    let properties = resolver
        .take_last_request()
        .expect("the composer issued a request")
        .resource
        .properties;
    assert_eq!(
        properties
            .get(usage_record::PROP_GTS_TYPE_ID)
            .and_then(serde_json::Value::as_str),
        Some(record.gts_type_id.as_ref()),
        "the PDP request must carry the referenced GTS type: {properties:?}"
    );
}

/// The whole of what advertising `gts_type_id` buys: DESIGN requires "a permit
/// **and** a returned scope that admits the full attribution tuple — tenant,
/// resource, referenced GTS type, and subject where supplied". Exercised
/// through the PDP round-trip rather than a hand-built scope, because an
/// unadvertised property fails the whole constraint in `compile_constraint`,
/// before the gear sees it — which would deny the matching write as readily as
/// the mismatched one. The admitted half is what makes the dimension usable and
/// the pair is what makes it a narrowing.
///
/// It also pins the denial's **wording**: the reason must name the meter rather
/// than send an operator to the tenant policy.
#[tokio::test]
async fn a_pdp_permit_narrowed_to_one_meter_admits_it_and_denies_another() {
    assert_ne!(
        SAMPLE_GTS_TYPE_ID, OTHER_GTS_TYPE_ID,
        "test premise: the granted meter and the attributed one must differ, \
         or the second half asserts nothing"
    );
    let enforcer = enforcer_for(MeterNarrowingPermitResolver::sole(SAMPLE_GTS_TYPE_ID)
        as Arc<dyn authz_resolver_sdk::AuthZResolverApi>);

    let granted = record_with(None);
    assert_eq!(
        granted.gts_type_id.as_ref(),
        SAMPLE_GTS_TYPE_ID,
        "test premise: the shared record fixture is attributed to the granted meter"
    );
    authorize_attribution_tuple(
        &enforcer,
        &NoopMetrics,
        PdpOp::Ingest,
        &ctx(),
        &AttributionTupleKey::from_record(&granted, usage_record::actions::CREATE),
    )
    .await
    .expect("the write attributed to the granted meter must be admitted");

    let other = UsageRecord {
        gts_type_id: MeterTypeId::new(OTHER_GTS_TYPE_ID).expect("valid gts_type_id"),
        ..record_with(None)
    };
    let err = authorize_attribution_tuple(
        &enforcer,
        &NoopMetrics,
        PdpOp::Ingest,
        &ctx(),
        &AttributionTupleKey::from_record(&other, usage_record::actions::CREATE),
    )
    .await
    .expect_err("a scope narrowed to one meter must not admit a write attributed to another");
    assert!(
        matches!(err, crate::domain::DomainError::AuthorizationDenied { .. }),
        "got {err:?}"
    );
    let reason = denial_reason(&err);
    assert!(
        reason.contains(usage_record::PROP_GTS_TYPE_ID),
        "the denial must name the attribute that rejected, not blame the \
         tenant; got {reason:?}"
    );
    assert!(
        !reason.contains("not authorized for usage_record owning tenant"),
        "the record's tenant IS inside the grant — the scope echoes it back — \
         so a tenant-shaped reason would point an operator at the wrong \
         policy; got {reason:?}"
    );
}

/// One bad disjunct must fail the whole projection. Were the meter constraint
/// unadvertised, `compile_constraint` would drop it and the tenant-only sibling
/// would survive, serving the list under a scope strictly wider than the PDP
/// granted; advertised, `scope_to_odata_filter`'s per-constraint `?` fails the
/// whole projection closed — permit → deny, deliberately.
#[tokio::test]
async fn a_meter_constraint_beside_a_sibling_is_denied_not_silently_narrowed_on_list() {
    let enforcer = enforcer_for(MeterNarrowingPermitResolver::with_tenant_only_sibling(
        SAMPLE_GTS_TYPE_ID,
    ) as Arc<dyn authz_resolver_sdk::AuthZResolverApi>);

    let err = authorize_list_usage_records(&enforcer, &NoopMetrics, PdpOp::QueryRaw, &ctx())
        .await
        .expect_err("one bad disjunct must fail the whole projection");
    assert_denied_as_reserved_meter(&err, "list");
}

/// The list path's half of the reserved-property wording. Without it the
/// denial comes from the SDK compiler before the gear, so nothing distinguishes
/// a deliberate reservation from a policy naming an attribute the gear never
/// had.
#[tokio::test]
async fn a_scope_naming_the_meter_is_denied_by_name_on_the_list_path() {
    let enforcer = enforcer_for(MeterNarrowingPermitResolver::sole(SAMPLE_GTS_TYPE_ID)
        as Arc<dyn authz_resolver_sdk::AuthZResolverApi>);

    let err = authorize_list_usage_records(&enforcer, &NoopMetrics, PdpOp::QueryRaw, &ctx())
        .await
        .expect_err("the list projection must refuse the reserved property");
    assert_denied_as_reserved_meter(&err, "list");
}

/// The point lookup's half, at `authorize_get_usage_record_scope`, which
/// authorizes the `get` verb rather than `list`.
#[tokio::test]
async fn a_scope_naming_the_meter_is_denied_by_name_on_the_get_path() {
    let enforcer = enforcer_for(MeterNarrowingPermitResolver::sole(SAMPLE_GTS_TYPE_ID)
        as Arc<dyn authz_resolver_sdk::AuthZResolverApi>);

    let err = authorize_get_usage_record_scope(&enforcer, &NoopMetrics, PdpOp::GetRecord, &ctx())
        .await
        .expect_err("the get projection must refuse the reserved property");
    assert_denied_as_reserved_meter(&err, "get");
}

/// The feed gateway's half.
#[tokio::test]
async fn a_scope_naming_the_meter_is_denied_by_name_on_the_feed_path() {
    let enforcer = enforcer_for(MeterNarrowingPermitResolver::sole(SAMPLE_GTS_TYPE_ID)
        as Arc<dyn authz_resolver_sdk::AuthZResolverApi>);

    let err = authorize_read_usage_feed(&enforcer, &NoopMetrics, PdpOp::ReadFeed, &ctx())
        .await
        .expect_err("the feed projection must refuse the reserved property");
    assert_denied_as_reserved_meter(&err, "read_feed");
}

/// Reconciliation's half.
#[tokio::test]
async fn a_scope_naming_the_meter_is_denied_by_name_on_the_reconciliation_path() {
    let enforcer = enforcer_for(MeterNarrowingPermitResolver::sole(SAMPLE_GTS_TYPE_ID)
        as Arc<dyn authz_resolver_sdk::AuthZResolverApi>);

    let err = authorize_get_reconciliation_metadata(
        &enforcer,
        &NoopMetrics,
        PdpOp::Reconciliation,
        &ctx(),
    )
    .await
    .expect_err("the reconciliation projection must refuse the reserved property");
    assert_denied_as_reserved_meter(&err, "reconcile");
}

// scope_to_odata_filter — projects AccessScope into ODataQuery filter

#[cfg(test)]
mod scope_to_odata_tests {
    use super::SAMPLE_MEMBER_TYPE;
    use toolkit_odata::ast::{CompareOperator, Expr, Value};
    use toolkit_security::{
        AccessScope, InGroupScopeFilter, InTenantSubtreeScopeFilter, ScopeConstraint, ScopeFilter,
        ScopeValue, pep_properties,
    };
    use uuid::Uuid;

    use crate::domain::DomainError;
    use crate::domain::authz::{scope_to_odata_filter, usage_record};

    /// Helper — flatten an Expr to a debuggable s-expression string so
    /// assertions can read at a glance.
    fn fmt_expr(expr: &Expr) -> String {
        match expr {
            Expr::And(a, b) => format!("(and {} {})", fmt_expr(a), fmt_expr(b)),
            Expr::Or(a, b) => format!("(or {} {})", fmt_expr(a), fmt_expr(b)),
            Expr::Not(a) => format!("(not {})", fmt_expr(a)),
            Expr::Compare(lhs, op, rhs) => {
                let op = match op {
                    CompareOperator::Eq => "eq",
                    CompareOperator::Ne => "ne",
                    CompareOperator::Gt => "gt",
                    CompareOperator::Ge => "ge",
                    CompareOperator::Lt => "lt",
                    CompareOperator::Le => "le",
                };
                format!("({} {} {})", op, fmt_expr(lhs), fmt_expr(rhs))
            }
            Expr::In(lhs, vs) => {
                let vs: Vec<_> = vs.iter().map(fmt_expr).collect();
                format!("(in {} [{}])", fmt_expr(lhs), vs.join(" "))
            }
            Expr::Function(name, args) => {
                let args: Vec<_> = args.iter().map(fmt_expr).collect();
                format!("({} {})", name, args.join(" "))
            }
            Expr::Identifier(id) => id.clone(),
            Expr::Value(v) => match v {
                Value::Uuid(u) => format!("uuid:{u}"),
                Value::String(s) => format!("\"{s}\""),
                Value::Bool(b) => format!("{b}"),
                Value::Number(n) => format!("{n}"),
                Value::DateTime(t) => format!("dt:{t}"),
                Value::Date(d) => format!("date:{d}"),
                Value::Time(t) => format!("time:{t}"),
                Value::Null => "null".to_owned(),
            },
        }
    }

    fn uid(seed: u128) -> Uuid {
        Uuid::from_u128(seed)
    }

    #[test]
    fn unconstrained_scope_is_denied_fail_closed() {
        // Under `require_constraints(true)` a legitimate LIST/aggregate permit
        // always carries `OWNER_TENANT_ID In [..]` narrowing (admin included).
        // An `allow_all` scope only arises from a degenerate empty-predicate
        // permit (compiler.rs returns `allow_all()` when every compiled
        // constraint is empty), so emitting "no row narrowing" here would leak
        // every tenant's records. It MUST fail closed, mirroring the per-record
        // gate's `scope_admits_attribution_tuple`.
        let scope = AccessScope::allow_all();
        let err = scope_to_odata_filter(&scope).expect_err("allow_all -> authz denied");
        assert!(matches!(err, DomainError::AuthorizationDenied { .. }));
    }

    #[test]
    fn an_empty_constraint_disjunct_cannot_be_built() {
        // A constraint with no filters matches every row (an allow-all
        // disjunct), and honouring it would widen the projection to all
        // tenants. The shape is unbuildable, so the defence sits at the
        // constructor for every consumer at once.
        assert!(
            ScopeConstraint::try_new(vec![]).is_err(),
            "an empty constraint must not be constructible"
        );
    }

    #[test]
    fn deny_all_scope_lifts_to_authorization_denied() {
        let scope = AccessScope::deny_all();
        let err = scope_to_odata_filter(&scope).expect_err("deny_all -> authz denied");
        assert!(matches!(err, DomainError::AuthorizationDenied { .. }));
    }

    #[test]
    fn single_eq_constraint_projects_to_eq_compare() {
        let tenant = uid(0xAA);
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_TENANT_ID,
            tenant,
        )]));
        let expr = scope_to_odata_filter(&scope).expect("happy path");
        assert_eq!(
            fmt_expr(&expr),
            format!("(eq tenant_id uuid:{tenant})"),
            "OWNER_TENANT_ID must project to `tenant_id eq <uuid>`",
        );
    }

    #[test]
    fn single_in_constraint_projects_to_in_expression() {
        let t1 = uid(1);
        let t2 = uid(2);
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::in_uuids(
            pep_properties::OWNER_TENANT_ID,
            vec![t1, t2],
        )]));
        let expr = scope_to_odata_filter(&scope).unwrap();
        assert_eq!(
            fmt_expr(&expr),
            format!("(in tenant_id [uuid:{t1} uuid:{t2}])"),
        );
    }

    #[test]
    fn multi_filter_constraint_ands_within_a_constraint() {
        let tenant = uid(0xA);
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, tenant),
            ScopeFilter::eq(usage_record::PROP_RESOURCE_TYPE, "compute.vm"),
        ]));
        let expr = scope_to_odata_filter(&scope).unwrap();
        assert_eq!(
            fmt_expr(&expr),
            format!("(and (eq tenant_id uuid:{tenant}) (eq resource_type \"compute.vm\"))"),
        );
    }

    #[test]
    fn multi_constraint_scope_ors_at_top_level() {
        let t1 = uid(11);
        let t2 = uid(22);
        let scope = AccessScope::from_constraints(vec![
            ScopeConstraint::new(vec![ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t1)]),
            ScopeConstraint::new(vec![ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t2)]),
        ]);
        let expr = scope_to_odata_filter(&scope).unwrap();
        assert_eq!(
            fmt_expr(&expr),
            format!("(or (eq tenant_id uuid:{t1}) (eq tenant_id uuid:{t2}))"),
        );
    }

    #[test]
    fn tree_predicates_fail_closed() {
        let tenant = uid(0xBEEF);
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::InTenantSubtree(
            InTenantSubtreeScopeFilter::new(pep_properties::OWNER_TENANT_ID, tenant),
        )]));
        let err = scope_to_odata_filter(&scope).expect_err("tree filter -> deny");
        assert!(matches!(err, DomainError::AuthorizationDenied { .. }));

        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::InGroup(
            InGroupScopeFilter::new_typed(
                "owner_id",
                SAMPLE_MEMBER_TYPE,
                vec![ScopeValue::Uuid(uid(1))],
            ),
        )]));
        let err = scope_to_odata_filter(&scope).expect_err("InGroup -> deny");
        assert!(matches!(err, DomainError::AuthorizationDenied { .. }));
    }

    #[test]
    fn unknown_pep_property_fails_closed() {
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            "unsupported_prop",
            "x",
        )]));
        let err = scope_to_odata_filter(&scope).expect_err("unknown prop -> deny");
        assert!(matches!(err, DomainError::AuthorizationDenied { .. }));
    }

    #[test]
    fn type_mismatch_on_value_fails_closed() {
        // OWNER_TENANT_ID is UUID-typed; a string-typed value is a type
        // mismatch and MUST fail closed rather than silently coercing.
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_TENANT_ID,
            ScopeValue::String("not-a-uuid".into()),
        )]));
        let err = scope_to_odata_filter(&scope).expect_err("string->uuid mismatch");
        assert!(matches!(err, DomainError::AuthorizationDenied { .. }));
    }

    #[test]
    fn uuid_carried_as_string_is_accepted() {
        // The Compiler may emit a UUID as a string ScopeValue; the
        // projection MUST accept it as long as the string parses as a
        // valid UUID (mirrors `ScopeValue::as_uuid`'s convention).
        let t1 = uid(0x1234);
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_TENANT_ID,
            ScopeValue::String(t1.to_string()),
        )]));
        let expr = scope_to_odata_filter(&scope).unwrap();
        assert_eq!(fmt_expr(&expr), format!("(eq tenant_id uuid:{t1})"));
    }

    #[test]
    fn string_pep_property_projects_to_string_value() {
        // A non-tenant string property projects to a string value. Pair it with
        // the OWNER_TENANT_ID pinning every legitimate LIST constraint carries:
        // a tenant-less constraint now fails closed (see
        // `constraint_without_owner_tenant_filter_is_denied`).
        let t = uid(0xAA);
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
            ScopeFilter::eq(usage_record::PROP_SUBJECT_TYPE, "user"),
        ]));
        let expr = scope_to_odata_filter(&scope).unwrap();
        assert_eq!(
            fmt_expr(&expr),
            format!("(and (eq tenant_id uuid:{t}) (eq subject_type \"user\"))"),
        );
    }

    #[test]
    fn uuid_value_on_string_field_projects_to_canonical_string() {
        // A String-typed field (`resource_id`) carrying a `ScopeValue::Uuid`
        // MUST be accepted and rendered to its canonical string: both gates
        // share `coerce_scope_value`, whose policy is that a UUID-shaped
        // `resource_id` matches regardless of how the compiler typed it.
        // Tenant-pinned so the constraint clears
        // `constraint_to_odata_conjunction`'s tenant-narrowing requirement.
        let t = uid(0xAA);
        let u = uid(0x1357);
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
            ScopeFilter::eq(usage_record::PROP_RESOURCE_ID, ScopeValue::Uuid(u)),
        ]));
        let expr = scope_to_odata_filter(&scope).expect("uuid-on-string field accepted");
        assert_eq!(
            fmt_expr(&expr),
            format!("(and (eq tenant_id uuid:{t}) (eq resource_id \"{u}\"))"),
        );
    }

    /// Read at the projection itself. The denial has to be legible as a
    /// *reservation*: the gear advertises `gts_type_id` so the write gate can
    /// compare it, and refuses it here because the plugin's `record_column`
    /// (`query/translate.rs`) is a closed allowlist that deliberately excludes
    /// it. Neither fact has anything to do with a policy naming an attribute
    /// this gear never had, and the log must not render them the same way.
    #[test]
    fn the_reserved_meter_property_is_refused_by_name_on_the_projection() {
        let t = uid(1);
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
            ScopeFilter::eq(usage_record::PROP_GTS_TYPE_ID, super::SAMPLE_GTS_TYPE_ID),
        ]));
        let err = scope_to_odata_filter(&scope)
            .expect_err("the reserved property must fail the projection closed");
        super::assert_denied_as_reserved_meter(&err, "scope_to_odata_filter");
    }

    /// The emission pin, derived from what the projection **emits** rather
    /// than from the prose. The plugin's `record_column` (`translate.rs`) maps
    /// a closed set of identifiers, `gts_type_id` not among them, and an
    /// unmapped field is a translate error rather than an interpolated column —
    /// so a projection emitting it would turn today's deny into a plugin error.
    /// The oracle is the absence of the identifier from any produced filter,
    /// over every shape the projection accepts a property in.
    #[test]
    fn the_reserved_property_never_reaches_a_produced_filter() {
        let t = uid(1);
        let meter = super::SAMPLE_GTS_TYPE_ID;
        let scopes = [
            // Eq, beside the tenant pin the projection also requires.
            AccessScope::single(ScopeConstraint::new(vec![
                ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
                ScopeFilter::eq(usage_record::PROP_GTS_TYPE_ID, meter),
            ])),
            // In — the other filter shape `scope_filter_to_expr` lowers to an
            // identifier, and the one a multi-meter grant would arrive as.
            AccessScope::single(ScopeConstraint::new(vec![
                ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
                ScopeFilter::In(toolkit_security::InScopeFilter::from_values(
                    usage_record::PROP_GTS_TYPE_ID,
                    [meter, super::OTHER_GTS_TYPE_ID],
                )),
            ])),
            // As one disjunct of several: the `?` in the per-constraint loop
            // is what stops the clean sibling from carrying the read through.
            AccessScope::from_constraints(vec![
                ScopeConstraint::new(vec![
                    ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
                    ScopeFilter::eq(usage_record::PROP_GTS_TYPE_ID, meter),
                ]),
                ScopeConstraint::new(vec![ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t)]),
            ]),
        ];
        for scope in &scopes {
            match scope_to_odata_filter(scope) {
                Err(_) => {}
                Ok(expr) => panic!(
                    "the projection produced a filter naming the reserved property: {}",
                    fmt_expr(&expr)
                ),
            }
        }
    }

    #[test]
    fn constraint_without_owner_tenant_filter_is_denied() {
        // A non-empty constraint that narrows ONLY by a non-tenant property has
        // no OWNER_TENANT_ID pinning; honouring it would AND a cross-tenant
        // predicate into the user query. The LIST projection MUST fail closed,
        // mirroring the per-record gate's
        // `scope_without_owner_tenant_filter_is_denied`.
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            usage_record::PROP_RESOURCE_TYPE,
            "compute.vm",
        )]));
        let err = scope_to_odata_filter(&scope).expect_err("tenant-less constraint -> denied");
        assert!(matches!(err, DomainError::AuthorizationDenied { .. }));
    }

    #[test]
    fn multi_constraint_denied_when_any_disjunct_lacks_tenant_pinning() {
        // Constraints are OR-ed independent access paths; one tenant-less path
        // would still widen the projection cross-tenant, so the whole scope
        // fails closed rather than silently dropping the offending disjunct.
        let t = uid(1);
        let scope = AccessScope::from_constraints(vec![
            ScopeConstraint::new(vec![ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t)]),
            ScopeConstraint::new(vec![ScopeFilter::eq(
                usage_record::PROP_RESOURCE_TYPE,
                "compute.vm",
            )]),
        ]);
        let err = scope_to_odata_filter(&scope).expect_err("tenant-less disjunct -> denied");
        assert!(matches!(err, DomainError::AuthorizationDenied { .. }));
    }
}

// scope_admits_attribution_tuple — per-record attribution gate applied after a
// permit. Verifies the record's *full* attribution tuple (tenant + any other
// narrowing predicate the PDP returned) satisfies the granted scope, not just
// the owning tenant.

#[cfg(test)]
mod attribution_gate_tests {
    use super::SAMPLE_MEMBER_TYPE;
    use toolkit_security::{
        AccessScope, InGroupScopeFilter, InTenantSubtreeScopeFilter, ScopeConstraint, ScopeFilter,
        ScopeValue, pep_properties,
    };
    use usage_collector_sdk::{ResourceRef, UsageRecord};
    use uuid::Uuid;

    use super::super::{
        AttributionTupleKey, TupleRejection, scope_admits_attribution_tuple, usage_record,
    };

    /// The rejection a denied tuple carries, or a panic if the gate admitted.
    ///
    /// The `.is_ok()` / `.is_err()` assertions below pin the gate's *verdict*;
    /// this is how the cases whose **classification** is load-bearing pin that
    /// too. `authorize_attribution_tuple` renders the classification into the
    /// operator-facing reason, so a wrong one sends a reader to the wrong
    /// policy while the suite stays green.
    fn rejection_for(scope: &AccessScope, key: &AttributionTupleKey) -> TupleRejection {
        scope_admits_attribution_tuple(scope, key)
            .expect_err("the gate must deny for this scope/record pair")
    }

    fn uid(seed: u128) -> Uuid {
        Uuid::from_u128(seed)
    }

    /// A tuple key whose only meaningful attribute for tenant-only scopes is the
    /// owning tenant (resource attributes come from the shared record builder).
    fn key_for_tenant(tenant: Uuid) -> AttributionTupleKey {
        AttributionTupleKey::from_record(
            &super::record_with_tenant(tenant),
            usage_record::actions::CREATE,
        )
    }

    /// A tuple key with caller-chosen tenant and resource reference, for driving
    /// the non-tenant predicate paths.
    fn key_with_resource(
        tenant: Uuid,
        resource_id: &str,
        resource_type: &str,
    ) -> AttributionTupleKey {
        let record = UsageRecord {
            tenant_id: tenant,
            resource_ref: ResourceRef::new(resource_id, resource_type).expect("valid resource ref"),
            ..super::record_with(None)
        };
        AttributionTupleKey::from_record(&record, usage_record::actions::CREATE)
    }

    /// A tuple key with caller-chosen tenant and referenced GTS type, for
    /// driving the meter predicate the write gate gained with ruling G2.
    fn key_for_meter(tenant: Uuid, gts_type_id: &str) -> AttributionTupleKey {
        let record = UsageRecord {
            tenant_id: tenant,
            gts_type_id: usage_collector_sdk::MeterTypeId::new(gts_type_id)
                .expect("valid gts_type_id"),
            ..super::record_with(None)
        };
        AttributionTupleKey::from_record(&record, usage_record::actions::CREATE)
    }

    #[test]
    fn unconstrained_scope_is_denied_fail_closed() {
        // Under require_constraints(true) a legitimate permit always carries an
        // OWNER_TENANT_ID In[..] narrowing (a Global-scoped admin resolves to
        // In[all tenants], NOT allow_all — covered by the In-closure cases). An
        // unconstrained scope here only arises from a degenerate empty-predicate
        // permit, so the per-record gate MUST fail closed rather than admit
        // every tenant, matching the LIST path's fail-closed handling.
        assert!(
            scope_admits_attribution_tuple(&AccessScope::allow_all(), &key_for_tenant(uid(0xAB)))
                .is_err()
        );
    }

    #[test]
    fn deny_all_scope_admits_no_tenant() {
        assert!(
            scope_admits_attribution_tuple(&AccessScope::deny_all(), &key_for_tenant(uid(0xAB)))
                .is_err()
        );
    }

    #[test]
    fn tenant_within_in_closure_is_admitted() {
        let a = uid(1);
        let b = uid(2);
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::in_uuids(
            pep_properties::OWNER_TENANT_ID,
            vec![a, b],
        )]));
        assert!(scope_admits_attribution_tuple(&scope, &key_for_tenant(a)).is_ok());
        assert!(scope_admits_attribution_tuple(&scope, &key_for_tenant(b)).is_ok());
    }

    #[test]
    fn tenant_outside_in_closure_is_denied() {
        // The cross-tenant case: caller scoped to {a}, record names some
        // other tenant -> fail closed.
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::in_uuids(
            pep_properties::OWNER_TENANT_ID,
            vec![uid(1)],
        )]));
        assert!(scope_admits_attribution_tuple(&scope, &key_for_tenant(uid(0xC))).is_err());
    }

    #[test]
    fn tenant_as_uuid_string_value_is_admitted() {
        // The compiler may emit a UUID as a String ScopeValue; the gate MUST
        // accept it (mirrors scope_value_to_ast / AccessScope::contains_uuid).
        let t = uid(0x1234);
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_TENANT_ID,
            ScopeValue::String(t.to_string()),
        )]));
        assert!(scope_admits_attribution_tuple(&scope, &key_for_tenant(t)).is_ok());
    }

    #[test]
    fn scope_without_owner_tenant_filter_is_denied() {
        // A constrained permit that narrows ONLY by some other property (no
        // OWNER_TENANT_ID filter) MUST NOT admit — the gate requires the
        // record's owning tenant to be pinned by the granted scope, even when
        // the non-tenant predicate happens to match. Fail closed.
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            usage_record::PROP_RESOURCE_TYPE,
            "compute.vm",
        )]));
        assert!(
            scope_admits_attribution_tuple(
                &scope,
                &key_with_resource(uid(0xAB), "rsc-eq", "compute.vm")
            )
            .is_err()
        );
    }

    #[test]
    fn multi_constraint_disjunction_admits_when_any_path_covers_tenant() {
        // Constraints are OR-ed (independent access paths); a record tenant
        // covered by ANY path is admitted.
        let a = uid(1);
        let b = uid(2);
        let scope = AccessScope::from_constraints(vec![
            ScopeConstraint::new(vec![ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, a)]),
            ScopeConstraint::new(vec![ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, b)]),
        ]);
        assert!(scope_admits_attribution_tuple(&scope, &key_for_tenant(a)).is_ok());
        assert!(scope_admits_attribution_tuple(&scope, &key_for_tenant(b)).is_ok());
        assert!(scope_admits_attribution_tuple(&scope, &key_for_tenant(uid(3))).is_err());
    }

    // --- full-tuple gating: non-tenant predicates must constrain ---

    #[test]
    fn constraint_with_satisfied_resource_id_narrowing_is_admitted() {
        // Tenant pinned AND the record's resource_id matches the narrowing the
        // PDP returned -> admit. Guards against over-correction (the gate must
        // not deny a record the PDP actually granted).
        let t = uid(1);
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
            ScopeFilter::eq(usage_record::PROP_RESOURCE_ID, "granted-rsc"),
        ]));
        assert!(
            scope_admits_attribution_tuple(
                &scope,
                &key_with_resource(t, "granted-rsc", "compute.vm")
            )
            .is_ok()
        );
    }

    #[test]
    fn constraint_with_violated_resource_id_narrowing_is_denied() {
        // The defect the full-tuple gate closes: the permit pins the tenant
        // AND narrows resource_id to a value the record does NOT carry. A
        // tenant-only gate ignores the resource_id filter and grants more
        // than the PDP intended (within tenant); the full-tuple gate MUST
        // honour every filter and deny.
        let t = uid(1);
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
            ScopeFilter::eq(usage_record::PROP_RESOURCE_ID, "granted-rsc"),
        ]));
        assert!(
            scope_admits_attribution_tuple(
                &scope,
                &key_with_resource(t, "other-rsc", "compute.vm")
            )
            .is_err()
        );
    }

    /// The denial's classification follows **tenant coverage**, not the order
    /// the PDP happened to list its predicates in.
    ///
    /// The gate renders this into the operator-facing reason, so a `Property`
    /// verdict asserts something: *"your grant for tenant T does not admit
    /// this entry's `p`"* tells the reader the grant covers `T`. For a
    /// cross-tenant record it does not, and the tenant is the real blocker —
    /// which is what the `bool` this verdict replaced used to say, correctly,
    /// for that case.
    ///
    /// The oracle is the **pair** of orders in (b) and (c): one scope, one
    /// record, two spellings of the same constraint must classify the same way.
    /// (a) is the other half of the rule, and stops "always `Tenant`" passing.
    /// Classification by the first rejecting filter fails (c).
    #[test]
    fn the_rejected_attribute_is_classified_by_tenant_coverage_not_by_filter_order() {
        let granted_tenant = uid(1);
        let other_tenant = uid(2);
        assert_ne!(
            granted_tenant, other_tenant,
            "test premise: the granted tenant and the record's must differ, or \
             (b) and (c) are not cross-tenant at all"
        );
        assert_ne!(
            "granted-rsc", "other-rsc",
            "test premise: the granted resource and the record's must differ, \
             or nothing rejects on the property"
        );

        // (a) Inside the tenant, mismatched on the property: the grant really
        //     does cover this tenant, so naming `resource_id` is true.
        let inside = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, granted_tenant),
            ScopeFilter::eq(usage_record::PROP_RESOURCE_ID, "granted-rsc"),
        ]));
        assert_eq!(
            rejection_for(
                &inside,
                &key_with_resource(granted_tenant, "other-rsc", "compute.vm")
            ),
            TupleRejection::Property(usage_record::PROP_RESOURCE_ID.to_owned()),
            "a record the grant's tenant covers must be reported against the \
             attribute that actually narrowed it away"
        );

        // (b) and (c): same scope, same record, the two filter orders.
        let outside = key_with_resource(other_tenant, "other-rsc", "compute.vm");
        let tenant_first = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, granted_tenant),
            ScopeFilter::eq(usage_record::PROP_RESOURCE_ID, "granted-rsc"),
        ]));
        let property_first = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(usage_record::PROP_RESOURCE_ID, "granted-rsc"),
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, granted_tenant),
        ]));
        assert_eq!(
            rejection_for(&tenant_first, &outside),
            TupleRejection::Tenant,
            "a cross-tenant record is blocked by the tenant, whatever else the \
             constraint also narrows by"
        );
        assert_eq!(
            rejection_for(&property_first, &outside),
            TupleRejection::Tenant,
            "a cross-tenant record must read as cross-tenant however the PDP \
             ordered the constraint's filters"
        );
    }

    /// An unevaluable predicate **over the tenant** is a tenant rejection, not
    /// a property one.
    ///
    /// `is_owner_tenant_filter` answers "does this pin the tenant", and a tree
    /// predicate deliberately never pins (`authz.rs`'s own doc on it). Reusing
    /// that answer to classify the *rejection* renders "your grant for tenant X
    /// does not admit this entry's `owner_tenant_id`" — the tenant named twice,
    /// once as granted and once as the thing refusing it.
    ///
    /// Both shapes are covered because they fail differently: the lone tree
    /// predicate pins no tenant at all, while the second constraint pins one
    /// and still must not name `owner_tenant_id` as the narrowing property.
    #[test]
    fn an_unevaluable_tenant_predicate_is_classified_as_tenant_not_as_a_property() {
        let t = uid(1);
        let tree_only =
            AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::InTenantSubtree(
                InTenantSubtreeScopeFilter::new(pep_properties::OWNER_TENANT_ID, t),
            )]));
        assert_eq!(
            rejection_for(&tree_only, &key_for_tenant(t)),
            TupleRejection::Tenant,
            "a tenant subtree this flat gate cannot evaluate is a tenant rejection"
        );

        let flat_pin_and_tree = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
            ScopeFilter::InTenantSubtree(InTenantSubtreeScopeFilter::new(
                pep_properties::OWNER_TENANT_ID,
                t,
            )),
        ]));
        assert_eq!(
            rejection_for(&flat_pin_and_tree, &key_for_tenant(t)),
            TupleRejection::Tenant,
            "the flat filter pins the tenant, but the only thing that rejected \
             is still on the tenant dimension, so the reason must not offer \
             `owner_tenant_id` as a narrowing property"
        );
    }

    /// The write-gate half of the meter dimension, read directly off
    /// `scope_admits_attribution_tuple`. DESIGN requires a write to satisfy
    /// "the full attribution tuple — tenant, resource, referenced GTS type, and
    /// subject where supplied".
    ///
    /// The oracle is the **pair**: a scope narrowed to one meter must admit
    /// that meter and reject the other. Each half alone is satisfied by a gate
    /// that answers the same way for everything.
    #[test]
    fn a_constraint_narrowed_to_one_meter_admits_it_and_denies_another() {
        assert_ne!(
            super::SAMPLE_GTS_TYPE_ID,
            super::OTHER_GTS_TYPE_ID,
            "test premise: the granted meter and the rejected one must differ"
        );
        let t = uid(1);
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
            ScopeFilter::eq(usage_record::PROP_GTS_TYPE_ID, super::SAMPLE_GTS_TYPE_ID),
        ]));
        assert!(
            scope_admits_attribution_tuple(&scope, &key_for_meter(t, super::SAMPLE_GTS_TYPE_ID))
                .is_ok(),
            "a scope narrowed to the record's own meter must admit it"
        );
        assert!(
            scope_admits_attribution_tuple(&scope, &key_for_meter(t, super::OTHER_GTS_TYPE_ID))
                .is_err(),
            "a scope narrowed to one meter must not admit a write attributed to another"
        );
    }

    #[test]
    fn constraint_carrying_unknown_property_fails_closed_even_with_tenant_match() {
        // A permit narrowed by a property this gear doesn't understand cannot be
        // evaluated against a flat per-record tuple; as defensive as the LIST
        // path, the gate refuses to admit on the strength of the tenant filter
        // alone.
        let t = uid(1);
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
            ScopeFilter::eq("unsupported_prop", "x"),
        ]));
        assert!(scope_admits_attribution_tuple(&scope, &key_for_tenant(t)).is_err());
    }

    #[test]
    fn constraint_carrying_tree_predicate_fails_closed_even_with_tenant_match() {
        // usage_records is a flat resource with no group/closure membership; a
        // tree predicate alongside the tenant filter is unevaluable here and
        // MUST fail closed (mirrors scope_to_odata_filter's rejection).
        let t = uid(1);
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
            ScopeFilter::InGroup(InGroupScopeFilter::new_typed(
                "owner_id",
                SAMPLE_MEMBER_TYPE,
                vec![ScopeValue::Uuid(uid(9))],
            )),
        ]));
        assert!(scope_admits_attribution_tuple(&scope, &key_for_tenant(t)).is_err());
    }

    #[test]
    fn disjunction_admits_via_clean_constraint_despite_unevaluable_sibling() {
        // Constraints are OR-ed. A record covered by a clean tenant constraint
        // is admitted even when a SIBLING constraint carries an unevaluable
        // predicate — dropping the bad disjunct only ever narrows access, never
        // over-grants. (Pins the per-constraint fail-closed choice over a
        // whole-scope hard error.)
        let t = uid(1);
        let scope = AccessScope::from_constraints(vec![
            ScopeConstraint::new(vec![ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t)]),
            ScopeConstraint::new(vec![ScopeFilter::InGroup(InGroupScopeFilter::new_typed(
                "owner_id",
                SAMPLE_MEMBER_TYPE,
                vec![ScopeValue::Uuid(uid(9))],
            ))]),
        ]);
        assert!(scope_admits_attribution_tuple(&scope, &key_for_tenant(t)).is_ok());
    }
}

// Cross-gate leaf consistency
//
// The per-record gate (`scope_admits_attribution_tuple`) and the LIST
// projection (`scope_to_odata_filter`) share two leaf decisions: the recognized
// PEP property set and the `ScopeValue` coercion. They legitimately DIFFER at
// the policy layer — tenant-must-be-pinned is point-gate-only, bad disjuncts
// drop on the point side but fail the whole scope on the LIST side, and only
// the point gate matches a concrete record's values. So this suite pins
// agreement on the *leaf* verdict alone: a single tenant-pinned constraint,
// evaluated against a record built to carry the canonical value, must be
// accepted-or-rejected the same way by both gates.

#[cfg(test)]
mod gate_leaf_consistency_tests {
    use toolkit_security::{
        AccessScope, InTenantSubtreeScopeFilter, ScopeConstraint, ScopeFilter, ScopeValue,
        pep_properties,
    };
    use usage_collector_sdk::{ResourceRef, UsageRecord};
    use uuid::Uuid;

    use super::super::{
        AttributionTupleKey, scope_admits_attribution_tuple, scope_to_odata_filter, usage_record,
    };

    fn uid(seed: u128) -> Uuid {
        Uuid::from_u128(seed)
    }

    /// A key whose attribution tuple carries exactly the given tenant /
    /// `resource_id` / `resource_type`, so a *coercible* filter value also
    /// MATCHES the record. That isolates the structural accept/deny (unknown
    /// property, value typing) from a mere value mismatch.
    fn key(tenant: Uuid, resource_id: &str, resource_type: &str) -> AttributionTupleKey {
        let record = UsageRecord {
            tenant_id: tenant,
            resource_ref: ResourceRef::new(resource_id, resource_type).expect("valid resource ref"),
            ..super::record_with(None)
        };
        AttributionTupleKey::from_record(&record, usage_record::actions::CREATE)
    }

    #[test]
    fn gates_agree_on_leaf_verdict_across_scope_corpus() {
        let t = uid(0x7000);
        let u = uid(0x9999);
        let rid_uuid = u.to_string();

        // (label, single constraint, matching key). Tenant is pinned in every
        // case so the point gate *can* admit; whether it does then turns purely
        // on the shared leaf decisions, which is what we assert agreement on.
        let cases: Vec<(&str, ScopeConstraint, AttributionTupleKey)> = vec![
            (
                "tenant: uuid value",
                ScopeConstraint::new(vec![ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t)]),
                key(t, "rsc", "compute.vm"),
            ),
            (
                "tenant: uuid-as-string value",
                ScopeConstraint::new(vec![ScopeFilter::eq(
                    pep_properties::OWNER_TENANT_ID,
                    ScopeValue::String(t.to_string()),
                )]),
                key(t, "rsc", "compute.vm"),
            ),
            (
                "tenant: non-uuid string value (type mismatch)",
                ScopeConstraint::new(vec![ScopeFilter::eq(
                    pep_properties::OWNER_TENANT_ID,
                    ScopeValue::String("not-a-uuid".into()),
                )]),
                key(t, "rsc", "compute.vm"),
            ),
            (
                "resource_id: string value",
                ScopeConstraint::new(vec![
                    ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
                    ScopeFilter::eq(usage_record::PROP_RESOURCE_ID, "rsc"),
                ]),
                key(t, "rsc", "compute.vm"),
            ),
            (
                "resource_id: uuid value on a string field (the drift case)",
                ScopeConstraint::new(vec![
                    ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
                    ScopeFilter::eq(usage_record::PROP_RESOURCE_ID, ScopeValue::Uuid(u)),
                ]),
                key(t, &rid_uuid, "compute.vm"),
            ),
            (
                "resource_type: uuid value on a string field (the drift case)",
                ScopeConstraint::new(vec![
                    ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
                    ScopeFilter::eq(usage_record::PROP_RESOURCE_TYPE, ScopeValue::Uuid(u)),
                ]),
                key(t, "rsc", &rid_uuid),
            ),
            (
                "unknown property",
                ScopeConstraint::new(vec![
                    ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
                    ScopeFilter::eq("unsupported_prop", "x"),
                ]),
                key(t, "rsc", "compute.vm"),
            ),
            (
                "tree predicate alongside tenant",
                ScopeConstraint::new(vec![
                    ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
                    ScopeFilter::InTenantSubtree(InTenantSubtreeScopeFilter::new(
                        pep_properties::OWNER_TENANT_ID,
                        t,
                    )),
                ]),
                key(t, "rsc", "compute.vm"),
            ),
        ];

        for (label, constraint, key) in cases {
            let scope = AccessScope::single(constraint);
            let list_accepts = scope_to_odata_filter(&scope).is_ok();
            let point_admits = scope_admits_attribution_tuple(&scope, &key).is_ok();
            assert_eq!(
                list_accepts, point_admits,
                "LIST projection and per-record gate disagree on the leaf verdict for \
                 `{label}`: list_accepts={list_accepts}, point_admits={point_admits}",
            );
        }
    }
}
