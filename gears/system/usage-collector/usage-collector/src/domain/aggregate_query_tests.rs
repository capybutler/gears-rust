//! Unit tests for the aggregate read path orchestration built in
//! [`crate::domain::aggregate_query`] (`Service::query_aggregated_usage_records`).
//!
//! The query-surface gate itself is unit-tested in `query_tests.rs`;
//! `require_dimensions_declared` is unit-tested directly below. The `mod`s
//! past that point prove both are wired into the service at the right
//! position (before dispatch) with the right arguments, and cover what only
//! a `Service` fixture can show: the declared fold reaching the plugin
//! rather than a caller- or hard-coded one, a meter-narrowed PDP grant's
//! denial of the whole aggregate read, the composed (PDP-scoped) `$filter`
//! actually reaching the plugin rather than the caller's own, and withdrawal
//! exclusion from the fold.

// require_dimensions_declared — Spec §3.11: the admissible `group_by`
// surface is the fixed dimensions plus the queried meter's declared
// metadata keys, recomputed per request.
mod require_dimensions_declared_tests {
    use std::collections::BTreeSet;

    use usage_collector_sdk::{
        AggregationDimension, MetadataKey, MeterTypeId, UsageCollectorError, ValidationReason,
    };

    use crate::domain::aggregate_query::require_dimensions_declared;

    fn declared(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|k| (*k).to_owned()).collect()
    }

    /// A fixed queried-meter id for tests that don't care which meter, only
    /// that `require_dimensions_declared` threads it onto the error.
    fn meter_id() -> MeterTypeId {
        const GTS_ID: &str =
            toolkit_gts::gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    /// Assert `err` is the canonical undeclared-metadata-dimension rejection —
    /// attributed to `group_by` with `ValidationReason::UnknownMetadataKey`,
    /// `resource_name` carrying the queried meter — and that its `detail`
    /// names the offending key.
    fn assert_undeclared_dimension(err: UsageCollectorError, key: &str) {
        match err {
            UsageCollectorError::InvalidArgument {
                resource_name,
                field,
                reason,
                detail,
                ..
            } => {
                assert_eq!(
                    field, "group_by",
                    "undeclared-dimension violation attributes to group_by"
                );
                assert_eq!(reason, ValidationReason::UnknownMetadataKey);
                assert_eq!(
                    resource_name.as_deref(),
                    Some(meter_id().as_str()),
                    "resource_name must carry the queried meter, matching \
                     unknown_metadata_key's operator-log shape",
                );
                assert!(
                    detail.contains(key),
                    "detail must name the offending key '{key}': {detail}"
                );
            }
            other => panic!("expected InvalidArgument/UnknownMetadataKey, got {other:?}"),
        }
    }

    /// Assert `err` is the canonical repeated-grouping-dimension rejection —
    /// attributed to `group_by` with `ValidationReason::Validation`,
    /// `resource_name` carrying the queried meter — and that its `detail`
    /// contains `wire_fragment` (the repeated dimension's wire-rendered form).
    ///
    /// Asserting `reason` is load-bearing beyond this helper's own callers:
    /// `two_undeclared_dimensions_report_undeclared_rather_than_repeated`'s only
    /// oracle for "declaredness fired, not uniqueness" is that the two
    /// constructors carry different reasons (`Validation` vs
    /// `UnknownMetadataKey`).
    fn assert_repeated_dimension(err: UsageCollectorError, wire_fragment: &str) {
        match err {
            UsageCollectorError::InvalidArgument {
                resource_name,
                field,
                reason,
                detail,
                ..
            } => {
                assert_eq!(
                    field, "group_by",
                    "repeated-dimension violation attributes to group_by"
                );
                assert_eq!(
                    reason,
                    ValidationReason::Validation,
                    "repeated_grouping_dimension must carry a reason distinct from \
                     undeclared_metadata_dimension's UnknownMetadataKey: got detail {detail}"
                );
                assert_eq!(
                    resource_name.as_deref(),
                    Some(meter_id().as_str()),
                    "resource_name must carry the queried meter, matching \
                     undeclared_metadata_dimension's operator-log shape",
                );
                assert!(
                    detail.contains(wire_fragment),
                    "detail must name the repeated dimension's wire form '{wire_fragment}': {detail}"
                );
            }
            other => panic!("expected InvalidArgument/Validation, got {other:?}"),
        }
    }

    #[test]
    fn a_declared_metadata_key_is_groupable() {
        let dims = [AggregationDimension::Metadata(
            MetadataKey::new("region").unwrap(),
        )];
        require_dimensions_declared(&dims, &declared(&["region", "storage_class"]), &meter_id())
            .expect("a declared key is groupable");
    }

    #[test]
    fn an_undeclared_metadata_key_is_rejected_and_named() {
        let dims = [AggregationDimension::Metadata(
            MetadataKey::new("tier").unwrap(),
        )];
        let err = require_dimensions_declared(&dims, &declared(&["region"]), &meter_id())
            .expect_err("an undeclared key must not be groupable");
        assert!(err.to_string().contains("tier"));
        assert_undeclared_dimension(err, "tier");
    }

    #[test]
    fn the_fixed_dimensions_need_no_declaration() {
        let dims = [
            AggregationDimension::TenantId,
            AggregationDimension::ResourceId,
        ];
        require_dimensions_declared(&dims, &declared(&[]), &meter_id())
            .expect("fixed fields always admissible");
    }

    #[test]
    fn admissibility_is_recomputed_per_request_not_cached() {
        // Same dimension, same call site — only the declared-keys argument
        // differs between the two invocations. A stale-cache bug (an
        // admissible set computed once and reused) would make both calls agree;
        // a correct implementation reads `declared_keys` fresh each time.
        let dims = [AggregationDimension::Metadata(
            MetadataKey::new("region").unwrap(),
        )];

        require_dimensions_declared(&dims, &declared(&[]), &meter_id())
            .expect_err("not yet declared: must be rejected");
        require_dimensions_declared(&dims, &declared(&["region"]), &meter_id())
            .expect("declared a moment later: must now be admissible, without a restart");
        // And the reverse direction: withdrawing the declaration must be
        // honored on the very next call too, not just its addition.
        require_dimensions_declared(&dims, &declared(&[]), &meter_id())
            .expect_err("withdrawn: must be rejected again on the next call");
    }

    /// A repeated grouping dimension is refused naming the repeat.
    ///
    /// DESIGN's `AggregationDimension` row says a request names the admissible
    /// dimensions "each at most once" and the yaml declares `uniqueItems: true`
    /// on `group_by`. Unenforced, a duplicate groups to the same partition and
    /// the caller gets a bucket key carrying one value twice — a right number
    /// for a malformed request, with nothing saying it was malformed.
    #[test]
    fn a_repeated_grouping_dimension_is_refused() {
        let dims = vec![
            AggregationDimension::TenantId,
            AggregationDimension::TenantId,
        ];
        let err = require_dimensions_declared(&dims, &declared(&[]), &meter_id())
            .expect_err("a repeated dimension is refused");
        assert_repeated_dimension(err, "tenant_id");
    }

    /// A repeated **metadata** grouping dimension is refused naming the wire
    /// form the caller actually sent — a tagged object (`{"metadata":"region"}`,
    /// per `AggregationDimension`'s `#[serde(rename_all = "snake_case")]` with a
    /// newtype `Metadata` variant) — not `AggregationDimension`'s Rust-side
    /// `Debug` rendering (`Metadata(MetadataKey("region"))`), which
    /// `dimension_wire_name` exists to avoid.
    ///
    /// Pins the metadata branch of `dimension_wire_name` specifically: `Debug`
    /// renders `TenantId` with no case change, so the fixed-variant test barely
    /// discriminates, and a mutation collapsing the metadata branch onto the
    /// `Debug` fallback reds nowhere else.
    #[test]
    fn a_repeated_metadata_dimension_is_refused_naming_the_wire_form() {
        let dims = vec![
            AggregationDimension::Metadata(MetadataKey::new("region").expect("declared")),
            AggregationDimension::Metadata(MetadataKey::new("region").expect("declared")),
        ];
        let err = require_dimensions_declared(&dims, &declared(&["region"]), &meter_id())
            .expect_err("a repeated metadata dimension is refused");
        assert_repeated_dimension(err, r#"{"metadata":"region"}"#);
    }

    /// ABSENCE PIN — there is no arity ceiling, and there must not become one.
    ///
    /// DESIGN says a request is bounded "never by a fixed ceiling on how many a
    /// request may carry", PRD §5.1 says "in any combination and any order", and
    /// the yaml's `group_by` description agrees. Pinned rather than merely not
    /// built, because an unpinned absence is what let an invented "bounded band"
    /// survive.
    ///
    /// Paired with the rejection above because `usage-query.md`'s §6 criterion
    /// pairs them: "A grouping list repeating one dimension is rejected, while a
    /// list naming every admissible dimension once, in an arbitrary order, is
    /// admitted."
    #[test]
    fn every_admissible_dimension_once_in_an_arbitrary_order_is_admitted() {
        // All five fixed dimensions plus two declared metadata keys, shuffled.
        let dims = vec![
            AggregationDimension::Metadata(MetadataKey::new("region").expect("declared")),
            AggregationDimension::SubjectType,
            AggregationDimension::TenantId,
            AggregationDimension::Metadata(MetadataKey::new("tier").expect("declared")),
            AggregationDimension::ResourceType,
            AggregationDimension::SubjectId,
            AggregationDimension::ResourceId,
        ];
        require_dimensions_declared(&dims, &declared(&["region", "tier"]), &meter_id())
            .expect("seven dimensions, each once, is admissible: arity is bounded by the set");
    }

    /// A metadata key differing only in case is undeclared, not repeated —
    /// because declaredness wins regardless of how the uniqueness pass compares.
    ///
    /// Declaredness is an exact `BTreeSet<String>` match and runs first, so
    /// `Region` is refused as undeclared where only `region` is declared,
    /// whatever the uniqueness pass would have done. **This does not pin
    /// case-sensitivity of the uniqueness comparison** —
    /// `two_case_varied_declared_dimensions_are_two_distinct_dimensions` does,
    /// with both spellings declared so that pass is reached.
    #[test]
    fn a_case_varied_metadata_dimension_is_undeclared_rather_than_repeated() {
        let dims = vec![
            AggregationDimension::Metadata(MetadataKey::new("Region").expect("a valid key")),
            AggregationDimension::Metadata(MetadataKey::new("region").expect("a valid key")),
        ];
        let err = require_dimensions_declared(&dims, &declared(&["region"]), &meter_id())
            .expect_err("`Region` is not declared");
        let UsageCollectorError::InvalidArgument {
            ref reason,
            ref detail,
            ..
        } = err
        else {
            panic!("expected InvalidArgument, got {err:?}");
        };
        // The `reason` discriminator, not the `detail` wording, is what proves
        // declaredness fired rather than uniqueness: both details embed the key
        // string and neither contains the literal word "repeat", so a
        // `detail`-only assertion passes under either code path.
        assert_eq!(
            *reason,
            ValidationReason::UnknownMetadataKey,
            "declaredness, not uniqueness, must be what fires; got {detail}"
        );
        assert!(
            detail.contains("Region"),
            "the caller's typo is what they can fix; got {detail}"
        );
        assert!(
            !detail.to_lowercase().contains("repeat"),
            "a typo is not a duplicate; got {detail}"
        );
    }

    /// The uniqueness comparison is exact, so two spellings
    /// differing only in case are two dimensions rather than a repeat.
    ///
    /// The only test reaching the uniqueness pass with a case-varied pair: both
    /// keys are declared, so declaredness does not short-circuit first. Red under
    /// a case-insensitive pass, which would fold the two into one and refuse a
    /// legitimate request as a duplicate.
    #[test]
    fn two_case_varied_declared_dimensions_are_two_distinct_dimensions() {
        let dims = vec![
            AggregationDimension::Metadata(MetadataKey::new("Region").expect("a valid key")),
            AggregationDimension::Metadata(MetadataKey::new("region").expect("a valid key")),
        ];
        require_dimensions_declared(&dims, &declared(&["region", "Region"]), &meter_id())
            .expect("two spellings, both declared, are two distinct keys under exact comparison");
    }

    /// Declaredness is reported before repetition, for the same reason.
    #[test]
    fn two_undeclared_dimensions_report_undeclared_rather_than_repeated() {
        let dims = vec![
            AggregationDimension::Metadata(MetadataKey::new("nope").expect("a valid key")),
            AggregationDimension::Metadata(MetadataKey::new("nope").expect("a valid key")),
        ];
        let err = require_dimensions_declared(&dims, &declared(&[]), &meter_id())
            .expect_err("`nope` is not declared");
        let UsageCollectorError::InvalidArgument {
            ref reason,
            ref detail,
            ..
        } = err
        else {
            panic!("expected InvalidArgument, got {err:?}");
        };
        // Both dimensions are the SAME key, so an order bug running uniqueness
        // first would produce a detail containing "nope" too. Only the `reason`
        // discriminates: `undeclared_metadata_dimension` alone sets
        // `UnknownMetadataKey`.
        assert_eq!(
            *reason,
            ValidationReason::UnknownMetadataKey,
            "declaredness, not uniqueness, must be what fires; got {detail}"
        );
        assert!(
            detail.contains("nope") && !detail.to_lowercase().contains("repeat"),
            "an undeclared name is wrong whatever its multiplicity; got {detail}"
        );
    }
}

// A meter declares exactly one fold and a caller cannot choose one, so there
// is no (op, kind) pair left to validate: these tests pin that the declared
// fold is what reaches the plugin, that an unresolvable type fails closed, and
// the unrelated `MAX_AGGREGATION_BUCKETS` cap.
mod aggregate_declared_fold_tests {
    use toolkit_gts::gts_id;
    use toolkit_odata::ODataQuery;
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{
        AggregationBucket, AggregationFold, AggregationResult, MAX_AGGREGATION_BUCKETS,
        MeterTypeId, UsageCollectorError, ValidationReason,
    };

    use crate::domain::test_support::{
        RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_not_found, fake_declaration_source_with_fold,
        fake_declaration_source_with_fold_by_id, recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");
    // Shares the `cf.mini_chat._.tokens_` name prefix with `GTS_ID` above,
    // but names a distinct meter.
    const GTS_ID_B: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_produced.v1~");

    fn meter_id() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn meter_id_b() -> MeterTypeId {
        MeterTypeId::new(GTS_ID_B).expect("valid gts_type_id")
    }

    /// A `Service` + [`RecordingPlugin`] spy over `source`, wired against the
    /// fixed-tenant PDP fake the aggregate surface requires.
    fn service_with_recording_plugin(
        source: std::sync::Arc<dyn crate::domain::ports::declarations::DeclarationSource>,
    ) -> (
        std::sync::Arc<crate::domain::Service>,
        std::sync::Arc<RecordingPlugin>,
    ) {
        let plugin = RecordingPlugin::new();
        let service = ServiceFixture::default()
            .with_source(source)
            .with_resolver(recording_plugin_resolver())
            .build(
                std::sync::Arc::clone(&plugin)
                    as std::sync::Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
                RECORDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    #[tokio::test]
    async fn aggregate_serves_the_fold_the_declaration_names() {
        // The request carries no aggregation parameter. Whatever fold reaches
        // the plugin must have come from the resolved declaration.
        let source = fake_declaration_source_with_fold("MAX");
        let (svc, spy) = service_with_recording_plugin(source);
        spy.set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });

        svc.query_aggregated_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &ODataQuery::default(),
            &[],
            &[],
        )
        .await
        .expect("aggregates");

        assert_eq!(spy.last_fold(), Some(AggregationFold::Max));
    }

    #[tokio::test]
    async fn aggregate_serves_every_declared_fold_not_a_hard_coded_one() {
        // Covers more than one fold value so a service hard-coded to always
        // push `AggregationFold::Sum` cannot pass: every fold a declaration
        // can name must reach the plugin unchanged, not just one of them.
        for fold in [
            AggregationFold::Sum,
            AggregationFold::Count,
            AggregationFold::Max,
            AggregationFold::Min,
            AggregationFold::Latest,
        ] {
            let source = fake_declaration_source_with_fold(fold.as_str());
            let (svc, spy) = service_with_recording_plugin(source);
            spy.set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });

            svc.query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
                &[],
            )
            .await
            .unwrap_or_else(|e| panic!("fold {fold:?} MUST dispatch, got {e:?}"));

            assert_eq!(
                spy.last_fold(),
                Some(fold),
                "the fold reaching the plugin must be the one the declaration names",
            );
        }
    }

    /// Two meters sharing a name prefix but declaring different folds each
    /// resolve to their own. `fake_declaration_source_with_fold_by_id` is the
    /// one double here able to answer two ids differently; every other fixture
    /// answers the same fold whatever id it is asked for.
    #[tokio::test]
    async fn two_meters_sharing_a_name_prefix_with_different_folds_each_resolve_to_their_own() {
        let source =
            fake_declaration_source_with_fold_by_id(&[(meter_id(), "SUM"), (meter_id_b(), "MAX")]);
        let (svc, spy) = service_with_recording_plugin(source);
        spy.set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });

        svc.query_aggregated_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &ODataQuery::default(),
            &[],
            &[],
        )
        .await
        .expect("the SUM meter aggregates");
        assert_eq!(
            spy.last_fold(),
            Some(AggregationFold::Sum),
            "meter_id() must resolve to its OWN declared fold, SUM",
        );

        svc.query_aggregated_usage_records(
            &ctx(),
            meter_id_b(),
            test_time_range(),
            &ODataQuery::default(),
            &[],
            &[],
        )
        .await
        .expect("the MAX meter aggregates");
        assert_eq!(
            spy.last_fold(),
            Some(AggregationFold::Max),
            "meter_id_b() must resolve to its OWN declared fold, MAX, despite \
             sharing meter_id()'s name prefix",
        );
    }

    #[tokio::test]
    async fn aggregate_fails_closed_when_the_type_does_not_resolve() {
        let source = fake_declaration_source_not_found();
        let (svc, spy) = service_with_recording_plugin(source);

        let err = svc
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
                &[],
            )
            .await
            .expect_err("an unresolvable type must not reach the plugin");

        assert!(
            matches!(err, UsageCollectorError::NotFound { .. }),
            "expected NotFound, got {err:?}",
        );
        assert_eq!(
            spy.calls(),
            0,
            "the plugin must not be dispatched for an unresolvable type",
        );
    }

    // Build an `AggregationResult` with exactly `n` empty-key buckets. Mirrors
    // what the plugin's `LIMIT MAX_AGGREGATION_BUCKETS + 1` produces on an
    // over-cardinality `group_by`.
    fn result_with_buckets(n: usize) -> AggregationResult {
        AggregationResult {
            buckets: (0..n)
                .map(|_| AggregationBucket {
                    key: Vec::new(),
                    value: None,
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn over_cap_bucket_count_is_rejected() {
        // One bucket over the cap — exactly what the plugin's `LIMIT cap + 1`
        // yields on overflow — must lift to a client-fixable 400, not a page.
        let source = fake_declaration_source_with_fold("COUNT");
        let (svc, spy) = service_with_recording_plugin(source);
        spy.set_query_aggregated_usage_records_response(result_with_buckets(
            MAX_AGGREGATION_BUCKETS + 1,
        ));

        match svc
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
                &[],
            )
            .await
        {
            Err(UsageCollectorError::InvalidArgument { reason, .. }) => {
                assert_eq!(reason, ValidationReason::AggregationResultTooLarge);
            }
            other => panic!("expected AGGREGATION_RESULT_TOO_LARGE 400, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn at_cap_bucket_count_is_allowed() {
        // Exactly at the cap is the boundary — `>` must not reject it.
        let source = fake_declaration_source_with_fold("COUNT");
        let (svc, spy) = service_with_recording_plugin(source);
        spy.set_query_aggregated_usage_records_response(result_with_buckets(
            MAX_AGGREGATION_BUCKETS,
        ));

        let result = svc
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
                &[],
            )
            .await
            .expect("a result exactly at the cap must be returned");
        assert_eq!(result.buckets.len(), MAX_AGGREGATION_BUCKETS);
    }
}

/// Whether a meter-narrowed PDP grant can still run the aggregate read.
///
/// `authorize_list_usage_records` serves both `Service::list_usage_records`
/// and `Service::query_aggregated_usage_records` under `actions::LIST`, so
/// `POST /records/aggregate` sits in the same blast radius as the raw list.
/// `authz_tests` pins the gate-level denial; these pin the route. The
/// round-trip goes through `MeterNarrowingPermitResolver` so it passes
/// `compile_constraint`, where the permit->deny move happens; a hand-built
/// `AccessScope` would skip that step and prove nothing.
///
/// There is no silent widening to catch: this SPI carries the authorized
/// scope through no channel but the composed `$filter`, and
/// `scope_to_odata_filter` denies the whole projection the moment any one
/// constraint names `gts_type_id`. The SDK compiler's drop-and-continue
/// fail-open on a capability-gated operator is the one carve-out — see
/// [`crate::domain::test_support::UncompilableSiblingPermitResolver`] — and it
/// stays theoretical for every shape driven below.
mod meter_narrowed_grant_aggregate_read_tests {
    use std::sync::Arc;

    use authz_resolver_sdk::AuthZResolverApi;
    use toolkit_gts::gts_id;
    use toolkit_odata::ODataQuery;
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{
        AggregationResult, MeterTypeId, UsageCollectorError, UsageCollectorPluginV1,
    };

    use crate::domain::authz::usage_record;
    use crate::domain::test_support::{
        HappyPathPlugin, MeterNarrowingPermitResolver, ServiceFixture, authenticated_ctx,
        fake_declaration_source_with_fold, test_time_range,
    };

    /// The meter the PDP grant is narrowed to, and the one the request
    /// queries. Whether the two match is irrelevant to the outcome:
    /// `authorize_list_usage_records` runs — and denies, when it denies —
    /// before `TypeResolver::resolve` ever reads `gts_type_id`.
    const OTHER_METER: &str =
        gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn meter() -> MeterTypeId {
        MeterTypeId::new(OTHER_METER).expect("valid gts_type_id")
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// Mirrors `authz_tests::assert_denied_as_reserved_meter`, rewritten here
    /// rather than reused: that module is private to `domain::authz`, and its
    /// helper is typed for `DomainError::AuthorizationDenied` while this gate
    /// one layer up returns `UsageCollectorError::PermissionDenied`.
    fn assert_aggregate_denied_as_reserved_meter(err: &UsageCollectorError, label: &str) {
        let UsageCollectorError::PermissionDenied { detail } = err else {
            panic!("{label}: expected a PermissionDenied carrying a PDP reason, got {err:?}");
        };
        assert!(
            detail.contains("reserved"),
            "{label}: the aggregate denial must name the property as reserved \
             on the scope projection; got {detail:?}"
        );
        assert!(
            detail.contains(usage_record::PROP_GTS_TYPE_ID),
            "{label}: the aggregate denial must name the offending property; \
             got {detail:?}"
        );
        assert!(
            !detail.contains("unknown property"),
            "{label}: the aggregate denial must be distinguishable from the \
             unknown-property denial - an operator reading the log has to \
             tell a deliberate reservation from a policy naming an attribute \
             this gear never had; got {detail:?}"
        );
    }

    /// A meter-narrowed grant cannot run an aggregate read at all: both
    /// denied, by name.
    ///
    /// Both constructors are driven because they could in principle answer
    /// differently — `sole` against a projection that denies outright,
    /// `with_tenant_only_sibling` against one that could instead drop the
    /// meter half and serve the sibling alone. If either ever starts serving,
    /// this fails loud.
    #[tokio::test]
    async fn a_meter_narrowed_grant_on_the_aggregate_read() {
        for (label, resolver) in [
            ("sole", MeterNarrowingPermitResolver::sole(OTHER_METER)),
            (
                "with_tenant_only_sibling",
                MeterNarrowingPermitResolver::with_tenant_only_sibling(OTHER_METER),
            ),
        ] {
            let plugin = HappyPathPlugin::new();
            // Programmed so a regression that lets the denial through fails as
            // `expect_err` on an `Ok` page, not as "not programmed".
            plugin
                .set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });
            let suffix = format!("test.g17.aggregate_read.{label}.v1");
            let svc = ServiceFixture::default()
                .with_source(fake_declaration_source_with_fold("SUM"))
                .with_resolver(resolver as Arc<dyn AuthZResolverApi>)
                .build(
                    Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                    &suffix,
                );

            let err = svc
                .query_aggregated_usage_records(
                    &ctx(),
                    meter(),
                    test_time_range(),
                    &ODataQuery::new(),
                    &[],
                    &[],
                )
                .await
                .expect_err(
                    "a meter-narrowed grant must not serve the aggregate read \
                     wider than it was granted",
                );
            assert_aggregate_denied_as_reserved_meter(&err, label);

            assert!(
                plugin.last_aggregate_query().is_none(),
                "{label}: the plugin must not be dispatched when the PDP \
                 denies - got a recorded aggregate query"
            );
        }
    }
}

// ── The aggregate read dispatches the COMPOSED filter ───────────────────────
//
// `query_aggregated_usage_records`'s SPI signature carries the authorized
// scope through no channel but the `$filter` it hands the plugin: unlike
// `get_usage_record`, `read_feed_page` and `get_reconciliation_metadata`, it
// takes no `scope: &ast::Expr`. So on this read path "the composed filter
// reached the plugin" *is* tenant isolation, and nothing else observes it.
//
// `query::compose_query_with_scope` is unit-tested in `private_helpers_tests`,
// but that pins the function, not the dispatch: nothing there fails if the
// service composes a query and then hands the plugin the caller's own. These
// two tests are what red on that cross-tenant read amplification.
mod aggregate_composed_filter_dispatch_tests {
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_odata::ODataQuery;
    use toolkit_odata::ast::{CompareOperator, Expr, Value};
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{AggregationResult, MeterTypeId, UsageCollectorPluginV1};
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_with_metadata, recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    /// The tenant [`recording_plugin_resolver`]'s fixed `OWNER_TENANT_ID`
    /// constraint names. `authz::scope_to_odata_filter` projects that PEP
    /// property to the `tenant_id` wire field as a UUID value, so this is the
    /// literal the composed `$filter` has to carry.
    fn scope_tenant() -> Uuid {
        Uuid::from_u128(2)
    }

    fn meter_id() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// A `Service` plus its [`RecordingPlugin`] spy, over a declaration
    /// declaring no metadata keys (nothing here filters on one) and the
    /// fixed-tenant PDP fake the aggregate path requires.
    fn svc_and_spy() -> (Arc<Service>, Arc<RecordingPlugin>) {
        let plugin = RecordingPlugin::new();
        plugin.set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(&[]))
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                RECORDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    /// Every `Eq` comparison of an identifier against a literal anywhere in
    /// `expr`, as `(field, rendered-value)` pairs.
    ///
    /// Walks the whole tree rather than matching the root: composition
    /// AND-merges the scope predicate *under* whatever shape the caller's
    /// filter had, so the scope is a leaf once the caller supplies one of
    /// their own. Matching the root would pass on `Expr::And(..)` alone.
    ///
    /// The value is carried as its `Debug` rendering because
    /// `toolkit_odata::ast::Value` derives no `PartialEq`. That names the
    /// variant as well as the payload, so a UUID-typed `tenant_id` predicate
    /// stays distinguishable from a string-typed one carrying the same digits.
    fn eq_leaves(expr: &Expr) -> Vec<(String, String)> {
        match expr {
            Expr::Compare(lhs, CompareOperator::Eq, rhs) => match (lhs.as_ref(), rhs.as_ref()) {
                (Expr::Identifier(field), Expr::Value(value)) => {
                    vec![(field.clone(), format!("{value:?}"))]
                }
                _ => Vec::new(),
            },
            Expr::And(lhs, rhs) | Expr::Or(lhs, rhs) => {
                let mut found = eq_leaves(lhs);
                found.extend(eq_leaves(rhs));
                found
            }
            Expr::Not(inner) => eq_leaves(inner),
            _ => Vec::new(),
        }
    }

    /// One `(field, rendered-value)` pair, built through [`eq_leaves`]' own
    /// rendering so the expectation cannot drift from it.
    fn leaf(field: &str, value: &Value) -> (String, String) {
        (field.to_owned(), format!("{value:?}"))
    }

    /// The dispatched aggregate `$filter`'s `Eq` leaves, or a panic naming
    /// what was missing.
    fn dispatched_eq_leaves(spy: &RecordingPlugin) -> Vec<(String, String)> {
        let dispatched = spy
            .last_aggregate_query()
            .expect("the plugin MUST have been dispatched");
        let filter = dispatched
            .filter()
            .expect(
                "the aggregate dispatch MUST carry a $filter: the composed \
                 one is the only channel this SPI has for the authorized \
                 scope, so a `None` filter is an unscoped cross-tenant read",
            )
            .clone();
        eq_leaves(&filter)
    }

    #[tokio::test]
    async fn an_unfiltered_aggregate_read_still_dispatches_the_scope_predicate() {
        // The caller supplies no `$filter`, so the dispatched one is entirely
        // server-injected: dispatching the caller's query here would mean
        // `filter: None` — every row of every tenant inside the range.
        let (svc, spy) = svc_and_spy();

        svc.query_aggregated_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &ODataQuery::new(),
            &[],
            &[],
        )
        .await
        .expect("an unfiltered aggregate read under a tenant-scoped permit is a complete request");

        assert_eq!(
            dispatched_eq_leaves(&spy),
            vec![leaf("tenant_id", &Value::Uuid(scope_tenant()))],
            "the aggregate dispatch MUST carry the PDP scope's own \
             `tenant_id` predicate as its whole $filter when the caller \
             supplied none - anything else is an unscoped aggregate over \
             every tenant's rows",
        );
    }

    #[tokio::test]
    async fn the_aggregate_dispatch_carries_the_scope_predicate_beside_the_callers_own() {
        // The intersection branch: the composed filter must be
        // `caller AND scope`. Both halves are asserted — the caller half rules
        // out a service that *replaced* the caller's filter with the scope.
        let caller = {
            let expr = toolkit_odata::parse_filter_string("resource_id eq 'r1'")
                .expect("test filter parses")
                .into_expr();
            ODataQuery::from(Some(expr))
        };
        assert_eq!(
            eq_leaves(caller.filter().expect("the caller's own filter")),
            vec![leaf("resource_id", &Value::String("r1".to_owned()))],
            "precondition: the caller's filter names no tenant, so an \
             assertion that the dispatched one does can only be satisfied \
             by composition",
        );

        let (svc, spy) = svc_and_spy();
        svc.query_aggregated_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &caller,
            &[],
            &[],
        )
        .await
        .expect("a filtered aggregate read under a tenant-scoped permit is a complete request");

        let leaves = dispatched_eq_leaves(&spy);
        assert!(
            leaves.contains(&leaf("tenant_id", &Value::Uuid(scope_tenant()))),
            "the aggregate dispatch MUST carry the PDP scope's `tenant_id` \
             predicate: this SPI has no `scope` parameter, so a filter \
             without it is a cross-tenant read that still answers Ok; got \
             {leaves:?}",
        );
        assert!(
            leaves.contains(&leaf("resource_id", &Value::String("r1".to_owned()))),
            "and MUST still carry the caller's own predicate: composition \
             is an intersection, not a substitution; got {leaves:?}",
        );
    }
}

// The query-surface gate is stated in [`crate::domain::query`]; its
// pure-function coverage lives in `query_tests.rs`. This module proves the
// checks are wired into `Service::list_usage_records` /
// `Service::query_aggregated_usage_records` — right position (before
// dispatch), right arguments (the resolved declaration's `declared_keys`).
mod query_admissibility_tests {
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_odata::{ODataQuery, ast};
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{
        AggregationDimension, AggregationResult, MetadataFilter, MetadataKey, MeterTypeId,
        UsageCollectorError, UsageCollectorPluginV1, ValidationReason,
    };

    use crate::domain::Service;
    use crate::domain::ports::declarations::DeclarationSource;
    use crate::domain::test_support::{
        RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_with_metadata, recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn meter_id() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// An `ODataQuery` whose `$filter` is a bare `reserved_field eq 'x'`
    /// predicate, so whatever the request is rejected for is the
    /// admissibility gate under test. The mandatory read range travels beside
    /// the filter as a typed parameter and contributes no conjunct.
    fn filter_naming(reserved_field: &str) -> ODataQuery {
        ODataQuery::from(Some(ast::Expr::Compare(
            Box::new(ast::Expr::Identifier(reserved_field.to_owned())),
            ast::CompareOperator::Eq,
            Box::new(ast::Expr::Value(ast::Value::String("x".to_owned()))),
        )))
    }

    /// An `ODataQuery` whose `$filter` is `field eq '<value>'`, with the
    /// literal under the caller's control — `filter_naming` fixes it at
    /// `"x"`, which cannot express an off-label closed-set literal.
    fn filter_eq(field: &str, value: &str) -> ODataQuery {
        ODataQuery::from(Some(ast::Expr::Compare(
            Box::new(ast::Expr::Identifier(field.to_owned())),
            ast::CompareOperator::Eq,
            Box::new(ast::Expr::Value(ast::Value::String(value.to_owned()))),
        )))
    }

    /// A `Service` + [`RecordingPlugin`] spy over `source`, wired against the
    /// fixed-tenant PDP fake both read paths require.
    fn service_with_recording_plugin(
        source: Arc<dyn DeclarationSource>,
    ) -> (Arc<Service>, Arc<RecordingPlugin>) {
        let plugin = RecordingPlugin::new();
        let service = ServiceFixture::default()
            .with_source(source)
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                RECORDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    /// Assert `err` is the canonical reserved-filter-field rejection.
    fn assert_reserved_field_rejection(err: &UsageCollectorError) {
        match err {
            UsageCollectorError::InvalidArgument { field, reason, .. } => {
                assert_eq!(field, "$filter", "attributes to $filter");
                assert_eq!(*reason, ValidationReason::Validation);
            }
            other => panic!("expected InvalidArgument/Validation, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn aggregate_rejects_a_filter_naming_a_window_bound() {
        let (svc, spy) = service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));

        let err = svc
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &filter_naming("window_start"),
                &[],
                &[],
            )
            .await
            .expect_err("a $filter naming window_start must be rejected");
        assert_reserved_field_rejection(&err);
        assert_eq!(
            spy.calls(),
            0,
            "a rejected filter must never reach the plugin",
        );
    }

    #[tokio::test]
    async fn aggregate_rejects_an_undeclared_group_by_dimension() {
        let (svc, spy) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&["region"]));

        let dims = [AggregationDimension::Metadata(
            MetadataKey::new("tier").expect("valid key"),
        )];
        let err = svc
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
                &dims,
            )
            .await
            .expect_err("an undeclared group_by dimension must be rejected");
        assert!(
            err.to_string().contains("tier"),
            "the rejection must name the offending key: {err}",
        );
        assert_eq!(
            spy.calls(),
            0,
            "a rejected group_by must never reach the plugin",
        );
    }

    #[tokio::test]
    async fn aggregate_accepts_a_declared_group_by_dimension() {
        let (svc, spy) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&["region"]));
        spy.set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });

        let dims = [AggregationDimension::Metadata(
            MetadataKey::new("region").expect("valid key"),
        )];
        svc.query_aggregated_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &ODataQuery::default(),
            &[],
            &dims,
        )
        .await
        .expect("a declared group_by dimension must be accepted");
        assert_eq!(spy.calls(), 1, "an accepted group_by must reach the plugin");
    }

    #[tokio::test]
    async fn aggregate_group_by_admissibility_is_recomputed_per_request() {
        // Two independently-resolved declarations for the same meter id, one
        // declaring `region` and one not: the SAME dimension is rejected
        // against the first and accepted against the second, so the admissible
        // set comes from the declaration resolved for *this* request.
        let dims = [AggregationDimension::Metadata(
            MetadataKey::new("region").expect("valid key"),
        )];

        let (svc_before, _spy_before) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));
        let err = svc_before
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
                &dims,
            )
            .await
            .expect_err("not yet declared: must be rejected");
        assert!(err.to_string().contains("region"));

        let (svc_after, spy_after) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&["region"]));
        spy_after
            .set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });
        svc_after
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
                &dims,
            )
            .await
            .expect("declared a moment later: must now be admissible, without a restart");
    }
    // ── metadata_filter admissibility ─────────────────────────────────────
    //
    // `metadata_filter` is the dynamic-key side channel that exists because
    // `toolkit-odata`'s grammar cannot express a filter over a JSON map key.
    // It never flows through `$filter`, so it needs its own declared-keys
    // gate on both read paths.

    /// Build a `MetadataFilter` for `key` with a single candidate value.
    fn metadata_filter(key: &str) -> MetadataFilter {
        MetadataFilter::new(key, ["x".to_owned()]).expect("valid metadata filter")
    }

    #[tokio::test]
    async fn aggregate_rejects_an_undeclared_metadata_filter_key() {
        let (svc, spy) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&["region"]));

        let filters = [metadata_filter("tier")];
        let err = svc
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &filters,
                &[],
            )
            .await
            .expect_err("an undeclared metadata_filter key must be rejected");
        assert!(
            err.to_string().contains("tier"),
            "the rejection must name the offending key: {err}",
        );
        assert_eq!(
            spy.calls(),
            0,
            "a rejected metadata_filter must never reach the plugin",
        );
    }

    #[tokio::test]
    async fn aggregate_accepts_a_declared_metadata_filter_key() {
        let (svc, spy) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&["region"]));
        spy.set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });

        let filters = [metadata_filter("region")];
        svc.query_aggregated_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &ODataQuery::default(),
            &filters,
            &[],
        )
        .await
        .expect("a declared metadata_filter key must be accepted");
        assert_eq!(
            spy.calls(),
            1,
            "an accepted metadata_filter must reach the plugin",
        );
    }
    /// The `metadata_filter` count cap holds on the aggregate path too. That
    /// path has no cursor to blow, so the reason here is the published
    /// `maxItems: 16` on the wire contract rather than a pagination bound.
    #[tokio::test]
    async fn the_metadata_filter_caps_hold_on_the_aggregate_path() {
        let (svc, spy) = service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));
        let filters: Vec<MetadataFilter> = (0..17)
            .map(|i| MetadataFilter::new(format!("k{i}"), ["v"]).expect("valid"))
            .collect();
        let err = svc
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::new(),
                &filters,
                &[],
            )
            .await
            .expect_err("17 predicates exceeds the cap on every surface");
        // None of `k0..k16` is declared either, so `InvalidArgument` alone
        // would also match `unknown_metadata_key`; the reason and detail are
        // pinned so disabling the cap check alone turns this red.
        let UsageCollectorError::InvalidArgument {
            ref field,
            ref reason,
            ref detail,
            ..
        } = err
        else {
            panic!("expected InvalidArgument, got {err:?}");
        };
        assert_eq!(field, "metadata", "the cap rejection blames the bag");
        // `reason` is the structural discriminator (it rides onto
        // `field_violations[0].reason` in `sdk_error_mapping.rs`):
        // `too_many_metadata_filters` carries `Validation`, while
        // `unknown_metadata_key` carries `UnknownMetadataKey`.
        assert_eq!(
            *reason,
            ValidationReason::Validation,
            "the rejection MUST be the count cap, not unknown_metadata_key \
             (ValidationReason::UnknownMetadataKey), which shares the same \
             `metadata` field; got {reason:?}"
        );
        // `reason` cannot carry the offending count; `detail` pins that.
        assert!(
            detail.contains("17") && detail.contains("16"),
            "the rejection detail MUST name the count cap (17 over a cap of 16): {detail}"
        );
        assert_eq!(
            spy.calls(),
            0,
            "the cap is checked before dispatch, so the plugin saw nothing"
        );
    }
    // ── Closed-label `$filter` literals ───────────────────────────────────
    //
    // `entry_type` and `origin` are closed-label columns: an off-label
    // `entry_type` literal raises PostgreSQL 22P02 at the backend (a 500 on
    // caller input), and an off-label `origin` literal silently answers an
    // empty page. `query::reject_off_label_literals` refuses both before
    // dispatch, on both read paths. The property under test here is "before
    // dispatch", which only the plugin-double call count can show; the
    // function's own return value is pinned in `query_tests.rs`.

    #[tokio::test]
    async fn aggregate_rejects_an_off_label_origin_literal_before_dispatch() {
        let (svc, spy) = service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));

        let err = svc
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &filter_eq("origin", "imported"),
                &[],
                &[],
            )
            .await
            .expect_err("`imported` is not a label");
        let UsageCollectorError::InvalidArgument { ref detail, .. } = err else {
            panic!("expected InvalidArgument, got {err:?}");
        };
        assert!(
            detail.contains("imported"),
            "the refusal must echo the offending value; got {detail}"
        );
        assert_eq!(
            spy.calls(),
            0,
            "an off-label literal must never reach the plugin"
        );
    }
}

// ── Withdrawal exclusion from the fold ─────────────────────────────────────
//
// The gear folds nothing: `query_aggregated_usage_records` dispatches to the
// plugin and returns what it computes, so these tests bind no real storage
// plugin. They hold the reference semantics
// ([`usage_collector_sdk::UsageCollectorPluginV1::query_aggregated_usage_records`])
// against the in-memory `FoldingPlugin`, and catch a gear-side regression in
// what the plugin is handed: a dropped `time_range`, or a fold other than the
// declared one, moves these numbers.
mod withdrawal_exclusion_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use bigdecimal::BigDecimal;
    use time::OffsetDateTime;
    use toolkit_gts::gts_id;
    use toolkit_odata::ODataQuery;
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{
        AggregationFold, AggregationResult, EntryType, IdempotencyKey, MeterTypeId, RecordOrigin,
        ResourceRef, UsageCollectorPluginV1, UsageRecord, derive_usage_record_id,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        FOLDING_PLUGIN_SUFFIX, FoldingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_with_fold, recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    /// The tenant every entry here is attributed to — the one
    /// [`recording_plugin_resolver`] returns as its fixed `OWNER_TENANT_ID`
    /// constraint, so the composed scope names the fixture's own rows.
    fn tenant_id() -> Uuid {
        Uuid::from_u128(2)
    }

    fn meter_id() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// A persisted measurement inside [`test_time_range`], carrying `quantity`
    /// and ending `minutes` after the epoch. Built through
    /// [`derive_usage_record_id`] rather than a random `id`, so the identity
    /// is the one an invalidation's reference has to name.
    fn measurement(idem: &str, value: &str, minutes: i64) -> UsageRecord {
        let window_start = OffsetDateTime::UNIX_EPOCH;
        let window_end = OffsetDateTime::UNIX_EPOCH + time::Duration::minutes(minutes);
        let idempotency_key = IdempotencyKey::new(idem).expect("valid idempotency key");
        UsageRecord {
            id: derive_usage_record_id(
                tenant_id(),
                &meter_id(),
                &idempotency_key,
                window_start,
                window_end,
                EntryType::Record,
            ),
            gts_type_id: meter_id(),
            tenant_id: tenant_id(),
            resource_ref: ResourceRef::new("rsc-fold", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: value.parse().expect("valid decimal quantity"),
            idempotency_key,
            accepted_at: window_end,
            origin: RecordOrigin::Live,
            invalidation: None,
            window_start,
            window_end,
        }
    }

    /// A `Service` over a fresh [`FoldingPlugin`], serving `fold` as the
    /// declared one.
    fn service_over_folding_plugin(fold: AggregationFold) -> (Arc<Service>, Arc<FoldingPlugin>) {
        let plugin = FoldingPlugin::new();
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold(fold.as_str()))
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                FOLDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    /// The one bucket a no-grouping aggregate answers with.
    fn single_bucket(result: &AggregationResult) -> Option<BigDecimal> {
        match result.buckets.as_slice() {
            [bucket] => {
                assert!(bucket.key.is_empty(), "no grouping was requested");
                bucket.value.clone()
            }
            other => panic!("a no-grouping aggregate answers with one bucket, got {other:?}"),
        }
    }

    fn big(literal: &str) -> BigDecimal {
        literal.parse().expect("valid decimal literal")
    }

    async fn aggregate(service: &Service) -> AggregationResult {
        service
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
                &[],
            )
            .await
            .expect("the aggregate must reach the plugin and fold")
    }

    /// Also the sole coverage, crate-wide, for two ledger-path claims this
    /// module otherwise never touches: that both halves of a withdrawn pair
    /// stay readable via `get_usage_record`, and that `list_usage_records`
    /// returns every entry as persisted, withdrawn or not. Widen either
    /// assertion with care — an edit here moves three read paths at once.
    #[tokio::test]
    async fn a_withdrawn_pair_folds_to_nothing_while_both_stay_readable() {
        // A fold that admitted the echoed quantity would double-count the
        // very measurement the withdrawal removes. Both entries carry one
        // covered period, so no range selects one of the pair without the
        // other.
        let (svc, plugin) = service_over_folding_plugin(AggregationFold::Sum);
        let withdrawn = measurement("idem-withdrawn", "42.5", 30);
        plugin.store(measurement("idem-survivor", "7.5", 10));
        let invalidation = plugin.store_withdrawn(withdrawn.clone());

        assert_eq!(
            single_bucket(&aggregate(&svc).await),
            Some(big("7.5")),
            "only the entry nothing withdrew may reach the fold",
        );

        // …and the ledger paths return every entry as persisted: leaving a
        // withdrawn pair out of a locally computed fold is the reader's
        // obligation, and the invalidation's reference is what they read.
        let page = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
            )
            .await
            .expect("the raw path must reach the plugin");
        assert_eq!(
            page.items.len(),
            3,
            "the raw path returns every entry as persisted, withdrawn or not",
        );

        for id in [withdrawn.id, invalidation.id] {
            let row = svc
                .get_usage_record(&ctx(), id)
                .await
                .expect("both entries of a withdrawn pair stay readable by id");
            assert_eq!(row.id, id);
        }
        assert_eq!(
            invalidation.invalidation.as_ref().map(|inv| inv.target),
            Some(withdrawn.id),
            "the invalidation names the entry it withdrew, which is what a \
             consumer folding on its own side reads",
        );
    }

    #[tokio::test]
    async fn excluding_only_the_target_double_counts() {
        // A plugin that dropped the withdrawn record but kept the echoed
        // invalidation reports the measurement it was told to remove. Over
        // the pair alone the wrong answer under `SUM` is 42.5 and the right
        // one is an empty selection, so this fails loudly.
        let (svc, plugin) = service_over_folding_plugin(AggregationFold::Sum);
        plugin.store_withdrawn(measurement("idem-only-pair", "42.5", 30));

        assert_eq!(
            single_bucket(&aggregate(&svc).await),
            None,
            "a withdrawn pair leaves nothing to fold; 42.5 would be the \
             echoed quantity counted once more",
        );
    }

    #[tokio::test]
    async fn an_orphan_invalidation_still_contributes_nothing() {
        // An invalidation contributes nothing to a fold whether or not its
        // target is in the selection. Retention is plugin-owned, so a
        // conforming deployment can purge a target and keep the entry that
        // withdrew it — a reachable state, not a malformed ledger. It is also
        // the one input shape that tells the two half-rules apart: over a
        // conforming pair the invalidation is a faithful copy, so "kept the
        // target" and "kept the invalidation" fold to the same number.
        let (svc, plugin) = service_over_folding_plugin(AggregationFold::Sum);
        plugin.store(measurement("idem-survivor", "7.5", 10));
        plugin.store(FoldingPlugin::withdrawal_of(
            &measurement("idem-purged", "42.5", 30),
            RecordOrigin::Live,
        ));

        assert_eq!(
            single_bucket(&aggregate(&svc).await),
            Some(big("7.5")),
            "an orphan invalidation contributes nothing; 42.5 would be the \
             echoed quantity admitted with its target already gone",
        );
    }

    #[tokio::test]
    async fn every_declared_fold_excludes_the_pair() {
        // Withdrawal is the primitive precisely because its meaning does not
        // depend on what a quantity means, so every declared fold must agree.
        //
        // Two pairs are withdrawn, their quantities straddling the survivors'
        // — 99 above and 1 below. With a single withdrawn quantity, either
        // `MAX` or `MIN` would answer the same whether the pair leaked in or
        // not. Both pairs also end later than either survivor, so `LATEST`
        // moves too: admitting them moves every fold.
        //
        // The answers are collected before a single assertion, so a defect
        // reports every fold it moved rather than stopping at the first.
        let mut answers = Vec::new();
        for fold in [
            AggregationFold::Sum,
            AggregationFold::Count,
            AggregationFold::Max,
            AggregationFold::Min,
            AggregationFold::Latest,
        ] {
            let (svc, plugin) = service_over_folding_plugin(fold);
            plugin.store(measurement("idem-low", "7.5", 10));
            plugin.store(measurement("idem-high", "20", 20));
            plugin.store_withdrawn(measurement("idem-big", "99", 30));
            plugin.store_withdrawn(measurement("idem-small", "1", 40));

            answers.push((fold, single_bucket(&aggregate(&svc).await)));
        }

        assert_eq!(
            answers,
            vec![
                (AggregationFold::Sum, Some(big("27.5"))),
                (AggregationFold::Count, Some(big("2"))),
                (AggregationFold::Max, Some(big("20"))),
                (AggregationFold::Min, Some(big("7.5"))),
                (AggregationFold::Latest, Some(big("20"))),
            ],
            "every declared fold must see the two survivors alone",
        );
    }

    // ── The declared LATEST tie-break ──────────────────────────────────────
    //
    // Not withdrawal exclusion; here because this is where the `FoldingPlugin`
    // fixtures live. `fold_over`'s `Latest` arm orders on greatest
    // `window_end`, then `accepted_at`, then `id`, and every case above ties
    // on none of them, so the tie-breaks reached no oracle in this crate. The
    // SDK's `latest-tie-break` check covers a real storage plugin, not this
    // double.
    //
    // Two tests rather than one combined case: a double that got `accepted_at`
    // and `id` wrong in compensating directions would pass a single assertion
    // over a single population.

    /// A pair tying on `window_end` is decided by the greater `accepted_at`,
    /// with the `id` order made to disagree.
    #[tokio::test]
    async fn latest_breaks_a_window_end_tie_on_accepted_at() {
        let (svc, plugin) = service_over_folding_plugin(AggregationFold::Latest);

        // Both entries end the same minute, so `window_end` decides nothing.
        // Which derived id is the greater is a digest's business, so the
        // fixture reads it off and gives the *later* `accepted_at` to the
        // **lesser** id — putting `accepted_at` and `id` in opposition, so a
        // fold that fell straight through to `id` answers the other entry.
        let one = measurement("idem-tie-accepted-one", "0", 30);
        let two = measurement("idem-tie-accepted-two", "0", 30);
        assert_ne!(one.id, two.id, "two idempotency keys derive two ids");
        assert_eq!(
            one.window_end, two.window_end,
            "the pair must tie on window_end or the fold never reaches accepted_at"
        );
        let (lesser, greater) = if one.id < two.id {
            (one, two)
        } else {
            (two, one)
        };

        let lesser_end = lesser.window_end;
        plugin.store(UsageRecord {
            quantity: "11".parse().expect("valid decimal quantity"),
            accepted_at: lesser_end + time::Duration::minutes(5),
            ..lesser
        });
        let greater_end = greater.window_end;
        plugin.store(UsageRecord {
            quantity: "22".parse().expect("valid decimal quantity"),
            accepted_at: greater_end,
            ..greater
        });

        assert_eq!(
            single_bucket(&aggregate(&svc).await),
            Some(big("11")),
            "LATEST takes the greater `accepted_at` over the greater `id`; 22 \
             is what a fold falling from `window_end` straight to `id` answers",
        );
    }

    /// A pair tying on `window_end` **and** `accepted_at` is decided by the
    /// greater `id`, with insertion order made to disagree.
    #[tokio::test]
    async fn latest_breaks_an_accepted_at_tie_on_the_greater_id() {
        let (svc, plugin) = service_over_folding_plugin(AggregationFold::Latest);

        // `measurement` stamps `accepted_at` from `window_end`, so a pair
        // sharing a covered period leaves `id` the only key left to decide
        // with. The greater-`id` entry is stored **first**, against the order
        // a fold that ran out of keys would answer in: `max_by_key` keeps the
        // last of several equal maxima, so dropping `id` picks the second.
        let one = measurement("idem-tie-id-one", "0", 30);
        let two = measurement("idem-tie-id-two", "0", 30);
        assert_ne!(one.id, two.id, "two idempotency keys derive two ids");
        assert_eq!(
            one.window_end, two.window_end,
            "the pair must tie on window_end"
        );
        assert_eq!(
            one.accepted_at, two.accepted_at,
            "the pair must tie on accepted_at too, or `id` never decides"
        );
        let (lesser, greater) = if one.id < two.id {
            (one, two)
        } else {
            (two, one)
        };

        plugin.store(UsageRecord {
            quantity: "33".parse().expect("valid decimal quantity"),
            ..greater
        });
        plugin.store(UsageRecord {
            quantity: "44".parse().expect("valid decimal quantity"),
            ..lesser
        });

        assert_eq!(
            single_bucket(&aggregate(&svc).await),
            Some(big("33")),
            "LATEST takes the greater `id` once both keys above it tie; 44 is \
             what a fold that ran out of keys and fell to insertion order answers",
        );
    }
}
