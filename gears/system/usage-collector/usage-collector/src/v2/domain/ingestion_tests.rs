//! Unit tests for the ingestion pipeline (`Service::create_usage_records` /
//! `Service::backfill_usage_records`) built in [`crate::v2::domain::ingestion`].
//!
//! Coverage: the PDP and `gts_type_id` dedup pre-passes and the
//! invalidation-target pre-check that collapse a batch's per-record calls
//! into one round-trip per distinct key; the single-submission create path
//! driven one pipeline stage at a time (PDP deny, plugin-reported transient,
//! a target that resolves to nothing or is itself a withdrawal, the copy /
//! metadata rules, the dedup conflict of an invalidation, and the happy
//! path); the batch size-cap guard; the server-assigned `id` / `origin` /
//! `accepted_at` stamps; the covered-period preconditions on both the batch
//! path and the live service's two-sided bound; the backfill route's
//! `origin`-driven differences from the live path; and that ingestion
//! resolves a meter's declared type the same way the aggregate read path
//! does.

use usage_collector_sdk::v2::UsageRecordFilterField;

use crate::v2::domain::test_support::HappyPathPlugin;

/// Assert that the scope the service last handed `get_usage_record` is one a
/// conforming backend can translate.
///
/// The question is "does it translate", not "what shape is it", which a
/// `Debug`-string assertion cannot draw: this reads
/// [`HappyPathPlugin::last_get_scope_expr`] and puts the real converter
/// behind the answer. It reads the LAST capture, so every caller sits after
/// an assertion pinning `get_usage_record_calls()` to exactly 1; a test over
/// several targets must pin the count too.
fn assert_translatable_scope(plugin: &HappyPathPlugin, call_site: &str) {
    let scope = plugin
        .last_get_scope_expr()
        .expect("the target pre-read must have dispatched to the plugin");

    toolkit_odata::filter::convert_expr_to_filter_node::<UsageRecordFilterField>(&scope)
        .unwrap_or_else(|err| {
            unreachable!(
                "the scope `{call_site}` hands `get_usage_record` must be translatable \
                 by a conforming backend; `{scope:?}` was refused as {err:?}"
            )
        });
}

// ── PDP dedup pre-pass in `create_usage_records` ───────────────────────────
//
// Records sharing the same attribution tuple
// `(tenant_id, resource_type, resource_id, subject_id, subject_type)` MUST
// collapse to a single PDP `evaluate` round-trip, projected onto every input
// index in the group.
#[cfg(test)]
mod pdp_dedup_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use usage_collector_sdk::v2::{
        CreateUsageRecord, EntryType, IdempotencyKey, MeterTypeId, RecordOrigin, ResourceRef,
        SubjectRef, UsageCollectorPluginV2, UsageRecord,
    };
    use uuid::Uuid;

    use crate::v2::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, fake_declaration_source_with_fold,
        recent_window_end, recent_window_start,
    };

    const HAPPY_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");

    /// A [`ServiceFixture`] with an arbitrary working declaration (these tests
    /// exercise PDP dedup, not the declaration), exposing the
    /// [`CountingTenantPermitResolver`] handle.
    fn service_with_counting_permit(
        plugin: Arc<dyn UsageCollectorPluginV2>,
        suffix: &str,
    ) -> (
        Arc<crate::v2::domain::Service>,
        Arc<crate::v2::domain::test_support::CountingTenantPermitResolver>,
    ) {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build_with_default_resolver_handle(plugin, suffix)
    }

    fn persisted_record(tenant_id: Uuid, resource_id: &str, idem: &str) -> UsageRecord {
        UsageRecord {
            id: Uuid::new_v4(),
            gts_type_id: MeterTypeId::new(HAPPY_GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new(resource_id, "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::v2::domain::test_support::qty("1"),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            accepted_at: recent_window_end(),
            origin: RecordOrigin::Live,
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    fn input_record(tenant_id: Uuid, resource_id: &str, idem: &str) -> CreateUsageRecord {
        // Distinct `idem` values keep records distinct even when they share an
        // attribution tuple.
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(HAPPY_GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new(resource_id, "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::v2::domain::test_support::qty("1"),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    fn input_record_with_subject(
        tenant_id: Uuid,
        resource_id: &str,
        subject_id: &str,
        idem: &str,
    ) -> CreateUsageRecord {
        let mut r = input_record(tenant_id, resource_id, idem);
        r.subject_ref =
            Some(SubjectRef::new(subject_id, None::<String>).expect("valid subject ref"));
        r
    }

    /// Five records, identical attribution tuple → exactly one PDP
    /// `evaluate` round-trip.
    #[tokio::test]
    async fn create_usage_records_collapses_pdp_calls_for_identical_attribution_tuple() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xAA);
        let input: Vec<CreateUsageRecord> = (0..5)
            .map(|i| input_record(tenant_id, "rsc-shared", &format!("idem-shared-{i}")))
            .collect();

        plugin.set_create_records(
            input
                .iter()
                .map(|r| Ok(persisted_record(r.tenant_id, "rsc-shared", "idem-persist")))
                .collect(),
        );

        let (service, resolver) = service_with_counting_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.pdp_dedup.shared_tuple.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 5);
        assert!(
            results.iter().all(Result::is_ok),
            "every record MUST be accepted under a permit-by-default PDP",
        );

        assert_eq!(
            resolver.calls(),
            1,
            "5 records sharing the same attribution tuple MUST collapse to a single PDP evaluate call (intra-batch dedup); observed {} calls",
            resolver.calls(),
        );
    }

    /// Three records, three distinct `resource_id` values → exactly three PDP
    /// calls: one call per distinct tuple, no dedup possible.
    #[tokio::test]
    async fn create_usage_records_issues_one_pdp_call_per_distinct_attribution_tuple() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xBB);
        let input = vec![
            input_record(tenant_id, "rsc-A", "idem-A"),
            input_record(tenant_id, "rsc-B", "idem-B"),
            input_record(tenant_id, "rsc-C", "idem-C"),
        ];

        plugin.set_create_records(
            input
                .iter()
                .map(|r| {
                    Ok(persisted_record(
                        r.tenant_id,
                        r.resource_ref.resource_id(),
                        r.idempotency_key.as_str(),
                    ))
                })
                .collect(),
        );

        let (service, resolver) = service_with_counting_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.pdp_dedup.distinct_tuples.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(Result::is_ok));

        assert_eq!(
            resolver.calls(),
            3,
            "3 distinct attribution tuples MUST produce 3 PDP evaluate calls; observed {} calls",
            resolver.calls(),
        );
    }

    /// Mixed batch (two records share tuple A, two share tuple B) → exactly
    /// two PDP calls. Pins the projection step: the second call's decision
    /// MUST apply to every input index whose tuple key matches.
    #[tokio::test]
    async fn create_usage_records_projects_pdp_decision_across_shared_tuple_groups() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xCC);
        let input = vec![
            input_record(tenant_id, "rsc-A", "idem-A-0"),
            input_record(tenant_id, "rsc-B", "idem-B-0"),
            input_record(tenant_id, "rsc-A", "idem-A-1"),
            input_record(tenant_id, "rsc-B", "idem-B-1"),
        ];

        plugin.set_create_records(
            input
                .iter()
                .map(|r| {
                    Ok(persisted_record(
                        r.tenant_id,
                        r.resource_ref.resource_id(),
                        r.idempotency_key.as_str(),
                    ))
                })
                .collect(),
        );

        let (service, resolver) = service_with_counting_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.pdp_dedup.mixed_groups.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 4);
        assert!(results.iter().all(Result::is_ok));

        assert_eq!(
            resolver.calls(),
            2,
            "2 distinct attribution tuples carrying 4 records MUST produce 2 PDP evaluate calls; observed {} calls",
            resolver.calls(),
        );
    }

    /// Deny-side projection: when PDP denies one tuple group, every input index
    /// in that group MUST surface as a rejected record, and every index in a
    /// permitted group MUST proceed.
    #[tokio::test]
    async fn create_usage_records_projects_pdp_deny_across_shared_tuple_groups() {
        use crate::v2::domain::service::Service;
        use crate::v2::domain::test_support::{
            DenyOneResourceResolver, enforcer_for, hub_with_plugin,
        };

        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xEE);
        // Two tuple groups, two records each: rsc-DENY and rsc-OK.
        let input = vec![
            input_record(tenant_id, "rsc-DENY", "idem-D-0"),
            input_record(tenant_id, "rsc-OK", "idem-K-0"),
            input_record(tenant_id, "rsc-DENY", "idem-D-1"),
            input_record(tenant_id, "rsc-OK", "idem-K-1"),
        ];

        // Only the two permitted records reach the plugin SPI.
        plugin.set_create_records(
            input
                .iter()
                .filter(|r| r.resource_ref.resource_id() == "rsc-OK")
                .map(|r| {
                    Ok(persisted_record(
                        r.tenant_id,
                        r.resource_ref.resource_id(),
                        r.idempotency_key.as_str(),
                    ))
                })
                .collect(),
        );

        let hub = hub_with_plugin(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.pdp_dedup.deny_projection.records.v1",
            "constructorfabric",
        );
        let enforcer = enforcer_for(DenyOneResourceResolver::new("rsc-DENY"));
        let type_resolver =
            std::sync::Arc::new(crate::v2::domain::type_resolver::TypeResolver::new(
                fake_declaration_source_with_fold("SUM"),
                crate::v2::domain::type_resolver::TypeResolverConfig {
                    ttl: std::time::Duration::from_mins(1),
                    capacity: 16,
                },
                Arc::new(crate::v2::domain::ports::metrics::NoopMetrics),
            ));
        let service = Arc::new(Service::new_with_metrics(
            hub,
            "constructorfabric".to_owned(),
            enforcer,
            Arc::new(crate::v2::domain::ports::metrics::NoopMetrics),
            type_resolver,
            crate::v2::domain::test_support::inert_reverse_resolver(Arc::new(
                crate::v2::domain::ports::metrics::NoopMetrics,
            )),
            crate::v2::domain::service::ServiceConfig::default(),
        ));

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded - per-record outcomes carry deny");

        assert_eq!(results.len(), 4);
        for (idx, label) in [(0_usize, "rsc-DENY first"), (2, "rsc-DENY second")] {
            let err = results[idx]
                .as_ref()
                .expect_err(&format!("{label} record MUST surface a per-record deny"));
            assert!(
                matches!(
                    err,
                    usage_collector_sdk::v2::UsageCollectorError::PermissionDenied { .. }
                ),
                "{label}: expected PermissionDenied, got {err:?}",
            );
        }
        for (idx, label) in [(1_usize, "rsc-OK first"), (3, "rsc-OK second")] {
            results[idx].as_ref().unwrap_or_else(|err| {
                unreachable!("{label} record MUST be accepted (got {err:?})")
            });
        }
    }

    /// Subject-presence asymmetry: a record WITH a `subject_ref` and a record
    /// WITHOUT one MUST be treated as distinct attribution tuples (`subject_id`
    /// is part of the tuple key only when present).
    #[tokio::test]
    async fn create_usage_records_distinguishes_subject_presence_in_tuple_key() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xDD);
        let input = vec![
            input_record(tenant_id, "rsc-X", "idem-X-0"),
            input_record_with_subject(tenant_id, "rsc-X", "subject-1", "idem-X-1"),
            input_record_with_subject(tenant_id, "rsc-X", "subject-1", "idem-X-2"),
        ];

        plugin.set_create_records(
            input
                .iter()
                .map(|r| {
                    Ok(persisted_record(
                        r.tenant_id,
                        r.resource_ref.resource_id(),
                        r.idempotency_key.as_str(),
                    ))
                })
                .collect(),
        );

        let (service, resolver) = service_with_counting_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.pdp_dedup.subject_presence.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(Result::is_ok));

        assert_eq!(
            resolver.calls(),
            2,
            "subject-absent and subject-present records form distinct tuple keys; observed {} calls",
            resolver.calls(),
        );
    }
}

// ── gts_type_id dedup pre-pass in `create_usage_records` ───────────────────
//
// Records sharing the same meter `gts_type_id` MUST collapse to a single Type
// Resolver round-trip, projected onto every input index referencing that id.
#[cfg(test)]
mod gts_type_id_dedup_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use usage_collector_sdk::v2::{
        CreateUsageRecord, EntryType, IdempotencyKey, Invalidation, MeterTypeId, RecordOrigin,
        ResourceRef, USAGE_RECORD_RESOURCE, UsageCollectorError, UsageCollectorPluginV2,
        UsageRecord,
    };
    use uuid::Uuid;

    use crate::v2::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, fake_declaration_source_counting,
        fake_declaration_source_with_one_unresolvable, recent_window_end, recent_window_start,
    };

    const GTS_A: &str = gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");
    const GTS_B: &str = gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_emitted.v1~");
    const GTS_C: &str = gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_buffered.v1~");

    fn meter_id_for(gts: &str) -> MeterTypeId {
        MeterTypeId::new(gts).expect("valid meter type id")
    }

    fn record_for(gts: &str, tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: meter_id_for(gts),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-gts-dedup", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::v2::domain::test_support::qty("1"),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    fn persisted_for(input: &CreateUsageRecord) -> UsageRecord {
        UsageRecord {
            id: Uuid::new_v4(),
            gts_type_id: input.gts_type_id.clone(),
            tenant_id: input.tenant_id,
            resource_ref: input.resource_ref.clone(),
            subject_ref: input.subject_ref.clone(),
            metadata: input.metadata.clone(),
            quantity: input.quantity,
            idempotency_key: input.idempotency_key.clone(),
            accepted_at: input.window_end,
            origin: RecordOrigin::Live,
            // The submission states the reason alone; the persisted shape
            // pairs it with the gateway-derived target.
            invalidation: input.invalidation.clone().map(|reason| Invalidation {
                target: crate::v2::domain::invalidation::derive_invalidation_target(input)
                    .expect("fixture withdrawal carries a key"),
                reason,
            }),
            window_start: input.window_start,
            window_end: input.window_end,
        }
    }

    // The "5 records, identical gts_type_id → exactly one resolution" case
    // lives in `ingestion_declared_type_tests::a_batch_resolves_each_distinct_type_once`.

    /// Three records, three distinct `gts_type_id`s → exactly three
    /// declaration resolutions (one per distinct id), asking the resolver
    /// for exactly the distinct ids in the batch.
    #[tokio::test]
    async fn create_usage_records_issues_one_resolution_per_distinct_gts_type_id() {
        let plugin = HappyPathPlugin::new();
        let source = fake_declaration_source_counting();

        let tenant_id = Uuid::from_u128(0xFF);
        let input = vec![
            record_for(GTS_A, tenant_id, "idem-A"),
            record_for(GTS_B, tenant_id, "idem-B"),
            record_for(GTS_C, tenant_id, "idem-C"),
        ];

        plugin.set_create_records(input.iter().map(|r| Ok(persisted_for(r))).collect());

        let service = ServiceFixture::default().with_source(source.clone()).build(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.gts_dedup.distinct.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(Result::is_ok));

        assert_eq!(
            source.fetch_calls(),
            3,
            "3 distinct gts_type_ids MUST produce 3 declaration resolutions; \
             observed {} calls",
            source.fetch_calls(),
        );

        let mut seen: Vec<String> = source
            .fetch_inputs()
            .into_iter()
            .map(|id| id.to_string())
            .collect();
        seen.sort();
        let mut expected = vec![
            meter_id_for(GTS_A).to_string(),
            meter_id_for(GTS_B).to_string(),
            meter_id_for(GTS_C).to_string(),
        ];
        expected.sort();
        assert_eq!(
            seen, expected,
            "the deduped fan-out MUST ask the resolver for exactly the distinct meter ids",
        );
    }

    /// Mixed batch: `gts_type_id` A resolves, `gts_type_id` B does not.
    /// Records sharing the unresolvable id are all rejected with
    /// `NotFound`, records sharing the resolvable id are accepted.
    #[tokio::test]
    async fn create_usage_records_projects_not_found_to_every_record_sharing_unknown_gts_type_id() {
        let plugin = HappyPathPlugin::new();
        let source = fake_declaration_source_with_one_unresolvable(meter_id_for(GTS_B));

        let tenant_id = Uuid::from_u128(0x11);
        let input = vec![
            record_for(GTS_A, tenant_id, "idem-A-0"),
            record_for(GTS_B, tenant_id, "idem-B-0"),
            record_for(GTS_A, tenant_id, "idem-A-1"),
            record_for(GTS_B, tenant_id, "idem-B-1"),
        ];

        // Only the two resolvable-gts_type_id records reach the SPI.
        let accepted: Vec<_> = input
            .iter()
            .filter(|r| r.gts_type_id.to_string() == GTS_A)
            .map(|r| Ok(persisted_for(r)))
            .collect();
        plugin.set_create_records(accepted);

        let service = ServiceFixture::default()
            .with_source(Arc::clone(&source)
                as Arc<dyn crate::v2::domain::ports::declarations::DeclarationSource>)
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
                "test.gts_dedup.mixed.records.v1",
            );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 4);

        assert!(
            results[0].is_ok(),
            "record at index 0 (gts_type_id A) MUST be accepted, got {:?}",
            results[0],
        );
        assert!(
            results[2].is_ok(),
            "record at index 2 (gts_type_id A) MUST be accepted, got {:?}",
            results[2],
        );
        let expected_name = meter_id_for(GTS_B).to_string();
        for idx in [1usize, 3usize] {
            match results[idx].as_ref() {
                Err(UsageCollectorError::NotFound {
                    resource_type,
                    name,
                    ..
                }) => {
                    assert_eq!(*resource_type, USAGE_RECORD_RESOURCE);
                    assert_eq!(
                        name, &expected_name,
                        "record at index {idx} MUST be rejected with NotFound \
                         carrying the unresolvable meter id",
                    );
                }
                other => unreachable!(
                    "record at index {idx} (gts_type_id B) MUST surface NotFound, \
                     got {other:?}",
                ),
            }
        }

        assert_eq!(
            source.fetch_calls(),
            2,
            "2 distinct gts_type_ids carrying 4 records MUST produce 2 declaration \
             resolutions; observed {} calls",
            source.fetch_calls(),
        );
    }
}

// ── invalidation-target pre-check in `create_usage_records` ───────────────
//
// Entries naming the same target MUST collapse to a single `get_usage_record`
// SPI round-trip, an ordinary measurement MUST NOT trigger a target read at
// all, and the per-index pairing the fan-out produces must stay correct.
#[cfg(test)]
mod invalidation_target_batch_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use usage_collector_sdk::v2::{
        ConflictOutcome, CreateUsageRecord, EntryType, IdempotencyKey, Invalidation, MetadataKey,
        MeterTypeId, ReasonCode, ResourceRef, USAGE_RECORD_RESOURCE, UsageCollectorError,
        UsageCollectorPluginError, UsageCollectorPluginV2, UsageRecord, ValidationReason,
    };
    use uuid::Uuid;

    use super::assert_translatable_scope;
    use crate::v2::domain::Service;
    use crate::v2::domain::test_support::{
        HappyPathPlugin, ServiceFixture, UncompilableSiblingPermitResolver, authenticated_ctx,
        counter_sum_with_label, fake_declaration_source_with_fold,
        fake_declaration_source_with_metadata, projected, recent_window_end, recent_window_start,
    };

    const COUNTER_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");

    /// A [`ServiceFixture`] with an arbitrary working declaration — these
    /// tests exercise the target pre-check, not the declaration.
    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV2>, suffix: &str) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(plugin, suffix)
    }

    /// The base measurement every fixture here is shaped from. An
    /// invalidation is a faithful copy of it, so a withdrawal is built by
    /// changing exactly the two permitted departures — `entry_type` and the
    /// reason code.
    fn ordinary_record(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(COUNTER_GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-target", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::v2::domain::test_support::qty("10"),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    /// The withdrawal of `ordinary_record(tenant_id, idem)`.
    ///
    /// It names no target: the shape has no field for one. It repeats every
    /// identity input of the entry it withdraws, idempotency key included, and
    /// the gateway derives the target from those. Passing `idem` therefore
    /// chooses *which* entry is withdrawn; [`target_id`] says which.
    fn withdrawal_of(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Invalidation,
            invalidation: Some(ReasonCode::new("emitter_defect").expect("valid reason code")),
            ..ordinary_record(tenant_id, idem)
        }
    }

    /// The persisted measurement `withdrawal_of(tenant_id, idem)` withdraws:
    /// the projection of the very submission that withdrawal copies, so the
    /// pair cannot drift apart.
    fn target_row(tenant_id: Uuid, idem: &str) -> UsageRecord {
        projected(&ordinary_record(tenant_id, idem))
    }

    /// The identifier `withdrawal_of(tenant_id, idem)` resolves.
    fn target_id(tenant_id: Uuid, idem: &str) -> Uuid {
        target_row(tenant_id, idem).id
    }

    /// Five withdrawals of one target MUST collapse to a single
    /// `get_usage_record` SPI round-trip.
    #[tokio::test]
    async fn withdrawals_of_one_target_share_a_single_lookup() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x501);
        plugin.set_get_record(target_row(tenant_id, "idem-w"));

        let input: Vec<CreateUsageRecord> =
            (0..5).map(|_| withdrawal_of(tenant_id, "idem-w")).collect();
        // All five share one dedup identity, so the batch dispatches a single
        // representative; the mock is programmed with one outcome to match.
        let stored = projected(&input[0]);
        plugin.set_create_records(vec![Ok(stored.clone())]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.target.shared.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 5);
        assert!(
            results.iter().all(Result::is_ok),
            "every faithful withdrawal MUST be accepted by the gateway: {results:?}",
        );

        assert_eq!(
            plugin.get_usage_record_calls(),
            1,
            "5 withdrawals of one target MUST collapse to a single \
             get_usage_record SPI dispatch; observed {} calls",
            plugin.get_usage_record_calls(),
        );
        assert_eq!(
            plugin
                .last_create_records_input()
                .expect("dispatched")
                .len(),
            1,
            "one dispatch per identity: 5 withdrawals sharing one dedup identity \
             collapse to a single create_usage_records dispatch",
        );

        assert_translatable_scope(&plugin, "resolve_invalidation_targets");
    }

    /// Three distinct targets MUST produce three `get_usage_record`
    /// dispatches, for exactly those three ids.
    #[tokio::test]
    async fn distinct_targets_each_cost_their_own_lookup() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x502);

        let target_a = target_id(tenant_id, "idem-A");
        let target_b = target_id(tenant_id, "idem-B");
        let target_c = target_id(tenant_id, "idem-C");
        for idem in ["idem-A", "idem-B", "idem-C"] {
            plugin.set_get_record_for(target_id(tenant_id, idem), target_row(tenant_id, idem));
        }

        let input = vec![
            withdrawal_of(tenant_id, "idem-A"),
            withdrawal_of(tenant_id, "idem-B"),
            withdrawal_of(tenant_id, "idem-C"),
        ];
        plugin.set_create_records(input.iter().map(|r| Ok(projected(r))).collect());

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.target.distinct.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(Result::is_ok), "{results:?}");

        assert_eq!(
            plugin.get_usage_record_calls(),
            3,
            "3 distinct targets MUST produce 3 get_usage_record SPI dispatches; \
             observed {} calls",
            plugin.get_usage_record_calls(),
        );

        let mut seen = plugin.get_usage_record_inputs();
        seen.sort();
        let mut expected = vec![target_a, target_b, target_c];
        expected.sort();
        assert_eq!(
            seen, expected,
            "the deduped fan-out MUST ask the plugin for exactly the distinct targets",
        );
    }

    /// An ordinary measurement MUST cost no target lookup at all. The
    /// *absence* of the dispatch is the pin: a gateway that looked up
    /// unconditionally would pass every other test here while doubling the
    /// plugin traffic of the common path.
    #[tokio::test]
    async fn an_ordinary_record_costs_no_target_lookup() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x503);
        let input: Vec<CreateUsageRecord> = (0..5)
            .map(|i| ordinary_record(tenant_id, &format!("idem-ord-{i}")))
            .collect();
        plugin.set_create_records(input.iter().map(|r| Ok(projected(r))).collect());

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.target.no_invalidates.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 5);
        assert!(results.iter().all(Result::is_ok), "{results:?}");

        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "an entry carrying no `invalidates` MUST NOT trigger a target lookup; \
             observed {} get_usage_record dispatches",
            plugin.get_usage_record_calls(),
        );
    }

    /// A target that resolves to nothing MUST reject every entry naming it, at
    /// its own input index, while an entry naming a resolvable target in the
    /// same batch is still accepted. The rejections sit at indices 0 and 2
    /// with the acceptance between them, so a projection that wrote every
    /// outcome to one slot cannot pass.
    #[tokio::test]
    async fn an_unresolvable_target_rejects_every_entry_naming_it() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x504);

        let good = target_id(tenant_id, "idem-good");
        let missing = target_id(tenant_id, "idem-bad");
        plugin.set_get_record_for(good, target_row(tenant_id, "idem-good"));
        plugin.set_get_usage_record_not_found(missing);

        // Indices 0 and 2 withdraw the same entry, so they repeat one key and
        // resolve one identifier.
        let input = vec![
            withdrawal_of(tenant_id, "idem-bad"),
            withdrawal_of(tenant_id, "idem-good"),
            withdrawal_of(tenant_id, "idem-bad"),
        ];

        // Only the resolvable one reaches the persist SPI.
        plugin.set_create_records(vec![Ok(projected(&input[1]))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.target.mixed_not_found.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 3);

        assert!(
            results[1].is_ok(),
            "the entry naming a resolvable target MUST be accepted, got {:?}",
            results[1],
        );
        for idx in [0usize, 2] {
            match results[idx].as_ref() {
                Err(UsageCollectorError::NotFound {
                    resource_type,
                    name,
                    detail,
                    ..
                }) if *resource_type == USAGE_RECORD_RESOURCE => {
                    assert_eq!(
                        name,
                        &missing.to_string(),
                        "index {idx} MUST name the unresolvable target",
                    );
                    assert!(
                        detail.contains(&missing.to_string()),
                        "index {idx} detail MUST carry the unresolvable target",
                    );
                }
                other => unreachable!("index {idx} MUST be rejected as NotFound, got {other:?}"),
            }
        }

        assert_eq!(
            plugin.get_usage_record_calls(),
            2,
            "2 distinct targets carrying 3 entries MUST produce 2 get_usage_record \
             dispatches; observed {} calls",
            plugin.get_usage_record_calls(),
        );
    }

    /// Two withdrawals of **different** targets, resolved in one batch, MUST
    /// each be told about the target *they* named. The comparator trusts that
    /// the row and reference it is handed belong together, so the fan-out —
    /// keyed by target, projected back by input index — is the only place they
    /// can come apart. The entries fail in different *kinds* of way, so a swap
    /// of key or index changes which reason lands where.
    #[tokio::test]
    async fn each_rejection_names_the_target_its_own_entry_sent() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x505);

        // Target A resolves to nothing at all.
        let target_a = target_id(tenant_id, "idem-pair-a");
        plugin.set_get_usage_record_not_found(target_a);

        // Target B exists, and the submission copies it unfaithfully.
        let target_b = target_id(tenant_id, "idem-pair-b");
        let mut row_b = target_row(tenant_id, "idem-pair-b");
        row_b.quantity = crate::v2::domain::test_support::qty("999");
        plugin.set_get_record_for(target_b, row_b);

        let input = vec![
            withdrawal_of(tenant_id, "idem-pair-a"),
            withdrawal_of(tenant_id, "idem-pair-b"),
        ];

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.target.pairing.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 2);

        match results[0].as_ref() {
            Err(UsageCollectorError::NotFound { name, .. }) => assert_eq!(
                name,
                &target_a.to_string(),
                "index 0 MUST be told about the target IT resolved",
            ),
            other => unreachable!("index 0 MUST be refused as a missing target, got {other:?}"),
        }
        match results[1].as_ref() {
            Err(UsageCollectorError::InvalidArgument {
                reason: ValidationReason::InvalidationFieldMismatch,
                field,
                resource_name,
                ..
            }) => {
                assert_eq!(field, "quantity", "the field that differs MUST be named");
                assert_eq!(
                    resource_name.as_deref(),
                    Some(target_b.to_string().as_str()),
                    "index 1 MUST be told about the target IT resolved",
                );
            }
            other => unreachable!("index 1 MUST be refused as an unfaithful copy, got {other:?}"),
        }
    }

    /// A submission that breaks the copy rule **and** the metadata rule is
    /// told about the copy.
    ///
    /// Error priority: the target rules run before the declaration's. The
    /// metadata a caller would be told to fix is metadata it has to copy
    /// from the target regardless, so telling it about the metadata first
    /// sends it to fix the wrong thing.
    #[tokio::test]
    async fn a_copy_mismatch_outranks_a_metadata_rejection() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x506);
        let target = target_id(tenant_id, "idem-both-broken");

        // The declaration declares `region` alone, so `zone` is undeclared and
        // fails the closed-shape check. `metadata` is no identity input, so
        // adding it below leaves the resolved target unchanged.
        let mut row = target_row(tenant_id, "idem-both-broken");
        row.quantity = crate::v2::domain::test_support::qty("999");
        plugin.set_get_record_for(target, row);

        let mut submission = withdrawal_of(tenant_id, "idem-both-broken");
        submission.metadata.insert(
            MetadataKey::new("zone").expect("valid metadata key"),
            "eu-1".to_owned(),
        );

        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(&["region"]))
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
                "test.target.priority.records.v1",
            );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::InvalidArgument { reason, .. }) => assert_eq!(
                *reason,
                ValidationReason::InvalidationFieldMismatch,
                "the copy rejection MUST outrank the metadata one",
            ),
            other => {
                unreachable!("a submission breaking both rules MUST be rejected, got {other:?}")
            }
        }
    }

    /// A second withdrawal under another reason code reaches the same dedup
    /// identity: `reason_code` is no identity input, so two withdrawals of
    /// one entry agree on all six. The store reports an ordinary conflict
    /// carrying the stored invalidation, and the gateway reports it as
    /// `AlreadyInvalidated` because the dispatched entry is an invalidation.
    #[tokio::test]
    async fn a_store_conflict_on_a_withdrawal_is_lifted_as_already_invalidated() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x507);
        let target = target_id(tenant_id, "idem-withdrawn");
        plugin.set_get_record_for(target, target_row(tenant_id, "idem-withdrawn"));
        let stored = projected(&withdrawal_of(tenant_id, "idem-withdrawn"));
        let stored_id = stored.id;
        // The conflict carries the key the withdrawal was dispatched under —
        // its target's own; no prefix is reserved.
        plugin.set_create_records(vec![Err(UsageCollectorPluginError::idempotency_conflict(
            usage_collector_sdk::v2::IdempotencyKey::new("idem-withdrawn")
                .expect("valid idempotency key"),
            crate::v2::domain::test_support::as_stored(stored),
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.target.already_invalidated.records.v1",
        );

        let mut second = withdrawal_of(tenant_id, "idem-withdrawn");
        second.invalidation = Some(ReasonCode::new("late_correction").expect("valid reason code"));

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![second])
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::Conflict {
                outcome:
                    ConflictOutcome::AlreadyInvalidated {
                        invalidated_by,
                        reason_code,
                    },
                name,
                ..
            }) => {
                assert_eq!(name, &target.to_string());
                assert_eq!(*invalidated_by, stored_id);
                assert_eq!(reason_code.as_str(), "emitter_defect");
            }
            other => unreachable!(
                "a conflict on a dispatched invalidation is AlreadyInvalidated, got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn a_store_conflict_on_a_record_stays_an_idempotency_conflict() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x508);
        let stored = projected(&ordinary_record(tenant_id, "idem-conflict"));
        let stored_id = stored.id;
        plugin.set_create_records(vec![Err(UsageCollectorPluginError::idempotency_conflict(
            usage_collector_sdk::v2::IdempotencyKey::new("idem-conflict")
                .expect("valid idempotency key"),
            crate::v2::domain::test_support::as_stored(stored),
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.record.idempotency_conflict.records.v1",
        );
        let mut divergent = ordinary_record(tenant_id, "idem-conflict");
        divergent.quantity = crate::v2::domain::test_support::qty("11");

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![divergent])
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::Conflict {
                outcome: ConflictOutcome::IdempotencyConflict,
                name,
                ..
            }) => assert_eq!(name, &stored_id.to_string()),
            other => {
                unreachable!("a conflict on a record stays IdempotencyConflict, got {other:?}")
            }
        }
    }

    /// A backend fault on the target read MUST fail the submission, not
    /// reject it: an unreadable target is not an absent one, and a caller
    /// told `NotFound` would stop retrying a withdrawal that is perfectly
    /// valid.
    #[tokio::test]
    async fn a_transient_target_lookup_fails_the_entry_rather_than_rejecting_it() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x508);
        plugin.set_get_usage_record_transient("target store timed out", Some(7));

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.target.transient.records.v1",
        );

        let results = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![withdrawal_of(tenant_id, "idem-transient")],
            )
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::ServiceUnavailable {
                detail,
                retry_after_seconds,
            }) => {
                assert_eq!(detail, "target store timed out");
                assert_eq!(*retry_after_seconds, Some(7));
            }
            other => {
                unreachable!("a transient target read MUST NOT become a rejection, got {other:?}")
            }
        }
    }

    /// The plugin MUST see the batch in submission order, even though the
    /// target pre-check pushes its verified entries onto the end of
    /// `eligible`. Per-entry results are routed back by input index either
    /// way, so `last_create_records_input` is the only place the property is
    /// observable; the invalidation is at index 0, so removing the sort
    /// reverses the pair rather than leaving it untouched.
    #[tokio::test]
    async fn the_plugin_sees_the_batch_in_submission_order() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x50A);
        plugin.set_get_record_for(
            target_id(tenant_id, "idem-order-withdrawal"),
            target_row(tenant_id, "idem-order-withdrawal"),
        );

        let input = vec![
            withdrawal_of(tenant_id, "idem-order-withdrawal"),
            ordinary_record(tenant_id, "idem-order-plain"),
        ];
        plugin.set_create_records(input.iter().map(|r| Ok(projected(r))).collect());
        let expected_ids: Vec<Uuid> = input.iter().map(|r| projected(r).id).collect();

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.target.order.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        let result_ids: Vec<Uuid> = results
            .iter()
            .map(|result| result.as_ref().expect("accepted").id)
            .collect();
        assert_eq!(
            result_ids, expected_ids,
            "each result is the row for the input at its own position",
        );

        let dispatched = plugin
            .last_create_records_input()
            .expect("the batch reached the persist SPI");
        let keys: Vec<&str> = dispatched
            .iter()
            .map(|r| r.idempotency_key.as_str())
            .collect();
        assert_eq!(
            keys,
            vec!["idem-order-withdrawal", "idem-order-plain"],
            "the deferred target pre-check MUST NOT reorder the batch the \
             plugin is handed, and a withdrawal is dispatched under its \
             target's own key",
        );
    }

    /// A transient on one entry's target MUST NOT abandon the entries
    /// behind it in the pending list.
    ///
    /// The backend-fault arm projects per entry and continues; a version that
    /// returned instead would leave every later slot unfilled. The unreadable
    /// target is first, so the survivors are exactly what a `return` drops.
    #[tokio::test]
    async fn a_transient_on_one_target_does_not_abandon_the_rest_of_the_batch() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x50B);

        let unreadable = target_id(tenant_id, "idem-iso-unreadable");
        plugin.set_get_usage_record_transient_for(unreadable, "target store timed out", Some(7));
        plugin.set_get_record_for(
            target_id(tenant_id, "idem-iso-readable"),
            target_row(tenant_id, "idem-iso-readable"),
        );

        let input = vec![
            withdrawal_of(tenant_id, "idem-iso-unreadable"),
            withdrawal_of(tenant_id, "idem-iso-readable"),
        ];
        plugin.set_create_records(vec![Ok(projected(&input[1]))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.target.isolation.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 2);

        match results[0].as_ref() {
            Err(UsageCollectorError::ServiceUnavailable { detail, .. }) => {
                assert_eq!(detail, "target store timed out");
            }
            other => unreachable!("index 0's target was unreadable, got {other:?}"),
        }
        assert!(
            results[1].is_ok(),
            "a backend fault on one entry's target MUST NOT reject — or drop — \
             an entry whose own target read succeeded: {:?}",
            results[1],
        );
    }

    /// A batch MUST label each entry with **its own** `entry_type`.
    ///
    /// The label is captured per input index before the pipeline consumes the
    /// submissions and re-joined to the outcomes by `zip` afterwards; that
    /// join is the only place a label can come apart from its entry, and no
    /// single-emit test reaches it.
    #[tokio::test]
    async fn a_batch_labels_each_entry_with_its_own_entry_type() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x50C);
        plugin.set_get_record_for(
            target_id(tenant_id, "idem-label-withdrawal"),
            target_row(tenant_id, "idem-label-withdrawal"),
        );

        let input = vec![
            ordinary_record(tenant_id, "idem-label-plain"),
            withdrawal_of(tenant_id, "idem-label-withdrawal"),
        ];
        plugin.set_create_records(input.iter().map(|r| Ok(projected(r))).collect());

        let (service, provider, exporter) = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build_with_metrics(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
                "test.target.batch_label.records.v1",
            );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        provider.force_flush().unwrap();

        assert_eq!(
            counter_sum_with_label(
                &exporter,
                "uc_ingestion_records_total",
                "entry_type",
                "record"
            ),
            1,
        );
        assert_eq!(
            counter_sum_with_label(
                &exporter,
                "uc_ingestion_records_total",
                "entry_type",
                "invalidation",
            ),
            1,
        );
    }

    /// A plugin answering `get_usage_record(a)` with a *different* row MUST be
    /// refused, not believed. The submission is a faithful copy of the row the
    /// plugin returns, so without the id check every other rule passes and the
    /// entry is **accepted**. A host-invariant breach, so `Internal`, and the
    /// message names only the id the caller sent — the row came back from an
    /// unscoped read and its own identity must not cross back.
    #[tokio::test]
    async fn a_plugin_answering_with_the_wrong_row_is_refused_not_believed() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x50D);
        let requested = target_id(tenant_id, "idem-wrong-row");
        let answered = target_id(tenant_id, "idem-other-row");

        // Asked for `requested`, answered with the row for `answered`. The two
        // rows differ in their idempotency key alone, which the copy
        // comparator does not read — so only the id check can catch this.
        plugin.set_get_record_for(requested, target_row(tenant_id, "idem-other-row"));

        // The persist SPI is programmed to succeed even though a correct
        // gateway never reaches it, so a gateway that admitted the entry fails
        // visibly as `Ok(..)` rather than on an unprogrammed SPI.
        let submission = withdrawal_of(tenant_id, "idem-wrong-row");
        plugin.set_create_records(vec![Ok(projected(&submission))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.target.wrong_row.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::Internal { detail }) => {
                assert!(
                    detail.contains(&requested.to_string()),
                    "the breach MUST name the id the caller sent: {detail}",
                );
                assert!(
                    !detail.contains(&answered.to_string()),
                    "the breach MUST NOT echo the row's own identity: {detail}",
                );
            }
            other => unreachable!(
                "a mis-answering plugin MUST NOT have its row believed — this \
                 submission copies it faithfully and would otherwise be \
                 accepted; got {other:?}",
            ),
        }
        assert!(
            plugin.last_create_records_input().is_none(),
            "the breach MUST short-circuit before the persist SPI",
        );
    }

    /// A plugin answering the derived identifier with a row that is itself an
    /// invalidation MUST be refused, not believed: the identifier was derived
    /// with `entry_type = record`, so such a row contradicts it. The row is a
    /// faithful copy in every compared field, so without this guard the entry
    /// is **accepted** and a withdrawal of a withdrawal is persisted. A
    /// host-invariant breach, so `Internal`, naming only the identifier the
    /// gateway asked for.
    #[tokio::test]
    async fn a_plugin_answering_with_an_invalidation_row_is_refused_not_believed() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x50E);
        let target = target_id(tenant_id, "idem-corrupt-row");
        let mut row = target_row(tenant_id, "idem-corrupt-row");
        row.invalidation = Some(Invalidation {
            target: Uuid::from_u128(0x61F),
            reason: ReasonCode::new("emitter_defect").expect("valid reason code"),
        });
        assert_eq!(
            row.id, target,
            "test premise: the row answers the derived id"
        );
        plugin.set_get_record_for(target, row);

        // Programmed so a gateway that believed the row fails visibly as
        // `Ok(..)` rather than on an unprogrammed SPI.
        let submission = withdrawal_of(tenant_id, "idem-corrupt-row");
        plugin.set_create_records(vec![Ok(projected(&submission))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.target.invalidation_row.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::Internal { detail }) => assert!(
                detail.contains(&target.to_string()),
                "the breach MUST name the id the gateway asked for: {detail}",
            ),
            other => {
                unreachable!(
                    "a row contradicting its own identifier MUST NOT be believed; got {other:?}"
                )
            }
        }
        assert!(
            plugin.last_create_records_input().is_none(),
            "the breach MUST short-circuit before the persist SPI",
        );
    }

    /// One measurement, one good withdrawal and one bad withdrawal MUST
    /// come back index-aligned, with only the bad one rejected — and the
    /// bad one is last, so a projection that wrote to a fixed slot could
    /// not pass.
    #[tokio::test]
    async fn a_mixed_batch_rejects_only_the_entry_that_broke_a_rule() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x509);

        plugin.set_get_record_for(
            target_id(tenant_id, "idem-mixed-good"),
            target_row(tenant_id, "idem-mixed-good"),
        );
        let mut row_bad = target_row(tenant_id, "idem-mixed-bad");
        row_bad.quantity = crate::v2::domain::test_support::qty("999");
        plugin.set_get_record_for(target_id(tenant_id, "idem-mixed-bad"), row_bad);

        let input = vec![
            ordinary_record(tenant_id, "idem-mixed-plain"),
            withdrawal_of(tenant_id, "idem-mixed-good"),
            withdrawal_of(tenant_id, "idem-mixed-bad"),
        ];
        plugin.set_create_records(vec![Ok(projected(&input[0])), Ok(projected(&input[1]))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.target.mixed.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 3);
        assert!(results[0].is_ok(), "{:?}", results[0]);
        assert!(results[1].is_ok(), "{:?}", results[1]);
        assert!(
            matches!(
                results[2].as_ref(),
                Err(UsageCollectorError::InvalidArgument {
                    reason: ValidationReason::InvalidationFieldMismatch,
                    ..
                })
            ),
            "only the unfaithful copy MUST be rejected, and at its own index: {:?}",
            results[2],
        );
    }

    #[tokio::test]
    async fn a_target_not_yet_converged_is_a_retryable_conflict() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5C1);
        let target = target_id(tenant_id, "idem-nc");
        plugin.set_get_usage_record_not_converged(target);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.target.not_converged.records.v1",
        );

        let results = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![withdrawal_of(tenant_id, "idem-nc")],
            )
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::Conflict {
                outcome:
                    ConflictOutcome::TargetNotConverged {
                        retry_after_seconds,
                    },
                name,
                ..
            }) => {
                assert_eq!(name, &target.to_string());
                // This is the one test covering the path that reaches
                // `lift_domain_error` via `resolve_invalidation_targets`'s
                // generic `Some(Err(e))` fallback, so the delay is asserted
                // here rather than absorbed by `..`.
                assert_eq!(
                    *retry_after_seconds,
                    Some(crate::v2::domain::service::DEFAULT_TARGET_NOT_CONVERGED_RETRY_AFTER_SECS),
                    "a not-converged target reached via the batch pre-pass must carry the \
                     configured target_not_converged_retry_after_secs delay"
                );
            }
            other => unreachable!("a not-converged target is TargetNotConverged, got {other:?}"),
        }
        assert_eq!(plugin.get_usage_record_converged_only_flags(), vec![true]);
        assert!(
            plugin.last_create_records_input().is_none(),
            "nothing is dispatched"
        );
    }

    /// The target is read under the scope compiled from the submitter's own
    /// `create` permit, so another tenant's row answers exactly like an absent
    /// one, never with a field mismatch that would confirm it exists.
    #[tokio::test]
    async fn a_withdrawal_naming_another_tenants_row_answers_as_absent() {
        let plugin = HappyPathPlugin::new();
        let caller_tenant = Uuid::from_u128(0x5A1);
        let other_tenant = Uuid::from_u128(0x5A2);
        let target = target_id(caller_tenant, "idem-cross");
        plugin.set_get_record_for(target, target_row(other_tenant, "idem-cross"));
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.target.cross_tenant.records.v1",
        );

        let results = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![withdrawal_of(caller_tenant, "idem-cross")],
            )
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::NotFound {
                reason: usage_collector_sdk::v2::NotFoundReason::InvalidationTargetNotFound,
                name,
                ..
            }) => assert_eq!(name, &target.to_string()),
            other => unreachable!("an out-of-scope target must read as absent, got {other:?}"),
        }
        assert_translatable_scope(&plugin, "resolve_invalidation_targets");
        let scope = plugin.last_get_scope().expect("the lookup dispatched");
        assert!(
            scope.contains(&format!("{caller_tenant:?}"))
                && !scope.contains(&format!("{target:?}")),
            "the lookup reads under the caller's compiled permit, not `id eq <target>`: {scope}"
        );
    }

    /// A permit the per-entry gate admits but whose scope does not compile
    /// fails the withdrawal closed as `PermissionDenied`, before any target
    /// read, while an ordinary record in the same PDP group is still
    /// accepted. The ordinary record is what proves the gate admitted the
    /// group: a denial there would refuse both entries.
    #[tokio::test]
    async fn a_withdrawal_under_a_permit_scope_that_does_not_compile_is_permission_denied() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5C1);
        plugin.set_get_record(target_row(tenant_id, "idem-uncompilable-withdrawal"));
        let ordinary = ordinary_record(tenant_id, "idem-uncompilable-plain");
        let stored = projected(&ordinary);
        plugin.set_create_records(vec![Ok(stored.clone())]);
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .with_resolver(Arc::new(UncompilableSiblingPermitResolver))
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
                "test.target.uncompilable_scope.records.v1",
            );

        let results = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![
                    withdrawal_of(tenant_id, "idem-uncompilable-withdrawal"),
                    ordinary,
                ],
            )
            .await
            .expect("batch dispatch succeeded");

        assert!(
            matches!(
                results[0],
                Err(UsageCollectorError::PermissionDenied { .. })
            ),
            "a permit scope that does not compile fails the withdrawal closed, got {:?}",
            results[0],
        );
        assert_eq!(
            results[1]
                .as_ref()
                .expect("the ordinary record is accepted"),
            &stored,
        );
        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "no target is read under a scope that does not compile",
        );
        assert_eq!(
            plugin
                .last_create_records_input()
                .expect("the ordinary record reached the persist SPI")
                .len(),
            1,
        );
    }

    /// The target cache is keyed by `(target, permit scope)`, not by target
    /// alone: two withdrawals of one target in different PDP groups each read
    /// it under their own group's scope. Two *faithful* withdrawals cannot
    /// land in different groups, since a faithful copy repeats the group key,
    /// so the second withdrawal departs in `resource_ref` alone — its own
    /// lookup succeeds and it is then refused as an unfaithful copy, a verdict
    /// only a row read for it could produce.
    #[tokio::test]
    async fn withdrawals_of_one_target_under_two_permits_each_cost_their_own_lookup() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5C2);
        let target = target_id(tenant_id, "idem-two-permits");
        plugin.set_get_record(target_row(tenant_id, "idem-two-permits"));
        let faithful = withdrawal_of(tenant_id, "idem-two-permits");
        // `resource_ref` is no identity input, so the second withdrawal
        // resolves the same target while landing in another PDP group.
        let other_resource = CreateUsageRecord {
            resource_ref: ResourceRef::new("rsc-other", "compute.vm").expect("valid resource ref"),
            ..withdrawal_of(tenant_id, "idem-two-permits")
        };
        let stored = projected(&faithful);
        plugin.set_create_records(vec![Ok(stored.clone())]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.target.two_permits.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![faithful, other_resource])
            .await
            .expect("batch dispatch succeeded");

        assert_eq!(
            plugin.get_usage_record_calls(),
            2,
            "one target under two PDP groups is read once per group",
        );
        assert_eq!(plugin.get_usage_record_inputs(), vec![target, target]);
        assert_eq!(
            results[0]
                .as_ref()
                .expect("the faithful withdrawal is accepted"),
            &stored,
        );
        match results[1].as_ref() {
            Err(UsageCollectorError::InvalidArgument {
                reason: ValidationReason::InvalidationFieldMismatch,
                field,
                ..
            }) => assert_eq!(field, "resource_ref"),
            other => unreachable!(
                "the withdrawal in the other group resolves against its own read, got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn identical_entries_in_one_batch_dispatch_once_and_share_the_stored_row() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5B1);
        let entry = ordinary_record(tenant_id, "idem-twice");
        let stored = UsageRecord {
            origin: usage_collector_sdk::v2::RecordOrigin::Backfill,
            ..projected(&entry)
        };
        plugin.set_create_records(vec![Ok(stored.clone())]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.identity.identical_pair.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![entry.clone(), entry])
            .await
            .expect("batch dispatch succeeded");

        assert_eq!(
            plugin
                .last_create_records_input()
                .expect("dispatched")
                .len(),
            1,
            "one dispatch per dedup identity"
        );
        for (position, result) in results.iter().enumerate() {
            assert_eq!(
                result.as_ref().expect("accepted"),
                &stored,
                "entry {position} reports the stored row"
            );
        }
    }

    #[tokio::test]
    async fn a_divergent_later_entry_conflicts_with_the_earlier_one() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5B2);
        let first = ordinary_record(tenant_id, "idem-split");
        let mut second = first.clone();
        second.quantity = crate::v2::domain::test_support::qty("11");
        let stored = projected(&first);
        plugin.set_create_records(vec![Ok(stored.clone())]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.identity.divergent_pair.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![first, second])
            .await
            .expect("batch dispatch succeeded");

        assert_eq!(results[0].as_ref().expect("the first is accepted"), &stored);
        match results[1].as_ref() {
            Err(UsageCollectorError::Conflict {
                outcome: ConflictOutcome::IdempotencyConflict,
                name,
                ..
            }) => assert_eq!(name, &stored.id.to_string()),
            other => unreachable!("the later divergent entry conflicts, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn two_withdrawals_of_one_target_in_one_batch_report_already_invalidated() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5B3);
        let target = target_id(tenant_id, "idem-w");
        plugin.set_get_record_for(target, target_row(tenant_id, "idem-w"));
        let first = withdrawal_of(tenant_id, "idem-w");
        let mut second = withdrawal_of(tenant_id, "idem-w");
        second.invalidation = Some(ReasonCode::new("late_correction").expect("valid reason code"));
        let stored = projected(&first);
        plugin.set_create_records(vec![Ok(stored.clone())]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.identity.withdrawal_pair.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![first, second])
            .await
            .expect("batch dispatch succeeded");

        assert_eq!(
            plugin
                .last_create_records_input()
                .expect("dispatched")
                .len(),
            1
        );
        assert!(
            results[0].is_ok(),
            "the first withdrawal is accepted: {:?}",
            results[0]
        );
        match results[1].as_ref() {
            Err(UsageCollectorError::Conflict {
                outcome:
                    ConflictOutcome::AlreadyInvalidated {
                        invalidated_by,
                        reason_code,
                    },
                name,
                ..
            }) => {
                assert_eq!(name, &target.to_string());
                assert_eq!(*invalidated_by, stored.id);
                assert_eq!(reason_code.as_str(), "emitter_defect");
            }
            other => unreachable!("the later withdrawal is AlreadyInvalidated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_later_entry_equal_to_what_the_store_holds_is_accepted_with_it() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5B4);
        let held = ordinary_record(tenant_id, "idem-held");
        let mut first = held.clone();
        first.quantity = crate::v2::domain::test_support::qty("11");
        let stored = projected(&held);
        plugin.set_create_records(vec![Err(UsageCollectorPluginError::idempotency_conflict(
            usage_collector_sdk::v2::IdempotencyKey::new("idem-held")
                .expect("valid idempotency key"),
            crate::v2::domain::test_support::as_stored(stored.clone()),
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.identity.against_stored.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![first, held])
            .await
            .expect("batch dispatch succeeded");

        assert!(
            matches!(
                results[0].as_ref(),
                Err(UsageCollectorError::Conflict {
                    outcome: ConflictOutcome::IdempotencyConflict,
                    ..
                })
            ),
            "the first conflicts with the stored row: {:?}",
            results[0]
        );
        assert_eq!(
            results[1]
                .as_ref()
                .expect("the later entry matches what the store holds"),
            &stored
        );
    }

    #[tokio::test]
    async fn a_later_entry_shares_the_earlier_entrys_backend_failure() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5B5);
        let entry = ordinary_record(tenant_id, "idem-blip");
        plugin.set_create_records(vec![Err(UsageCollectorPluginError::transient(
            "backend blip",
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.identity.shared_failure.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![entry.clone(), entry])
            .await
            .expect("batch dispatch succeeded");

        for (position, result) in results.iter().enumerate() {
            assert!(
                matches!(result, Err(UsageCollectorError::ServiceUnavailable { .. })),
                "entry {position} carries the backend failure: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_withdrawal_of_an_entry_in_the_same_batch_is_not_converged() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5B6);
        let record = ordinary_record(tenant_id, "idem-same-batch");
        let record_id = projected(&record).id;
        plugin.set_get_usage_record_not_found(record_id);
        plugin.set_create_records(vec![Ok(projected(&record))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.identity.same_batch_target.records.v1",
        );

        let results = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![record, withdrawal_of(tenant_id, "idem-same-batch")],
            )
            .await
            .expect("batch dispatch succeeded");

        assert!(
            results[0].is_ok(),
            "the record is accepted: {:?}",
            results[0]
        );
        match results[1].as_ref() {
            Err(UsageCollectorError::Conflict {
                outcome: ConflictOutcome::TargetNotConverged { .. },
                name,
                ..
            }) => assert_eq!(name, &record_id.to_string()),
            other => {
                unreachable!("a target submitted in the same batch is not converged, got {other:?}")
            }
        }
    }
}

// This module drives `create_usage_records` with a single submission, where
// the PDP / declaration / target / metadata / SPI sequencing is observable one
// stage at a time rather than through the deduped fan-out that the
// `pdp_dedup`, `gts_id_dedup` and `invalidation_target_batch` modules cover.
// One outcome per stage: PDP deny, plugin-reported transient on the persist
// SPI, a target that resolves to nothing, a target that is itself a
// withdrawal, an unfaithful copy, the copy rule outranking the metadata rule,
// the dedup conflict of an invalidation, a backend fault on the target read,
// and the happy path.

mod create_usage_record_path_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use usage_collector_sdk::v2::{
        ConflictOutcome, CreateUsageRecord, EntryType, IdempotencyKey, Invalidation, MetadataKey,
        MeterTypeId, ReasonCode, RecordOrigin, ResourceRef, USAGE_RECORD_RESOURCE,
        UsageCollectorError, UsageCollectorPluginError, UsageCollectorPluginV2, UsageRecord,
        ValidationReason,
    };
    use uuid::Uuid;

    use super::assert_translatable_scope;
    use crate::v2::domain::Service;
    use crate::v2::domain::test_support::{
        DenyAllResolver, HappyPathPlugin, MeterNarrowingPermitResolver, ServiceFixture,
        UncompilableSiblingPermitResolver, authenticated_ctx, enforcer_for,
        fake_declaration_source_with_fold, fake_declaration_source_with_metadata, hub_with_plugin,
        projected, recent_window_end, recent_window_start,
    };

    const COUNTER_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");

    /// The base ordinary measurement (no `invalidates`) every test here
    /// shapes; call sites mutate `value` / `gts_type_id` / `invalidation` to
    /// drive the per-stage outcome. `value` drives no accept/reject decision.
    fn counter_record(tenant_id: Uuid, value: i64, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(COUNTER_GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-singular", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::v2::domain::test_support::qty(&value.to_string()),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    /// The withdrawal of `counter_record(tenant_id, 10, idem)`: a faithful
    /// copy, departing only in `entry_type` and the reason. The quantity is
    /// echoed, never negated — an invalidation removes a measurement rather
    /// than offsetting it. It names no target; the gateway derives one from
    /// the identity inputs this copies, so `idem` chooses which entry is
    /// withdrawn and [`target_id`] says which.
    fn counter_withdrawal(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        let mut r = counter_record(tenant_id, 10, idem);
        r.entry_type = EntryType::Invalidation;
        r.invalidation = Some(ReasonCode::new("emitter_defect").expect("valid reason code"));
        r
    }

    /// The persisted entry `counter_withdrawal(tenant_id, idem)` withdraws:
    /// the projection of the very submission it copies, so the two carry the
    /// same identity inputs and the derivation lands on this row.
    fn target_row(tenant_id: Uuid, idem: &str) -> UsageRecord {
        UsageRecord {
            // Deliberately not the origin the live submission carries: a
            // mismatch here MUST NOT make the copy unfaithful
            // (`faithful_copy_mismatch` ignores `origin`).
            origin: RecordOrigin::Backfill,
            ..projected(&counter_record(tenant_id, 10, idem))
        }
    }

    /// The identifier `counter_withdrawal(tenant_id, idem)` resolves.
    fn target_id(tenant_id: Uuid, idem: &str) -> Uuid {
        target_row(tenant_id, idem).id
    }

    /// A `Service` over a permit-by-default PDP, a Type Resolver declaring an
    /// arbitrary fold (these tests exercise ingestion, not aggregation), and
    /// the supplied plugin stub.
    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV2>, suffix: &str) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(plugin, suffix)
    }

    /// A `Service` over a deny-all PDP. The plugin stub is left unprogrammed,
    /// so a leaked SPI call surfaces as a `not_programmed` `Internal` rather
    /// than the expected authorization envelope.
    fn service_with_deny(plugin: Arc<dyn UsageCollectorPluginV2>, suffix: &str) -> Arc<Service> {
        let hub = hub_with_plugin(plugin, suffix, "constructorfabric");
        let enforcer = enforcer_for(Arc::new(DenyAllResolver) as _);
        Arc::new(Service::new(hub, "constructorfabric".to_owned(), enforcer))
    }

    /// PDP deny ⇒ `PermissionDenied`, with **no** catalog / semantics / SPI
    /// dispatch.
    #[tokio::test]
    async fn create_usage_record_pdp_deny_returns_authorization_before_any_spi_dispatch() {
        let plugin = HappyPathPlugin::new();
        let service = service_with_deny(
            Arc::clone(&plugin) as _,
            "test.singular.pdp_deny.records.v1",
        );

        let record = counter_record(Uuid::from_u128(0x701), 1, "idem-pdp-deny");

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![record])
            .await
            .expect(
                "a PDP deny is a per-record rejection, not a request-wide refusal, so the \
                 batch call itself still succeeds",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("PDP deny MUST surface as Err");

        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "PDP deny MUST surface as `PermissionDenied`, got {err:?}",
        );
        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "PDP deny MUST short-circuit before any target lookup",
        );
        assert!(
            plugin.last_create_records_input().is_none(),
            "PDP deny MUST short-circuit before any persist SPI dispatch",
        );
    }

    /// Plugin-reported `Transient` from the persist SPI ⇒
    /// `ServiceUnavailable` at the SDK boundary, with the
    /// `retry_after_seconds` hint forwarded verbatim.
    #[tokio::test]
    async fn create_usage_record_plugin_transient_lifts_to_service_unavailable() {
        let plugin = HappyPathPlugin::new();
        plugin.set_create_records(vec![Err(UsageCollectorPluginError::transient_with_retry(
            "downstream backend timed out",
            Some(13),
        ))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.plugin_transient.records.v1",
        );

        let record = counter_record(Uuid::from_u128(0x702), 1, "idem-plugin-transient");

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![record])
            .await
            .expect(
                "the persist SPI answered this one record with Transient inside its Vec \
                 of per-record outcomes, which `settle_dispatched` always routes to the \
                 slot rather than the outer Err - the outer dispatch call itself \
                 (`plugin.create_usage_records`) succeeded",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("plugin transient MUST surface as Err");

        match err {
            UsageCollectorError::ServiceUnavailable {
                detail,
                retry_after_seconds,
            } => {
                assert_eq!(detail, "downstream backend timed out");
                assert_eq!(retry_after_seconds, Some(13));
            }
            other => unreachable!(
                "plugin Transient MUST lift to `ServiceUnavailable` with the \
                 retry hint forwarded; got {other:?}",
            ),
        }
    }

    /// A faithful withdrawal of a resolvable target is accepted, and costs
    /// exactly one target read.
    #[tokio::test]
    async fn create_usage_record_accepts_a_faithful_withdrawal_after_one_target_lookup() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x703);
        plugin.set_get_record(target_row(tenant_id, "idem-faithful"));

        let submission = counter_withdrawal(tenant_id, "idem-faithful");
        plugin.set_create_records(vec![Ok(projected(&submission))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.faithful.records.v1",
        );

        service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect("the measurement is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("a faithful withdrawal of a resolvable target MUST be accepted");

        assert_eq!(
            plugin.get_usage_record_calls(),
            1,
            "an invalidation MUST resolve its target exactly once",
        );

        assert_translatable_scope(&plugin, "resolve_invalidation_targets");
    }

    /// The dispatched withdrawal keeps the caller's key, which is its
    /// target's key: no prefix is reserved and nothing is derived in its
    /// place. Keeping it is what lets the gateway resolve the target from the
    /// submission alone, and what makes two withdrawals of one entry collide.
    #[tokio::test]
    async fn create_usage_record_dispatches_an_invalidation_under_its_targets_key() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x704);
        plugin.set_get_record(target_row(tenant_id, "idem-withdrawn"));

        let submission = counter_withdrawal(tenant_id, "idem-withdrawn");
        plugin.set_create_records(vec![Ok(projected(&submission))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.derived_key.records.v1",
        );

        service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect("the withdrawal is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("a faithful withdrawal of a resolvable target MUST be accepted");

        let dispatched = plugin
            .last_create_records_input()
            .expect("the withdrawal reached the persist SPI")
            .into_iter()
            .next()
            .expect("one entry in, one dispatched record out");
        assert_eq!(
            dispatched.idempotency_key.as_str(),
            "idem-withdrawn",
            "a withdrawal is dispatched under its target's own key",
        );
        assert_eq!(
            dispatched
                .invalidation
                .as_ref()
                .expect("the dispatched entry is a withdrawal")
                .target,
            target_id(tenant_id, "idem-withdrawn"),
            "the gateway stamps the target it derived from the submission",
        );
    }

    /// A derived target the plugin does not hold ⇒ `NotFound` naming it. The
    /// batch path's cover is in `invalidation_target_batch_tests`.
    #[tokio::test]
    async fn create_usage_record_unresolvable_target_returns_not_found() {
        let plugin = HappyPathPlugin::new();

        let tenant_id = Uuid::from_u128(0x705);
        let missing = target_id(tenant_id, "idem-missing-target");
        plugin.set_get_usage_record_not_found(missing);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.target_not_found.records.v1",
        );

        let record = counter_withdrawal(tenant_id, "idem-missing-target");

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![record])
            .await
            .expect(
                "an unresolvable target is a per-record rejection: resolve_invalidation_targets \
                 always routes it to the slot, never to the outer Err",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("an unresolvable target MUST surface as Err");

        assert!(
            matches!(
                err,
                UsageCollectorError::NotFound { resource_type, ref name, ref detail, .. }
                    if resource_type == USAGE_RECORD_RESOURCE
                        && name == &missing.to_string()
                        && detail.contains(&missing.to_string())
            ),
            "a target that resolves to nothing MUST surface as NotFound naming \
             the identifier the submission resolved; got {err:?}",
        );
        assert!(
            plugin.last_create_records_input().is_none(),
            "an unresolvable target MUST short-circuit before the persist SPI",
        );
    }

    /// Withdrawing a withdrawal resolves the measurement they both copy, not
    /// the withdrawal, so "no invalidation of an invalidation" needs no check:
    /// the submission built to withdraw a withdrawal is, input for input, the
    /// submission that withdraws the measurement.
    #[tokio::test]
    async fn create_usage_record_cannot_express_a_withdrawal_of_a_withdrawal() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x707);
        let measurement = target_row(tenant_id, "idem-withdraw-a-withdrawal");
        let withdrawal = projected(&counter_withdrawal(tenant_id, "idem-withdraw-a-withdrawal"));
        assert_ne!(measurement.id, withdrawal.id, "test premise");
        plugin.set_get_record(measurement.clone());
        plugin.set_create_records(vec![Ok(withdrawal.clone())]);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.withdrawal_of_a_withdrawal.records.v1",
        );

        service
            .create_usage_records(
                &authenticated_ctx(),
                vec![counter_withdrawal(tenant_id, "idem-withdraw-a-withdrawal")],
            )
            .await
            .expect("the submission is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("the submission withdraws the measurement, and is accepted");

        assert_eq!(
            plugin.get_usage_record_inputs(),
            vec![measurement.id],
            "the derivation asks for the measurement, never for the withdrawal",
        );
    }

    /// The singular path's mirror of the batch row-contradiction guard: a
    /// plugin answering the derived identifier with a row that is itself an
    /// invalidation is refused as a host-invariant breach. The submission is a
    /// faithful copy of what came back, so without the guard it is accepted.
    #[tokio::test]
    async fn create_usage_record_refuses_a_plugin_that_answers_with_an_invalidation_row() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x70C);
        let target = target_id(tenant_id, "idem-corrupt-row");
        let mut row = target_row(tenant_id, "idem-corrupt-row");
        row.invalidation = Some(Invalidation {
            target: Uuid::from_u128(0x81F),
            reason: ReasonCode::new("emitter_defect").expect("valid reason code"),
        });
        assert_eq!(
            row.id, target,
            "test premise: the row answers the derived id"
        );
        plugin.set_get_record(row);

        let submission = counter_withdrawal(tenant_id, "idem-corrupt-row");
        plugin.set_create_records(vec![Ok(projected(&submission))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.invalidation_row.records.v1",
        );

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect(
                "a row contradicting its own identifier is a per-record breach, caught inside \
                 `resolve_invalidation_targets`, never a request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a row contradicting its own identifier MUST surface as Err");

        match err {
            UsageCollectorError::Internal { detail } => assert!(
                detail.contains(&target.to_string()),
                "the breach MUST name the id the gateway asked for: {detail}"
            ),
            other => unreachable!("expected a host-invariant breach, got {other:?}"),
        }
        assert!(
            plugin.last_create_records_input().is_none(),
            "the breach MUST short-circuit before the persist SPI",
        );
    }

    /// A withdrawal departing from its target in a copied field ⇒
    /// `InvalidationFieldMismatch` **naming the field that differs**. The
    /// message names what differs and never what it differs from: the
    /// target was read unscoped, so echoing its value would make the
    /// rejection an oracle for a row this caller has no grant for.
    #[tokio::test]
    async fn create_usage_record_unfaithful_copy_names_the_field_that_differs() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x708);

        let mut row = target_row(tenant_id, "idem-unfaithful");
        // `99.9`, not `999`: the detail asserted below carries the derived
        // target UUID, so a hex-digits-only sentinel would also match a
        // three-character window of that UUID and flake. The `.` is not a hex
        // digit, which makes the assertion structurally immune.
        row.quantity = crate::v2::domain::test_support::qty("99.9");
        plugin.set_get_record(row);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.field_mismatch.records.v1",
        );

        let result = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![counter_withdrawal(tenant_id, "idem-unfaithful")],
            )
            .await
            .expect(
                "an unfaithful copy is a per-record validation rejection, caught inside \
                 `resolve_invalidation_targets`, never a request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("an unfaithful copy MUST surface as Err");

        match err {
            UsageCollectorError::InvalidArgument {
                reason: ValidationReason::InvalidationFieldMismatch,
                field,
                detail,
                ..
            } => {
                assert_eq!(
                    field, "quantity",
                    "the rejection MUST name the field that differs"
                );
                assert!(
                    !detail.contains("99.9"),
                    "the rejection MUST NOT echo the target's value: {detail}",
                );
            }
            other => unreachable!(
                "a departure in a copied field MUST surface as \
                 InvalidationFieldMismatch; got {other:?}",
            ),
        }
    }

    /// A submission breaking the copy rule **and** the metadata rule is
    /// told about the copy: the metadata it would be sent to fix is
    /// metadata it has to copy from the target regardless.
    #[tokio::test]
    async fn create_usage_record_copy_mismatch_outranks_a_metadata_rejection() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x709);

        let mut row = target_row(tenant_id, "idem-both-broken");
        row.quantity = crate::v2::domain::test_support::qty("999");
        plugin.set_get_record(row);

        let mut submission = counter_withdrawal(tenant_id, "idem-both-broken");
        submission.metadata.insert(
            MetadataKey::new("zone").expect("valid metadata key"),
            "eu-1".to_owned(),
        );

        // The declaration declares `region` alone, so `zone` fails the
        // closed-shape check.
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(&["region"]))
            .build(
                Arc::clone(&plugin) as _,
                "test.singular.priority.records.v1",
            );

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect(
                "both rejections are per-record validation outcomes, never a request-wide \
                 refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a submission breaking both rules MUST surface as Err");

        match err {
            UsageCollectorError::InvalidArgument { reason, .. } => assert_eq!(
                reason,
                ValidationReason::InvalidationFieldMismatch,
                "the copy rejection MUST outrank the metadata one",
            ),
            other => unreachable!("expected a validation rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_usage_record_store_conflict_on_a_withdrawal_is_already_invalidated() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x70A);
        let target = target_id(tenant_id, "idem-withdrawn");
        plugin.set_get_record(target_row(tenant_id, "idem-withdrawn"));
        let stored = projected(&counter_withdrawal(tenant_id, "idem-withdrawn"));
        let stored_id = stored.id;
        plugin.set_create_records(vec![Err(UsageCollectorPluginError::idempotency_conflict(
            usage_collector_sdk::v2::IdempotencyKey::new("idem-withdrawn")
                .expect("valid idempotency key"),
            crate::v2::domain::test_support::as_stored(stored),
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.already_invalidated.records.v1",
        );

        let mut second = counter_withdrawal(tenant_id, "idem-withdrawn");
        second.invalidation = Some(ReasonCode::new("late_correction").expect("valid reason code"));

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![second])
            .await
            .expect(
                "the idempotency conflict is lifted by `settle_dispatched` into the slot, not \
                 the outer Err - the dispatch call itself succeeded",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a second withdrawal MUST surface as Err");

        match err {
            UsageCollectorError::Conflict {
                outcome:
                    ConflictOutcome::AlreadyInvalidated {
                        invalidated_by,
                        reason_code,
                    },
                name,
                ..
            } => {
                assert_eq!(name, target.to_string());
                assert_eq!(invalidated_by, stored_id);
                assert_eq!(reason_code.as_str(), "emitter_defect");
            }
            other => unreachable!("expected AlreadyInvalidated; got {other:?}"),
        }
    }

    /// A backend fault on the target read MUST fail the submission rather
    /// than reject it: an unreadable target is not an absent one.
    #[tokio::test]
    async fn create_usage_record_transient_target_lookup_lifts_to_service_unavailable() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_record_transient("target store timed out", Some(7));

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.target_transient.records.v1",
        );

        let result = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![counter_withdrawal(Uuid::from_u128(0x70B), "idem-t")],
            )
            .await
            .expect(
                "`resolve_invalidation_targets` has no outer-Err path at all: every \
                 `get_usage_record` outcome, transient included, is written to the slot",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a transient target read MUST surface as Err");

        match err {
            UsageCollectorError::ServiceUnavailable {
                detail,
                retry_after_seconds,
            } => {
                assert_eq!(detail, "target store timed out");
                assert_eq!(retry_after_seconds, Some(7));
            }
            other => {
                unreachable!("a transient target read MUST NOT become a rejection; got {other:?}")
            }
        }
    }

    /// A plugin answering `get_usage_record(a)` with a different row MUST be
    /// refused here too. The submission is again a faithful copy of the
    /// returned row, so without the id check it is accepted.
    #[tokio::test]
    async fn create_usage_record_refuses_a_plugin_that_answers_with_the_wrong_row() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x70C);
        let requested = target_id(tenant_id, "idem-wrong-row");
        let answered = target_id(tenant_id, "idem-other-row");
        plugin.set_get_record(target_row(tenant_id, "idem-other-row"));

        // Programmed so a gateway that believed the row fails visibly as
        // `Ok(..)`. The two rows differ in their idempotency key alone, which
        // the copy comparator does not read, so only the id check catches it.
        let submission = counter_withdrawal(tenant_id, "idem-wrong-row");
        plugin.set_create_records(vec![Ok(projected(&submission))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.wrong_row.records.v1",
        );

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect(
                "a mis-answering plugin is a per-record breach caught inside \
                 `resolve_invalidation_targets`, never a request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a mis-answering plugin MUST surface as Err");

        match err {
            UsageCollectorError::Internal { detail } => {
                assert!(detail.contains(&requested.to_string()), "{detail}");
                assert!(!detail.contains(&answered.to_string()), "{detail}");
            }
            other => unreachable!(
                "a row that is not the row asked for MUST be a host-invariant \
                 breach, not an acceptance; got {other:?}",
            ),
        }
        assert!(
            plugin.last_create_records_input().is_none(),
            "the breach MUST short-circuit before the persist SPI",
        );
    }

    /// Happy path: PDP permit + catalog hit + metadata validation pass +
    /// persist SPI ⇒ `Ok(persisted_record)`. The persisted echo's `id` differs
    /// from the input so the two can be told apart.
    #[tokio::test]
    async fn create_usage_record_happy_path_returns_persisted_echo() {
        let plugin = HappyPathPlugin::new();

        let mut persisted = projected(&counter_record(
            Uuid::from_u128(0xCAFE),
            1,
            "idem-happy-persist",
        ));
        persisted.id = Uuid::from_u128(0xDEAD_C0DE);
        plugin.set_create_records(vec![Ok(persisted.clone())]);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.happy_path.records.v1",
        );

        let input = counter_record(Uuid::from_u128(0x706), 1, "idem-happy-input");

        let returned = service
            .create_usage_records(&authenticated_ctx(), vec![input])
            .await
            .expect("the measurement is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("happy path MUST return the persisted record");

        assert_eq!(
            returned.id, persisted.id,
            "the returned record MUST be the plugin's persisted echo \
             (not the caller's input)",
        );
        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "an entry carrying no `invalidates` MUST NOT trigger a target lookup",
        );
    }

    #[tokio::test]
    async fn create_usage_record_not_converged_target_is_a_retryable_conflict() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x70C);
        let target = target_id(tenant_id, "idem-nc");
        plugin.set_get_usage_record_not_converged(target);
        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.not_converged.records.v1",
        );

        let result = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![counter_withdrawal(tenant_id, "idem-nc")],
            )
            .await
            .expect(
                "a not-converged target is a per-record conflict caught inside \
                 `resolve_invalidation_targets`, never a request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a not-converged target is refused");

        match err {
            UsageCollectorError::Conflict {
                outcome: ConflictOutcome::TargetNotConverged { .. },
                name,
                ..
            } => assert_eq!(name, target.to_string()),
            other => unreachable!("expected TargetNotConverged, got {other:?}"),
        }
        assert_eq!(plugin.get_usage_record_converged_only_flags(), vec![true]);
        assert!(
            plugin.last_create_records_input().is_none(),
            "nothing is dispatched"
        );
    }

    #[tokio::test]
    async fn create_usage_record_naming_another_tenants_row_answers_as_absent() {
        let plugin = HappyPathPlugin::new();
        let caller_tenant = Uuid::from_u128(0x70D);
        let other_tenant = Uuid::from_u128(0x70E);
        let target = target_id(caller_tenant, "idem-cross");
        plugin.set_get_record(target_row(other_tenant, "idem-cross"));
        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.cross_tenant.records.v1",
        );

        let result = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![counter_withdrawal(caller_tenant, "idem-cross")],
            )
            .await
            .expect(
                "an out-of-scope target is a per-record rejection caught inside \
                 `resolve_invalidation_targets`, never a request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("an out-of-scope target is refused");

        match err {
            UsageCollectorError::NotFound {
                reason: usage_collector_sdk::v2::NotFoundReason::InvalidationTargetNotFound,
                name,
                ..
            } => assert_eq!(name, target.to_string()),
            other => unreachable!("an out-of-scope target must read as absent, got {other:?}"),
        }
        assert!(
            plugin.last_create_records_input().is_none(),
            "nothing is dispatched"
        );
        assert_eq!(plugin.get_usage_record_calls(), 1);
        assert_translatable_scope(&plugin, "resolve_invalidation_targets");
        let scope = plugin.last_get_scope().expect("the lookup dispatched");
        assert!(
            scope.contains(&format!("{caller_tenant:?}"))
                && !scope.contains(&format!("{other_tenant:?}"))
                && !scope.contains(&format!("{target:?}")),
            "the lookup reads under the caller's compiled permit, not `id eq <target>` or the \
             target's tenant: {scope}"
        );
    }

    /// A permit the per-entry gate admits but whose scope does not compile
    /// fails a withdrawal closed as `PermissionDenied`, before any target
    /// read. The ordinary record submitted first under the same permit is
    /// accepted, which is what proves the gate admitted it.
    #[tokio::test]
    async fn create_usage_record_withdrawal_under_a_permit_scope_that_does_not_compile_is_permission_denied()
     {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x70F);
        plugin.set_get_record(target_row(tenant_id, "idem-uncompilable-withdrawal"));
        let ordinary = counter_record(tenant_id, 10, "idem-uncompilable-plain");
        plugin.set_create_records(vec![Ok(projected(&ordinary))]);
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .with_resolver(Arc::new(UncompilableSiblingPermitResolver))
            .build(
                Arc::clone(&plugin) as _,
                "test.singular.uncompilable_scope.records.v1",
            );

        service
            .create_usage_records(&authenticated_ctx(), vec![ordinary])
            .await
            .expect("the measurement is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("the gate admits the tuple through the tenant constraint");

        let result = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![counter_withdrawal(
                    tenant_id,
                    "idem-uncompilable-withdrawal",
                )],
            )
            .await
            .expect(
                "a scope-compile failure is projected onto the slot inside \
                 `create_usage_records_inner`'s per-entry loop, never the outer Err",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a permit scope that does not compile is refused");

        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "a permit scope that does not compile fails closed, got {err:?}",
        );
        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "no target is read under a scope that does not compile",
        );
    }

    /// A grant narrowed by meter admits a measurement and **denies a
    /// withdrawal**.
    ///
    /// `gts_type_id` is scopable on the attribution tuple, so
    /// `authz::scope_admits_attribution_tuple` admits the measurement. But a
    /// withdrawal reads its target under the `create` permit's own compiled
    /// scope (`authz::scope_to_odata_filter`), whose projection refuses the
    /// identifier because the storage filter vocabulary excludes it — so the
    /// reservation fires on part of the ingestion path, not only on reads.
    /// The asserted wording is what tells an operator which half it lost.
    #[tokio::test]
    async fn create_usage_record_under_a_meter_narrowed_grant_measures_but_cannot_withdraw() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x71A);
        plugin.set_get_record(target_row(tenant_id, "idem-meter-narrowed-withdrawal"));
        let ordinary = counter_record(tenant_id, 10, "idem-meter-narrowed-plain");
        assert_eq!(
            ordinary.gts_type_id.as_ref(),
            COUNTER_GTS_ID,
            "test premise: the fixture is attributed to the meter the grant names, \
             so the gate's verdict turns on nothing else"
        );
        plugin.set_create_records(vec![Ok(projected(&ordinary))]);
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .with_resolver(MeterNarrowingPermitResolver::sole(COUNTER_GTS_ID))
            .build(
                Arc::clone(&plugin) as _,
                "test.singular.meter_narrowed_scope.records.v1",
            );

        service
            .create_usage_records(&authenticated_ctx(), vec![ordinary])
            .await
            .expect("the measurement is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("the gate admits a measurement attributed to the granted meter");

        let result = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![counter_withdrawal(
                    tenant_id,
                    "idem-meter-narrowed-withdrawal",
                )],
            )
            .await
            .expect(
                "a scope-compile failure is projected onto the slot inside \
                 `create_usage_records_inner`'s per-entry loop, never the outer Err",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err(
            "the withdrawal's target lookup projects the same scope, and the meter \
             cannot be projected",
        );

        let UsageCollectorError::PermissionDenied { detail } = &err else {
            unreachable!("a scope the target lookup cannot project fails closed, got {err:?}");
        };
        assert!(
            detail.contains("reserved") && detail.contains("gts_type_id"),
            "the refusal an operator reads must name the reserved property; got {detail:?}"
        );
        assert!(
            detail.contains("withdraw"),
            "and it must say which half of the feature it took away, because \
             'denied' alone does not distinguish a meter-narrowed grant that \
             measures from one that cannot; got {detail:?}"
        );
        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "no target is read under a scope the projection refuses",
        );
    }
}

// ── Batch size-cap guard in `create_usage_records` ─────────────────────────
//
// Both out-of-bounds arms of the `actual == 0 || actual > max_batch_records`
// entry gate MUST reject with `invalid_batch_size` *before* any plugin
// dispatch. The fixture configures a small cap so the over-cap case does not
// need a 100+-record batch.
#[cfg(test)]
mod batch_size_cap_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use usage_collector_sdk::v2::{
        CreateUsageRecord, EntryType, IdempotencyKey, MeterTypeId, ResourceRef,
        UsageCollectorError, UsageCollectorPluginV2, ValidationReason,
    };
    use uuid::Uuid;

    use crate::v2::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, recent_window_end, recent_window_start,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");

    const CAP: usize = 3;

    fn input_record(idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id: Uuid::from_u128(1),
            resource_ref: ResourceRef::new("rsc-batch-cap", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::v2::domain::test_support::qty("1"),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    fn assert_invalid_batch_size(err: &UsageCollectorError) {
        assert!(
            matches!(
                err,
                UsageCollectorError::InvalidArgument {
                    reason: ValidationReason::Validation,
                    field,
                    ..
                } if field == "records"
            ),
            "expected invalid_batch_size InvalidArgument on `records`, got {err:?}",
        );
    }

    /// Empty batch (`actual == 0`) → `invalid_batch_size`, plugin untouched.
    #[tokio::test]
    async fn create_usage_records_rejects_empty_batch_without_dispatch() {
        let plugin = HappyPathPlugin::new();

        let service = ServiceFixture::default().with_max_batch_records(CAP).build(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.batch_cap.empty.records.v1",
        );

        let err = service
            .create_usage_records(&authenticated_ctx(), Vec::new())
            .await
            .expect_err("empty batch MUST reject");
        assert_invalid_batch_size(&err);

        assert!(
            plugin.last_create_records_input().is_none(),
            "empty batch MUST short-circuit before any plugin dispatch",
        );
    }

    /// Over-cap batch (`CAP + 1`) → `invalid_batch_size`, plugin untouched.
    #[tokio::test]
    async fn create_usage_records_rejects_over_cap_batch_without_dispatch() {
        let plugin = HappyPathPlugin::new();

        let service = ServiceFixture::default().with_max_batch_records(CAP).build(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.batch_cap.over.records.v1",
        );

        let input: Vec<CreateUsageRecord> = (0..=CAP)
            .map(|i| input_record(&format!("idem-cap-{i}")))
            .collect();
        assert_eq!(input.len(), CAP + 1);

        let err = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect_err("over-cap batch MUST reject");
        assert_invalid_batch_size(&err);

        assert!(
            plugin.last_create_records_input().is_none(),
            "over-cap batch MUST short-circuit before any plugin dispatch",
        );
    }

    /// The service enforces the configured cap it was built with, not a
    /// hard-coded constant.
    #[tokio::test]
    async fn the_configured_cap_is_the_one_enforced() {
        let plugin = HappyPathPlugin::new();
        let service = ServiceFixture::default()
            .with_max_batch_records(CAP)
            .build(plugin, "test.batch_cap.configured.records.v1");
        assert_eq!(service.max_batch_records(), CAP);
        let over: Vec<_> = (0..=CAP)
            .map(|i| input_record(&format!("cap-{i}")))
            .collect();
        let err = service
            .create_usage_records(&authenticated_ctx(), over)
            .await
            .expect_err("CAP + 1 must be refused");
        assert_invalid_batch_size(&err);
    }
}

// ── Service-level server-assigned stamps (in-process / SDK callers) ────────
//
// The create surface is identity-free: callers never supply an `id`. The
// domain `Service` is the single point where a submission acquires one, via
// the SDK projection its `entry_type` selects — see
// [`usage_collector_sdk::v2::derive_usage_record_id`]. These tests drive the
// `Service` create methods directly (NOT through the REST handler) and assert
// the record the plugin RECEIVED carries the derivation. `origin` is stamped
// from the same call, so it is pinned here too.
#[cfg(test)]
mod server_assigned_stamp_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use usage_collector_sdk::v2::{
        CreateUsageRecord, EntryType, IdempotencyKey, MeterTypeId, RecordOrigin, ResourceRef,
        UsageCollectorPluginV2, derive_usage_record_id,
    };
    use uuid::Uuid;

    use crate::v2::domain::Service;
    use crate::v2::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, fake_declaration_source_with_fold,
        projected, recent_window_end, recent_window_start,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");

    /// A [`ServiceFixture`] with an arbitrary working declaration — these
    /// tests exercise id derivation, not the declaration.
    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV2>, suffix: &str) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(plugin, suffix)
    }

    /// A create submission with a known dedup identity, so a passing assertion
    /// can only mean the service derived the dispatched record's id from it.
    fn input_record(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-derive", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::v2::domain::test_support::qty("1"),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    /// One-entry `create_usage_records`: the dispatched record's `id` MUST be
    /// the service-derived value.
    #[tokio::test]
    async fn create_usage_record_stamps_derived_id() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xD1);
        let idem = "idem-derive-singular";
        let input = input_record(tenant_id, idem);
        let expected = derive_usage_record_id(
            input.tenant_id,
            &input.gts_type_id,
            &input.idempotency_key,
            input.window_start,
            input.window_end,
            input.entry_type(),
        )
        .unwrap();

        // The plugin echoes the record back so the persist SPI succeeds; the
        // assertion reads the CAPTURED dispatched record.
        plugin.set_create_records(vec![Ok(projected(&input))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.derived_id.singular.records.v1",
        );

        service
            .create_usage_records(&authenticated_ctx(), vec![input])
            .await
            .expect("the measurement is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("happy path MUST accept the record");

        let dispatched = plugin
            .last_create_records_input()
            .expect("plugin received the dispatched batch")
            .into_iter()
            .next()
            .expect("one entry in, one dispatched record out");
        assert_eq!(
            dispatched.id, expected,
            "the SERVICE MUST stamp the dispatched record's id with \
             derive_usage_record_id(tenant_id, gts_type_id, idempotency_key, \
             window_start, window_end, entry_type).unwrap() - this guards the \
             in-process (non-REST) caller path independently of the handler",
        );
    }

    /// Batch `create_usage_records`: every dispatched record's `id` MUST be its
    /// own service-derived value.
    #[tokio::test]
    async fn create_usage_records_stamps_derived_id() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xD2);
        let idems = ["idem-derive-batch-0", "idem-derive-batch-1"];
        let input: Vec<CreateUsageRecord> = idems
            .iter()
            .map(|idem| input_record(tenant_id, idem))
            .collect();
        let expected: Vec<Uuid> = input
            .iter()
            .map(|r| {
                derive_usage_record_id(
                    r.tenant_id,
                    &r.gts_type_id,
                    &r.idempotency_key,
                    r.window_start,
                    r.window_end,
                    r.entry_type(),
                )
                .unwrap()
            })
            .collect();

        plugin.set_create_records(input.iter().map(|r| Ok(projected(r))).collect());

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.derived_id.batch.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert!(results.iter().all(Result::is_ok));

        let dispatched = plugin
            .last_create_records_input()
            .expect("plugin received the dispatched batch");
        let dispatched_ids: Vec<Uuid> = dispatched.iter().map(|r| r.id).collect();
        assert_eq!(
            dispatched_ids, expected,
            "the SERVICE MUST stamp each dispatched record's id with its own \
             derive_usage_record_id(tenant_id, gts_type_id, idempotency_key, \
             window_start, window_end).unwrap(), overwriting the caller-supplied ids - \
             this guards the in-process (non-REST) batch caller path \
             independently of the handler",
        );
    }

    /// Both live ingestion surfaces MUST stamp `origin = live` on what they
    /// dispatch. Read off the DISPATCHED record, not the returned one: the
    /// plugin echoes a fixture back, so a returned `Live` would only prove the
    /// fixture was built `Live`.
    #[tokio::test]
    async fn the_live_surfaces_stamp_origin_live() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xD3);
        let single = input_record(tenant_id, "idem-origin-singular");
        let batch = vec![input_record(tenant_id, "idem-origin-batch")];

        plugin.set_create_records(vec![Ok(projected(&single))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.origin_stamp.live.records.v1",
        );

        service
            .create_usage_records(&authenticated_ctx(), vec![single])
            .await
            .expect("the measurement is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("happy path MUST accept the record");
        assert_eq!(
            plugin
                .last_create_records_input()
                .expect("plugin received the dispatched batch")
                .into_iter()
                .next()
                .expect("one entry in, one dispatched record out")
                .origin,
            RecordOrigin::Live,
            "create_usage_records IS the live route, so the entry it \
             dispatches MUST carry origin = live",
        );

        plugin.set_create_records(batch.iter().map(|r| Ok(projected(r))).collect());

        let results = service
            .create_usage_records(&authenticated_ctx(), batch)
            .await
            .expect("batch dispatch succeeded");
        assert!(results.iter().all(Result::is_ok));
        let dispatched_origins: Vec<RecordOrigin> = plugin
            .last_create_records_input()
            .expect("plugin received the dispatched batch")
            .iter()
            .map(|r| r.origin)
            .collect();
        assert_eq!(
            dispatched_origins,
            vec![RecordOrigin::Live],
            "create_usage_records IS the live route, so every entry it \
             dispatches MUST carry origin = live",
        );
    }
}

// ── Server-stamped `accepted_at` ────────────────────────────────────────────
#[cfg(test)]
mod acceptance_stamp_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use usage_collector_sdk::v2::{
        CreateUsageRecord, EntryType, IdempotencyKey, MeterTypeId, ResourceRef,
        UsageCollectorPluginV2,
    };
    use uuid::Uuid;

    use crate::v2::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, fake_declaration_source_with_fold, qty,
        recent_window_end, recent_window_start,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");

    fn submission(idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(GTS_ID).unwrap(),
            tenant_id: Uuid::from_u128(1),
            resource_ref: ResourceRef::new("rsc-stamp", "compute.vm").unwrap(),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: qty("1"),
            idempotency_key: IdempotencyKey::new(idem).unwrap(),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    #[tokio::test]
    async fn every_entry_of_one_request_carries_one_microsecond_acceptance_instant() {
        let plugin = HappyPathPlugin::new();
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
                "test.accepted_at.stamp.records.v1",
            );
        let before = time::OffsetDateTime::now_utc();
        // The plugin's `create_records_response` is left unprogrammed, so the
        // call errors. What is asserted is what the plugin was HANDED, which
        // `HappyPathPlugin` captures regardless of the outcome.
        let _dispatch_outcome = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![
                    submission("stamp-a"),
                    submission("stamp-b"),
                    submission("stamp-c"),
                ],
            )
            .await;
        let after = time::OffsetDateTime::now_utc();

        let dispatched = plugin
            .last_create_records_input()
            .expect("the batch reached the plugin");
        assert_eq!(dispatched.len(), 3);
        let stamp = dispatched[0].accepted_at;
        assert!(
            dispatched.iter().all(|r| r.accepted_at == stamp),
            "one instant per request"
        );
        assert_eq!(
            stamp.nanosecond() % 1_000,
            0,
            "stamped at microsecond precision"
        );
        let floor = before.replace_microsecond(before.microsecond()).unwrap();
        assert!(floor <= stamp && stamp <= after, "stamped during the call");
    }
}

// ── Covered-period preconditions on the batch path ─────────────────────────
//
// The identity derivation is per-submission and fallible, so a rejected
// covered period is a PER-RECORD outcome routed to its own input index —
// never a batch-level failure, never a slot shifted onto a neighbour. The
// surviving vector is not index-aligned with the input, which is the mistake
// these tests exist to catch.
#[cfg(test)]
mod covered_period_batch_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use usage_collector_sdk::v2::{
        CreateUsageRecord, EntryType, IdempotencyKey, MeterTypeId, ResourceRef,
        UsageCollectorError, UsageCollectorPluginV2,
    };
    use uuid::Uuid;

    use crate::v2::domain::Service;
    use crate::v2::domain::test_support::{
        DenyOneResourceResolver, HappyPathPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_with_fold, projected, recent_window_end, recent_window_start,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");

    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV2>, suffix: &str) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(plugin, suffix)
    }

    /// Tenant `2` clears the fixture PDP; distinct `idem` values keep the
    /// submissions distinct without changing their attribution tuple, so the
    /// batch takes ONE PDP decision shared by every record — the arrangement
    /// in which a mis-routed index is invisible unless each outcome is
    /// asserted individually.
    fn submission(idem: &str) -> CreateUsageRecord {
        submission_for("rsc-period", idem)
    }

    fn submission_for(resource_id: &str, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id: Uuid::from_u128(2),
            resource_ref: ResourceRef::new(resource_id, "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::v2::domain::test_support::qty("1"),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    fn rejected_field(
        result: &Result<usage_collector_sdk::v2::UsageRecord, UsageCollectorError>,
    ) -> &str {
        match result {
            Err(UsageCollectorError::InvalidArgument { field, .. }) => field.as_str(),
            other => unreachable!("expected an InvalidArgument rejection, got {other:?}"),
        }
    }

    /// A batch of [valid, inverted-period, valid]: the middle submission is
    /// rejected at its OWN index and the two valid ones still reach the
    /// plugin, in input order.
    #[tokio::test]
    async fn a_rejected_period_surfaces_at_its_own_input_index() {
        let plugin = HappyPathPlugin::new();
        let good_0 = submission("idem-period-0");
        let mut bad = submission("idem-period-1");
        bad.window_end = bad.window_start - time::Duration::seconds(1);
        let good_2 = submission("idem-period-2");

        // Programming exactly two responses is itself a guard: a batch that
        // dispatched three would trip the dispatched-vs-returned invariant.
        plugin.set_create_records(vec![Ok(projected(&good_0)), Ok(projected(&good_2))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.batch.period_mixed.records.v1",
        );

        let results = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![good_0.clone(), bad, good_2.clone()],
            )
            .await
            .expect("a per-record period rejection MUST NOT fail the batch");

        assert_eq!(results.len(), 3, "one result slot per input submission");
        assert!(results[0].is_ok(), "index 0 must be accepted: {results:?}");
        assert_eq!(
            rejected_field(&results[1]),
            "window_end",
            "the rejection must land at index 1, attributed to window_end",
        );
        assert!(results[2].is_ok(), "index 2 must be accepted: {results:?}");

        let dispatched = plugin
            .last_create_records_input()
            .expect("plugin received the surviving batch");
        assert_eq!(
            dispatched
                .iter()
                .map(|r| r.idempotency_key.as_str())
                .collect::<Vec<_>>(),
            vec!["idem-period-0", "idem-period-2"],
            "only the two valid submissions reach the plugin, in input order",
        );
    }

    /// A sub-microsecond bound is the other precondition, and it is rejected
    /// per-record on the batch path too — attributed to whichever bound
    /// carried it.
    #[tokio::test]
    async fn a_sub_microsecond_bound_surfaces_at_its_own_input_index() {
        let plugin = HappyPathPlugin::new();
        let mut bad = submission("idem-period-sub-us");
        bad.window_start = bad.window_start.replace_nanosecond(1).expect("valid nanos");
        let good = submission("idem-period-clean");

        plugin.set_create_records(vec![Ok(projected(&good))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.batch.period_sub_us.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![bad, good.clone()])
            .await
            .expect("a per-record period rejection MUST NOT fail the batch");

        assert_eq!(results.len(), 2);
        assert_eq!(rejected_field(&results[0]), "window_start");
        assert!(results[1].is_ok(), "index 1 must be accepted: {results:?}");
    }

    /// Every submission rejected: the batch still returns `Ok` with a filled
    /// slot per input, and the plugin is never dispatched.
    ///
    /// `HappyPathPlugin` is left UNPROGRAMMED: an empty dispatch would surface
    /// as a batch-level `Err`, so `Ok(..)` is evidence the SPI was skipped
    /// rather than called with nothing. It also pins that the tail's "every
    /// slot populated" guard is satisfied by a converted-and-rejected slot,
    /// not by an invariant-breach `Internal`.
    #[tokio::test]
    async fn an_all_rejected_batch_returns_per_record_errors_without_dispatching() {
        let plugin = HappyPathPlugin::new();
        let mut first = submission("idem-period-all-0");
        first.window_end = first.window_start - time::Duration::seconds(1);
        let mut second = submission("idem-period-all-1");
        second.window_end = second
            .window_start
            .replace_nanosecond(7)
            .expect("valid nanos");

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.batch.period_all_rejected.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![first, second])
            .await
            .expect("an all-rejected batch is still an Ok batch");

        assert_eq!(results.len(), 2);
        assert_eq!(rejected_field(&results[0]), "window_end");
        assert_eq!(rejected_field(&results[1]), "window_end");
        assert!(
            plugin.last_create_records_input().is_none(),
            "no submission survived, so the persist SPI must not be dispatched",
        );
    }

    /// All three per-record outcomes in ONE batch — accepted,
    /// period-rejected, PDP-denied — deliberately interleaved so the
    /// rejected slot sits BETWEEN the other two.
    ///
    /// The interleaving is the point. The period-rejected submission never
    /// enters `derived`, so any pass that re-`enumerate()`s the surviving
    /// vector shifts the denial from input index 2 onto index 1, OVERWRITING
    /// the period error and promoting the denied record to accepted. A grouped
    /// layout would hide that as a mere invariant breach on an empty slot.
    #[tokio::test]
    async fn all_three_per_record_outcomes_coexist_at_their_own_input_indices() {
        let plugin = HappyPathPlugin::new();

        // Index 0: valid, permitted. Index 1: valid tuple but an inverted
        // period. Index 2: valid period, denied tuple (`rsc-DENY`).
        let accepted = submission_for("rsc-OK", "idem-three-way-accepted");
        let mut bad_period = submission_for("rsc-OK", "idem-three-way-period");
        bad_period.window_end = bad_period.window_start - time::Duration::seconds(1);
        let denied = submission_for("rsc-DENY", "idem-three-way-denied");

        // Exactly ONE record survives to the SPI; programming one response is
        // itself a guard against a batch that dispatched two.
        plugin.set_create_records(vec![Ok(projected(&accepted))]);

        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .with_resolver(DenyOneResourceResolver::new("rsc-DENY"))
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
                "test.batch.period_three_way.records.v1",
            );

        let expected_accepted_id = projected(&accepted).id;
        let results = service
            .create_usage_records(&authenticated_ctx(), vec![accepted, bad_period, denied])
            .await
            .expect("a mixed batch is still an Ok batch");

        assert_eq!(
            results.len(),
            3,
            "one result slot per input submission, whatever each outcome was",
        );
        assert_eq!(
            results[0]
                .as_ref()
                .expect("index 0 is permitted and well-formed")
                .id,
            expected_accepted_id,
            "index 0 must carry its OWN derived id",
        );
        assert_eq!(
            rejected_field(&results[1]),
            "window_end",
            "index 1 must carry the period rejection naming the offending \
             bound - not the denial that follows it in the input",
        );
        assert!(
            matches!(
                results[2],
                Err(UsageCollectorError::PermissionDenied { .. })
            ),
            "index 2 must carry the PDP denial, not an acceptance: {:?}",
            results[2],
        );

        let dispatched = plugin
            .last_create_records_input()
            .expect("the one surviving submission reached the plugin");
        assert_eq!(
            dispatched
                .iter()
                .map(|r| r.idempotency_key.as_str())
                .collect::<Vec<_>>(),
            vec!["idem-three-way-accepted"],
            "neither the rejected period nor the denied tuple may be dispatched",
        );
    }
}

// ── The live path's two-sided covered-period bound, at the service ─────────
//
// `covered_period_tests.rs` pins the rule itself. These pin that the Ingestion
// Gateway applies it — before the PDP call and before any plugin dispatch —
// and that it governs an invalidation over the period the invalidation copies.
mod covered_period_bounds_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use usage_collector_sdk::v2::{
        BACKFILL_ROUTE_PATH, CreateUsageRecord, EntryType, IdempotencyKey, MeterTypeId, ReasonCode,
        ResourceRef, UsageCollectorError, UsageCollectorPluginV2, ValidationReason,
    };
    use uuid::Uuid;

    use crate::v2::domain::Service;
    use crate::v2::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, fake_declaration_source_with_fold,
        projected, recent_window_end, recent_window_start,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");

    /// A [`ServiceFixture`] with an arbitrary working declaration — the bound
    /// is enforced ahead of the resolver, so the declaration only has to let
    /// an admitted entry through.
    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV2>, suffix: &str) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(plugin, suffix)
    }

    /// A submission the live path admits: the shared recent covered period,
    /// an hour long and closed an hour ago.
    fn fresh_record(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-bounds", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::v2::domain::test_support::qty("1"),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    /// The same submission, differing in the covered period alone: a month
    /// back, well beyond the live past tolerance and nowhere near the
    /// boundary, so the outcome does not depend on runner load. Offset from
    /// the memoised [`recent_window_end`] rather than a fresh `now_utc()`, so
    /// two submissions built by separate calls carry the SAME period — a
    /// per-call clock read lands them microseconds apart, and a withdrawal
    /// built that way is refused for `InvalidationFieldMismatch` instead.
    fn stale_record(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        let window_end = recent_window_end() - time::Duration::days(30);
        CreateUsageRecord {
            entry_type: EntryType::Record,
            window_start: window_end - time::Duration::hours(1),
            window_end,
            ..fresh_record(tenant_id, idem)
        }
    }

    fn invalid_argument(err: &UsageCollectorError) -> (&ValidationReason, &str) {
        match err {
            UsageCollectorError::InvalidArgument { reason, detail, .. } => (reason, detail),
            other => unreachable!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_live_path_refuses_a_period_older_than_the_past_tolerance_and_names_the_route() {
        let plugin = HappyPathPlugin::new();
        let submission = stale_record(Uuid::from_u128(0xC1), "idem-stale");
        // Armed to succeed, so the rejection can only come from the bound.
        plugin.set_create_records(vec![Ok(projected(&submission))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.bounds.live_past.record.v1",
        );

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect(
                "a rejected covered period is a precondition of that submission's identity \
                 derivation, per-record, never a request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a period 30 days old is beyond the 48-hour live tolerance");

        let (reason, detail) = invalid_argument(&err);
        assert_eq!(*reason, ValidationReason::PastWindow);
        assert!(
            detail.contains(BACKFILL_ROUTE_PATH),
            "the rejection MUST name the route the entry belongs on: {detail}",
        );
        assert_eq!(
            plugin.last_create_records_input(),
            None,
            "a refused period MUST NOT reach the storage plugin",
        );
    }

    #[tokio::test]
    async fn the_live_path_refuses_a_period_ending_beyond_the_future_tolerance() {
        // The only test that reaches the future tolerance THROUGH the Service
        // — the unit tests build `CoveredPeriodBounds` by hand, so they never
        // exercise the configured value's route from `[usage_collector]` to
        // this call. Without this, a projection that fed `backfill_window` in
        // as the future tolerance would ship green.
        let plugin = HappyPathPlugin::new();
        // `recent_window_end()` is an hour BEHIND now, so two hours on from it
        // is an hour AHEAD — outside a five-minute tolerance and nowhere near
        // the boundary. Anchored on the memoised fixture for the reason
        // `stale_record` documents.
        let window_end = recent_window_end() + time::Duration::hours(2);
        let submission = CreateUsageRecord {
            entry_type: EntryType::Record,
            window_start: window_end - time::Duration::hours(1),
            window_end,
            ..fresh_record(Uuid::from_u128(0xC5), "idem-future")
        };
        plugin.set_create_records(vec![Ok(projected(&submission))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.bounds.live_future.record.v1",
        );

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect(
                "a rejected covered period is a precondition of that submission's identity \
                 derivation, per-record, never a request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err =
            result.expect_err("a period ending an hour from now is beyond the 5-minute tolerance");

        let (reason, detail) = invalid_argument(&err);
        assert_eq!(*reason, ValidationReason::FutureWindow);
        assert!(
            !detail.contains(BACKFILL_ROUTE_PATH),
            "the backfill route lifts the PAST bound only, so pointing a \
             clock-skewed emitter at it would send a defect somewhere it is \
             just as invalid: {detail}",
        );
        assert_eq!(
            plugin.last_create_records_input(),
            None,
            "a refused period MUST NOT reach the storage plugin",
        );
    }

    #[tokio::test]
    async fn the_live_path_refuses_a_withdrawal_over_a_period_older_than_the_past_tolerance() {
        // The bound belongs to the path, not to the entry kind: an
        // invalidation is a faithful copy of its target, so its `window_end`
        // IS the target's, and withdrawing a closed month is refused on the
        // live path exactly as a fresh measurement of that month would be.
        //
        // If the bound read the arrival instant instead of the copied period,
        // this submission would be accepted and a correction of closed history
        // would persist reading `origin = live`.
        let tenant_id = Uuid::from_u128(0xC2);

        // A measurement whose period closed a month ago.
        let measurement = stale_record(tenant_id, "idem-target");
        let target_row = projected(&measurement);
        // A faithful copy of it, departing only in `entry_type` and the
        // reason: it repeats the target's idempotency key, which resolves the
        // target, and its covered period is the target's.
        let withdrawal = CreateUsageRecord {
            entry_type: EntryType::Invalidation,
            invalidation: Some(ReasonCode::new("emitter_defect").expect("valid reason code")),
            ..stale_record(tenant_id, "idem-target")
        };

        let plugin = HappyPathPlugin::new();
        plugin.set_get_record(target_row);
        plugin.set_create_records(vec![Ok(projected(&withdrawal))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.bounds.live_past.withdrawal.v1",
        );

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![withdrawal])
            .await
            .expect(
                "a rejected covered period is a precondition of that submission's identity \
                 derivation, per-record, never a request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a withdrawal of a closed month belongs on the backfill route");

        // The rejection must arrive on the PERIOD, not on the target: assert
        // the typed reason and that the target was never read. Neither holds
        // if the bound moved behind the target lookup.
        let (reason, detail) = invalid_argument(&err);
        assert_eq!(*reason, ValidationReason::PastWindow);
        assert!(detail.contains(BACKFILL_ROUTE_PATH), "{detail}");
        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "the period bound MUST refuse the entry before the target is read",
        );
    }

    #[tokio::test]
    async fn a_month_long_period_that_just_closed_is_ordinary_live_consumption() {
        // The bound reads the END of the covered period, so a monthly accrual
        // meter emitting the moment its month closes is admitted even though
        // the period it covers is fifteen times the live past tolerance.
        // Hand this call site `window_start` instead of `window_end` and only
        // this test goes red: every other ingestion fixture covers an hour, so
        // the swap is invisible inside a tolerance measured in days.
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xC4);
        // Truncated to the microsecond: a covered-period bound above
        // microsecond precision is rejected outright, because the identity
        // derivation reads a fixed-width microsecond form, and
        // `clock_gettime` reports nanoseconds on Linux.
        let now = time::OffsetDateTime::now_utc();
        let window_end = now
            .replace_nanosecond(now.microsecond() * 1_000)
            .expect("a microsecond-truncated nanosecond count is in range")
            - time::Duration::minutes(1);
        let submission = CreateUsageRecord {
            entry_type: EntryType::Record,
            window_start: window_end - time::Duration::days(30),
            window_end,
            ..fresh_record(tenant_id, "idem-month-long")
        };
        plugin.set_create_records(vec![Ok(projected(&submission))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.bounds.live_past.month_long.v1",
        );

        service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect("the measurement is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("a 30-day period that closed a minute ago is live consumption");
    }

    #[tokio::test]
    async fn a_batch_rejects_only_the_entries_whose_period_is_out_of_bounds() {
        // Per-submission, at its own input index, with the surviving entries
        // still dispatched: a batch-level rejection would make one stale entry
        // discard a whole import.
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xC3);
        let input = vec![
            fresh_record(tenant_id, "idem-ok-0"),
            stale_record(tenant_id, "idem-stale-1"),
            fresh_record(tenant_id, "idem-ok-2"),
        ];
        // Two per-record outcomes, not three: a refused period never reaches
        // the plugin, and a third would silently absorb a routing mistake.
        plugin.set_create_records(vec![Ok(projected(&input[0])), Ok(projected(&input[2]))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.bounds.live_past.batch.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("a refused period is per-entry, never batch-level");

        assert_eq!(results.len(), 3, "one result per input, in input order");
        assert!(results[0].is_ok(), "{:?}", results[0]);
        assert!(results[2].is_ok(), "{:?}", results[2]);
        let Err(err) = &results[1] else {
            unreachable!("index 1 must carry the rejection, at its own index");
        };
        assert_eq!(*invalid_argument(err).0, ValidationReason::PastWindow);

        let dispatched = plugin
            .last_create_records_input()
            .expect("the surviving entries MUST still be dispatched");
        assert_eq!(
            dispatched
                .iter()
                .map(|r| r.idempotency_key.as_str())
                .collect::<Vec<_>>(),
            vec!["idem-ok-0", "idem-ok-2"],
            "only the out-of-bounds entry is withheld",
        );
    }
}

// ── The backfill route ─────────────────────────────────────────────────────
//
// `Service::backfill_usage_records` is the live batch body under a different
// `origin`, so what is pinned here is what the origin changes: the marker
// every accepted entry carries, the past bound it lifts, that the lift reaches
// a withdrawal of closed history, and the PDP verb it derives per entry from
// the covered period. The live path's own cases are in
// `covered_period_bounds_tests` above.
mod backfill_route_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use authz_resolver_sdk::AuthZResolverApi;
    use toolkit_gts::gts_id;
    use usage_collector_sdk::v2::{
        BACKFILL_ROUTE_PATH, CreateUsageRecord, EntryType, IdempotencyKey, MeterTypeId, ReasonCode,
        RecordOrigin, ResourceRef, UsageCollectorError, UsageCollectorPluginV2, ValidationReason,
    };
    use uuid::Uuid;

    use crate::v2::domain::Service;
    use crate::v2::domain::test_support::{
        ActionRecordingPermitResolver, HappyPathPlugin, ServiceFixture, authenticated_ctx,
        default_covered_period_bounds, fake_declaration_source_with_fold, projected_with_origin,
        recent_window_end, recent_window_start,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");

    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV2>, suffix: &str) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(plugin, suffix)
    }

    /// A submission over the shared recent covered period — the one the
    /// live path admits.
    fn fresh_record(tenant_id: Uuid, resource_id: &str, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new(resource_id, "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::v2::domain::test_support::qty("1"),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    /// The same submission over a covered period `days` old. Offset from the
    /// memoised [`recent_window_end`] rather than a fresh `now_utc()`, so two
    /// submissions built by separate calls carry the SAME period — two clock
    /// reads derive two different ids and make a withdrawal an unfaithful
    /// copy of its target.
    fn aged_record(tenant_id: Uuid, resource_id: &str, idem: &str, days: i64) -> CreateUsageRecord {
        let window_end = recent_window_end() - time::Duration::days(days);
        CreateUsageRecord {
            entry_type: EntryType::Record,
            window_start: window_end - time::Duration::hours(1),
            window_end,
            ..fresh_record(tenant_id, resource_id, idem)
        }
    }

    fn invalid_argument(err: &UsageCollectorError) -> (&ValidationReason, &str) {
        match err {
            UsageCollectorError::InvalidArgument { reason, detail, .. } => (reason, detail),
            other => unreachable!("expected InvalidArgument, got {other:?}"),
        }
    }

    /// The fresh entry is the load-bearing half: a batch of nothing but aged
    /// periods would also be stamped correctly by a route that derived
    /// `origin` from the period's age rather than from the entry point. The
    /// assertion reads what the gateway HANDED the plugin, not what the plugin
    /// handed back — the echo is a fixture and would answer `backfill`
    /// however the entry was stamped.
    #[tokio::test]
    async fn the_backfill_route_stamps_every_accepted_entry_with_backfill() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xD1);
        let input = vec![
            aged_record(tenant_id, "rsc-import", "idem-import-aged", 30),
            fresh_record(tenant_id, "rsc-import", "idem-import-fresh"),
        ];
        plugin.set_create_records(
            input
                .iter()
                .map(|r| Ok(projected_with_origin(r, RecordOrigin::Backfill)))
                .collect(),
        );
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.backfill.stamp.records.v1",
        );

        let results = service
            .backfill_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");

        assert_eq!(results.len(), 2);
        assert!(results.iter().all(Result::is_ok), "{results:?}");

        let dispatched = plugin
            .last_create_records_input()
            .expect("both entries reached the storage plugin");
        assert_eq!(
            dispatched.iter().map(|r| r.origin).collect::<Vec<_>>(),
            vec![RecordOrigin::Backfill, RecordOrigin::Backfill],
            "the route stamps its own origin on every entry it admits, \
             whatever each entry's covered period is",
        );
        assert_eq!(
            results[0].as_ref().expect("accepted").origin,
            RecordOrigin::Backfill,
            "and the marker survives to the caller",
        );
    }

    /// Both halves in one test so they cannot drift apart: the live path
    /// refuses the period and names the route, and the route admits it.
    #[tokio::test]
    async fn the_backfill_route_admits_the_period_the_live_path_rejected() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xD2);
        let stale = aged_record(tenant_id, "rsc-both", "idem-both", 30);
        plugin.set_create_records(vec![Ok(projected_with_origin(
            &stale,
            RecordOrigin::Backfill,
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.backfill.admits.records.v1",
        );

        let live_result = service
            .create_usage_records(&authenticated_ctx(), vec![stale.clone()])
            .await
            .expect(
                "a rejected covered period is a precondition of that submission's identity \
                 derivation, per-record, never a request-wide refusal - and the rejection \
                 happens before persist dispatch, so the batch response programmed below for \
                 the backfill attempt is left untouched",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");
        let live_err = live_result.expect_err("30 days is beyond the 48-hour live past tolerance");
        let (reason, detail) = invalid_argument(&live_err);
        assert_eq!(*reason, ValidationReason::PastWindow);
        assert!(
            detail.contains(BACKFILL_ROUTE_PATH),
            "the rejection names the route this test then exercises: {detail}",
        );

        let results = service
            .backfill_usage_records(&authenticated_ctx(), vec![stale])
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 1);
        assert!(
            results[0].is_ok(),
            "the route exists for exactly the period the live path named it \
             for: {results:?}",
        );
    }

    /// The route lifts the past bound and nothing else. A future-dated entry
    /// is a clock-skewed emitter, and pointing one at the import route would
    /// send a defect somewhere it is just as invalid. The rejection is
    /// per-record, at the entry's input index, because a refused covered
    /// period is a precondition of that submission's identity derivation
    /// rather than of the batch.
    #[tokio::test]
    async fn the_backfill_route_refuses_a_period_ending_beyond_the_future_tolerance() {
        assert!(
            time::Duration::hours(1) > default_covered_period_bounds().future_tolerance,
            "the fixture puts the period an hour ahead of now and needs that \
             to be outside the configured tolerance, which is {:?}",
            default_covered_period_bounds().future_tolerance,
        );

        let plugin = HappyPathPlugin::new();
        // `recent_window_end()` is an hour BEHIND now, so two hours on from it
        // is an hour AHEAD. Anchored on the memoised fixture for the reason
        // `aged_record` documents.
        let window_end = recent_window_end() + time::Duration::hours(2);
        let submission = CreateUsageRecord {
            entry_type: EntryType::Record,
            window_start: window_end - time::Duration::hours(1),
            window_end,
            ..fresh_record(Uuid::from_u128(0xD5), "rsc-future", "idem-import-future")
        };
        // Armed to succeed, so the rejection below can only come from the bound.
        plugin.set_create_records(vec![Ok(projected_with_origin(
            &submission,
            RecordOrigin::Backfill,
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.backfill.future.records.v1",
        );

        let results = service
            .backfill_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect("the batch itself dispatches; the entry is refused in its own slot");

        assert_eq!(results.len(), 1);
        let err = results[0]
            .as_ref()
            .expect_err("the import route lifts the past bound, not the future one");
        let (reason, detail) = invalid_argument(err);
        assert_eq!(*reason, ValidationReason::FutureWindow);
        assert!(
            !detail.contains(BACKFILL_ROUTE_PATH),
            "the entry is already ON the backfill route, so naming it as the \
             place the entry belongs would be a loop: {detail}",
        );
        assert_eq!(
            plugin.last_create_records_input(),
            None,
            "a refused period MUST NOT reach the storage plugin",
        );
    }

    /// The target is built with `origin = Live` and withdrawn on the backfill
    /// route. `origin` is NOT one of the fields the faithful-copy rule
    /// compares, so a `live` target and a `backfill` withdrawal is the
    /// ordinary pair rather than a mismatch. If the comparator ever started
    /// reading `origin`, this is the test that goes red.
    #[tokio::test]
    async fn a_withdrawal_of_closed_history_is_refused_live_and_accepted_on_backfill() {
        let tenant_id = Uuid::from_u128(0xD3);

        // A measurement whose period closed a month ago, persisted as the live
        // path would have left it.
        let measurement = aged_record(tenant_id, "rsc-withdraw", "idem-target", 30);
        let target_row = projected_with_origin(&measurement, RecordOrigin::Live);
        assert_eq!(
            target_row.origin,
            RecordOrigin::Live,
            "the target must be a LIVE entry, or the pair this test is about \
             is not the pair it built",
        );
        // A faithful copy of it, departing only in `entry_type` and the
        // reason, repeating the key that resolves the target.
        let withdrawal = CreateUsageRecord {
            entry_type: EntryType::Invalidation,
            invalidation: Some(ReasonCode::new("emitter_defect").expect("valid reason code")),
            ..aged_record(tenant_id, "rsc-withdraw", "idem-target", 30)
        };

        let plugin = HappyPathPlugin::new();
        plugin.set_get_record(target_row.clone());
        plugin.set_create_records(vec![Ok(projected_with_origin(
            &withdrawal,
            RecordOrigin::Backfill,
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
            "test.backfill.withdrawal.records.v1",
        );

        let live_result = service
            .create_usage_records(&authenticated_ctx(), vec![withdrawal.clone()])
            .await
            .expect(
                "a rejected covered period is a precondition of that submission's identity \
                 derivation, per-record, never a request-wide refusal - and the rejection \
                 happens before the target lookup, so neither programmed batch response is \
                 touched by this attempt",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");
        let live_err =
            live_result.expect_err("a withdrawal of a closed month belongs on the backfill route");
        let (reason, detail) = invalid_argument(&live_err);
        assert_eq!(*reason, ValidationReason::PastWindow);
        assert!(detail.contains(BACKFILL_ROUTE_PATH), "{detail}");

        let results = service
            .backfill_usage_records(&authenticated_ctx(), vec![withdrawal])
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 1);
        let accepted = results[0]
            .as_ref()
            .expect("a faithful withdrawal of a live target is accepted here");
        assert_eq!(accepted.origin, RecordOrigin::Backfill);
        assert_eq!(
            plugin.get_usage_record_calls(),
            1,
            "the target was read exactly once, on the backfill attempt; the live \
             attempt was refused by the period bound before the lookup",
        );
        let dispatched = plugin
            .last_create_records_input()
            .expect("the withdrawal reached the storage plugin");
        assert_eq!(dispatched[0].origin, RecordOrigin::Backfill);
    }

    /// [`crate::v2::domain::authz::AttributionTupleKey`] carries `action` in its
    /// hash/eq so a batch bound to different actions cannot collapse onto a
    /// single PDP decision. Both entries here share ONE attribution tuple, so
    /// only the action keeps them apart: drop `action` from the key and the
    /// entry reaching past the backfill window rides in on the other's
    /// `create` permit. The assertion is on the recorded action STRINGS —
    /// counting calls would pass for the wrong reason, since a dedup broken on
    /// some other field also makes two calls.
    #[tokio::test]
    async fn one_backfill_batch_mixing_window_sides_authorizes_two_distinct_actions() {
        let bounds = default_covered_period_bounds();
        let inside_days = 30_i64;
        let beyond_days = 120_i64;
        assert!(
            time::Duration::days(inside_days) < bounds.backfill_window
                && time::Duration::days(beyond_days) > bounds.backfill_window,
            "the fixture's two periods MUST straddle the configured backfill \
             window ({:?}) or this test silently stops mixing actions",
            bounds.backfill_window,
        );

        let tenant_id = Uuid::from_u128(0xD4);
        let resolver = ActionRecordingPermitResolver::new();
        let plugin = HappyPathPlugin::new();
        // One attribution tuple, two idempotency keys: the submissions are
        // distinct entries that a PDP dedup keyed on anything but `action`
        // would fold together.
        let input = vec![
            aged_record(tenant_id, "rsc-mixed", "idem-inside", inside_days),
            aged_record(tenant_id, "rsc-mixed", "idem-beyond", beyond_days),
        ];
        plugin.set_create_records(
            input
                .iter()
                .map(|r| Ok(projected_with_origin(r, RecordOrigin::Backfill)))
                .collect(),
        );
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .with_resolver(Arc::clone(&resolver) as Arc<dyn AuthZResolverApi>)
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
                "test.backfill.mixed.records.v1",
            );

        let results = service
            .backfill_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert!(
            results.iter().all(Result::is_ok),
            "crossing the backfill window selects a different verb; it does \
             NOT reject the entry: {results:?}",
        );

        assert_eq!(
            resolver.actions_sorted(),
            vec!["backfill".to_owned(), "create".to_owned()],
            "one batch, one attribution tuple, two actions: the PDP must see \
             both",
        );
    }
}

// Ingestion validates against the meter's resolved declaration instead of a
// plugin-owned catalog row: the per-distinct-`gts_id` fan-out in
// `create_usage_records` is a resolver fan-out over the same Type Resolver the
// aggregate path uses.
mod ingestion_declared_type_tests {
    use std::collections::BTreeMap;

    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::v2::{
        CreateUsageRecord, EntryType, IdempotencyKey, MetadataKey, MeterTypeId, ResourceRef,
        UsageCollectorPluginV2,
    };
    use uuid::Uuid;

    use crate::v2::domain::Service;
    use crate::v2::domain::ports::declarations::DeclarationSource;
    use crate::v2::domain::test_support::{
        RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_counting, fake_declaration_source_not_found,
        fake_declaration_source_with_fold, fake_declaration_source_with_metadata, projected,
        recent_window_end, recent_window_start, recording_plugin_resolver,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// A `Service` + [`RecordingPlugin`] spy over `source` at the default
    /// metadata size cap, wired against the fixed-tenant PDP fake.
    fn service_with_recording_plugin(
        source: Arc<dyn DeclarationSource>,
    ) -> (Arc<Service>, Arc<RecordingPlugin>) {
        service_with_recording_plugin_and_cap(
            source,
            crate::v2::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES,
        )
    }

    /// Variant of [`service_with_recording_plugin`] taking an explicit
    /// `metadata_size_cap_bytes`.
    fn service_with_recording_plugin_and_cap(
        source: Arc<dyn DeclarationSource>,
        metadata_size_cap_bytes: usize,
    ) -> (Arc<Service>, Arc<RecordingPlugin>) {
        let plugin = RecordingPlugin::new();
        let service = ServiceFixture::default()
            .with_source(source)
            .with_resolver(recording_plugin_resolver())
            .with_cap(metadata_size_cap_bytes)
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
                RECORDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    /// A minimal valid `CreateUsageRecord`, tenant-matched to
    /// `service_with_recording_plugin`'s fixed-tenant PDP permit
    /// (`Uuid::from_u128(2)`) so it clears authorization regardless of the
    /// declaration under test.
    fn valid_create_record() -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id: Uuid::from_u128(2),
            resource_ref: ResourceRef::new("rsc-ingestion-declared", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::v2::domain::test_support::qty("1"),
            idempotency_key: IdempotencyKey::new(format!("idem-{}", Uuid::new_v4()))
                .expect("valid idempotency key"),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    #[tokio::test]
    async fn ingestion_rejects_an_undeclared_metadata_key() {
        let source = fake_declaration_source_with_metadata(&["region"]);
        let (svc, spy) = service_with_recording_plugin(source);

        let mut record = valid_create_record();
        record.metadata.insert(
            MetadataKey::new("tier").expect("valid key"),
            "gold".to_owned(),
        );

        let result = svc
            .create_usage_records(&ctx(), vec![record])
            .await
            .expect(
                "an undeclared metadata key is a per-record validation rejection, never a \
                 request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("an undeclared key must be rejected before persistence");
        assert!(
            err.to_string().contains("tier"),
            "the rejection must name the offending key: {err}",
        );
        // `spy.calls()` counts `query_aggregated_usage_records` dispatches
        // only; `last_create_records_input` is the create-path probe.
        assert!(
            spy.last_create_records_input().is_none(),
            "a rejected entry must not reach the plugin",
        );
    }

    #[tokio::test]
    async fn ingestion_accepts_declared_metadata_keys() {
        let source = fake_declaration_source_with_metadata(&["region"]);
        let (svc, spy) = service_with_recording_plugin(source);

        let mut record = valid_create_record();
        record.metadata.insert(
            MetadataKey::new("region").expect("valid key"),
            "eu-west-1".to_owned(),
        );
        spy.set_create_records(vec![Ok(projected(&record))]);

        svc.create_usage_records(&ctx(), vec![record])
            .await
            .expect("the measurement is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("a declared metadata key must be accepted");
        assert!(
            spy.last_create_records_input().is_some(),
            "an accepted entry must reach the persist SPI",
        );
    }

    #[tokio::test]
    async fn ingestion_fails_closed_when_the_type_does_not_resolve() {
        // 2.1 fail-closed: an unresolvable reference is rejected, never
        // admitted unvalidated to protect ingestion availability.
        let source = fake_declaration_source_not_found();
        let (svc, spy) = service_with_recording_plugin(source);

        let result = svc
            .create_usage_records(&ctx(), vec![valid_create_record()])
            .await
            .expect(
                "an unresolvable declaration is a per-record rejection (the declaration \
                 pre-pass writes to the slot), never a request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");
        assert!(result.is_err());
        assert!(
            spy.last_create_records_input().is_none(),
            "an unresolvable type must never reach the plugin",
        );
    }

    #[tokio::test]
    async fn a_batch_resolves_each_distinct_type_once() {
        // The resolver fan-out replaces the catalog fan-out: one resolution
        // per distinct gts_id, not one per record.
        let source = fake_declaration_source_counting();
        let (svc, spy) = service_with_recording_plugin(source.clone());

        let records = vec![
            valid_create_record(),
            valid_create_record(),
            valid_create_record(),
        ];
        spy.set_create_records(records.iter().map(|r| Ok(projected(r))).collect());

        let results = svc
            .create_usage_records(&ctx(), records)
            .await
            .expect("batch accepted");
        assert!(results.iter().all(Result::is_ok));

        assert_eq!(
            source.fetch_calls(),
            1,
            "three records of one type resolve once",
        );
    }

    /// `Service::metadata_size_cap_bytes` actually drives the cap
    /// `validate_submit_record_metadata` enforces — not a hard-coded constant.
    /// `validation_tests.rs` pins the parameter but cannot prove `Service`
    /// forwards its own field.
    #[tokio::test]
    async fn ingestion_honors_a_configured_non_default_metadata_size_cap() {
        let source = fake_declaration_source_with_metadata(&["blob"]);
        let configured_cap = 16;
        let (svc, spy) = service_with_recording_plugin_and_cap(source, configured_cap);

        let mut record = valid_create_record();
        record
            .metadata
            .insert(MetadataKey::new("blob").expect("valid key"), "x".repeat(64));

        let result = svc
            .create_usage_records(&ctx(), vec![record])
            .await
            .expect(
                "a metadata-size-cap rejection is a per-record validation outcome, never a \
                 request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a payload over the configured cap must be rejected");
        assert!(
            err.to_string().contains(&configured_cap.to_string()),
            "the rejection must name the configured cap, not a hard-coded default: {err}",
        );
        assert!(
            spy.last_create_records_input().is_none(),
            "a rejected entry must not reach the plugin",
        );
    }

    /// The ingestion path accepts a `COUNT` meter's entry with any quantity
    /// value: ingestion never consults the declared fold.
    /// `ResolvedDeclaration::aggregation_fold`'s only production readers are
    /// `query_aggregated_usage_records` and `get_reconciliation_metadata`.
    #[tokio::test]
    async fn ingestion_accepts_a_count_meters_entry_with_any_quantity_value() {
        let source = fake_declaration_source_with_fold("COUNT");
        let (svc, spy) = service_with_recording_plugin(source);

        // Zero, a plain integer and a fractional value: an ingestion path
        // wrongly validating the quantity against the fold could not accept
        // all three uniformly.
        for quantity in ["0", "1", "123.456"] {
            let mut record = valid_create_record();
            record.quantity = crate::v2::domain::test_support::qty(quantity);
            record.idempotency_key = IdempotencyKey::new(format!("idem-{}", Uuid::new_v4()))
                .expect("valid idempotency key");
            spy.set_create_records(vec![Ok(projected(&record))]);

            svc.create_usage_records(&ctx(), vec![record])
                .await
                .expect("the measurement is not refused request-wide")
                .into_iter()
                .next()
                .expect("one entry in, one slot out")
                .unwrap_or_else(|e| {
                    unreachable!(
                        "quantity {quantity} must be accepted for a COUNT meter, got {e:?}"
                    )
                });
        }
        assert!(
            spy.last_create_records_input().is_some(),
            "an accepted COUNT-meter entry must reach the persist SPI",
        );
    }
}
