//! Unit tests for [`super`] — the reconciliation read's pure half.

use std::num::NonZeroU64;

use bigdecimal::BigDecimal;
use usage_collector_sdk::v2::{
    AggregationFold, ObservedQuantity, QuantitySummary, UsageCollectorError, UsageQuantity,
};

use super::{
    GRANULARITY_TENANT_GTS_TYPE, RESERVED_GRANULARITIES, check_summary_branch, parse_granularity,
};

#[test]
fn the_served_granularity_is_admitted() {
    parse_granularity(GRANULARITY_TENANT_GTS_TYPE)
        .expect("v1 admits the (tenant, GTS type) granularity");
}

#[test]
fn each_reserved_granularity_is_refused_by_name_and_says_it_is_not_served() {
    // `dod-reconciliation-caller-scopes-reserved` and `inst-radmit-reserved`
    // both require an error STATING the granularity is not served. Leaving
    // this to the yaml's single-value enum gives a bare deserialization
    // failure that names nothing, which is why the two values are parsed
    // rather than rejected as unknown.
    for reserved in RESERVED_GRANULARITIES {
        let err =
            parse_granularity(reserved).expect_err("a reserved granularity is not served in v1");
        let rendered = format!("{err}");
        assert!(
            rendered.contains(reserved),
            "the rejection for `{reserved}` must name it; got: {rendered}"
        );
        assert!(
            rendered.contains("not served"),
            "the rejection for `{reserved}` must say it is not served rather than that it \
             is unknown — the granularity is reserved, and a caller told `unknown` will \
             think they misspelled it; got: {rendered}"
        );
    }
}

#[test]
fn an_unknown_granularity_is_refused_without_claiming_it_is_reserved() {
    // The two rejections share one error variant, so this asserts the DETAIL
    // (global constraints): a variant assertion cannot tell this guard from
    // the reserved-granularity guard above.
    let err = parse_granularity("per_resource").expect_err("only three spellings are known");
    let rendered = format!("{err}");
    assert!(rendered.contains("per_resource"), "got: {rendered}");
    assert!(
        !rendered.contains("reserved"),
        "an unknown granularity is a caller mistake, not a deferred feature; calling it \
         reserved promises it is coming. got: {rendered}"
    );
}

#[test]
fn the_accrued_branch_is_accepted_under_sum_and_refused_under_every_other_fold() {
    let accrued = QuantitySummary::Accrued(BigDecimal::from(1));
    check_summary_branch(AggregationFold::Sum, &accrued)
        .expect("a SUM meter reports an accrued sum");

    for fold in [
        AggregationFold::Count,
        AggregationFold::Max,
        AggregationFold::Min,
        AggregationFold::Latest,
    ] {
        let err = check_summary_branch(fold, &accrued).expect_err(
            "a non-accruing meter answered with an accrued sum is a host-contract breach",
        );
        let rendered = format!("{err}");
        assert!(
            rendered.contains("Accrued") || rendered.contains("accrued"),
            "the breach must name the branch that was returned; got: {rendered}"
        );
        assert!(
            rendered.contains(&format!("{fold:?}")),
            "the breach must name the declared fold that was expected; got: {rendered}"
        );
    }
}

#[test]
fn the_observation_branch_is_refused_under_sum() {
    let observations = QuantitySummary::Observations(None);
    let err = check_summary_branch(AggregationFold::Sum, &observations)
        .expect_err("a SUM meter answered with an observation count is a host-contract breach");
    let rendered = format!("{err}");
    assert!(rendered.contains("Sum"), "got: {rendered}");
    // Same bar as the sibling test above: `check_summary_branch` is the only
    // thing standing between a plugin returning the wrong branch and a
    // wrong figure reaching a billing surface, so a caller reading this
    // breach needs the returned branch named, not just the expected fold.
    assert!(
        rendered.contains("Observations") || rendered.contains("observation"),
        "the breach must name the branch that was returned; got: {rendered}"
    );
}

#[test]
fn the_observation_branch_is_accepted_under_every_non_sum_fold() {
    // The reject side (above) exercises all five illegal fold/branch
    // combinations; this closes the matching accept side, which the
    // original suite left to inspection alone for every fold but SUM.
    let observations = QuantitySummary::Observations(Some(ObservedQuantity {
        count: NonZeroU64::new(3).unwrap(),
        latest: UsageQuantity::parse("1").unwrap(),
    }));
    for fold in [
        AggregationFold::Count,
        AggregationFold::Max,
        AggregationFold::Min,
        AggregationFold::Latest,
    ] {
        check_summary_branch(fold, &observations)
            .expect("a non-accruing meter reports an observation count and latest");
    }
}

#[test]
fn a_branch_breach_is_internal_rather_than_a_caller_error() {
    // The caller supplied nothing that could cause this: the fold comes from
    // the declaration and the branch from the plugin. Reporting it as a 400
    // would tell an operator to fix their request.
    let err = check_summary_branch(AggregationFold::Sum, &QuantitySummary::Observations(None))
        .expect_err("expected a breach");
    assert!(
        matches!(err, UsageCollectorError::Internal { .. }),
        "got {err:?}"
    );
}

/// `Service::get_reconciliation_metadata` — the Query Gateway's
/// operator-facing read path.
mod get_reconciliation_metadata_tests {
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::v2::{
        AggregationFold, MeterTypeId, TimeRange, UsageCollectorError, UsageCollectorPluginV2,
    };
    use uuid::Uuid;

    use crate::v2::domain::Service;
    use crate::v2::domain::ports::declarations::DeclarationSource;
    use crate::v2::domain::test_support::{
        DenyOneSubjectResolver, RecordingReconciliationPlugin, ServiceFixture,
        fake_declaration_source_not_found, fake_declaration_source_with_fold, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");

    /// Registration suffix this module's `Service` fixtures build under.
    const SUFFIX: &str = "test.usage_collector.reconciliation.plugin.v1";

    /// The subject [`DenyOneSubjectResolver`] permits (with an
    /// `OWNER_TENANT_ID` grant scoped to this subject's own tenant — see
    /// [`service_with`]).
    const PERMIT_SUBJECT: Uuid = Uuid::from_u128(1);
    /// A second permitted subject naming a different home tenant, for
    /// [`the_compiled_pdp_scope_reaches_the_plugin`]: two permitted callers
    /// whose compiled scopes must differ prove the scope the plugin receives
    /// is tied to the caller rather than a shared placeholder.
    const OTHER_PERMIT_SUBJECT: Uuid = Uuid::from_u128(3);
    /// The subject [`DenyOneSubjectResolver`] denies.
    const DENY_SUBJECT: Uuid = Uuid::from_u128(0xDEAD);

    /// [`PERMIT_SUBJECT`]'s home tenant.
    const PERMIT_TENANT: Uuid = Uuid::from_u128(2);
    /// [`OTHER_PERMIT_SUBJECT`]'s home tenant — distinct from
    /// [`PERMIT_TENANT`].
    const OTHER_PERMIT_TENANT: Uuid = Uuid::from_u128(4);

    /// The tenant scope parameter every fixture here passes to
    /// `get_reconciliation_metadata`. Deliberately distinct from either
    /// `SecurityContext`'s own tenant: it is dispatched straight to the plugin
    /// rather than read by the PDP request, so conflating the two in a fixture
    /// would hide them being mixed up in the code.
    const FIXTURE_TENANT: Uuid = Uuid::from_u128(0x7E57);

    fn fixture_meter() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn fixture_range() -> TimeRange {
        test_time_range()
    }

    /// A permitted caller. [`DenyOneSubjectResolver`] permits every subject
    /// but [`DENY_SUBJECT`], scoping the grant to this context's own
    /// [`PERMIT_TENANT`].
    fn permit_ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(PERMIT_SUBJECT)
            .subject_tenant_id(PERMIT_TENANT)
            .subject_type("user")
            .build()
            .expect("authenticated context")
    }

    /// A second permitted caller, naming [`OTHER_PERMIT_TENANT`] rather than
    /// [`permit_ctx`]'s, so the scope the plugin receives can be shown to
    /// track the calling context.
    fn other_permit_ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(OTHER_PERMIT_SUBJECT)
            .subject_tenant_id(OTHER_PERMIT_TENANT)
            .subject_type("user")
            .build()
            .expect("authenticated context")
    }

    /// A denied caller — same shape as [`permit_ctx`], differing only in the
    /// subject [`DenyOneSubjectResolver`] is built to deny.
    fn deny_ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(DENY_SUBJECT)
            .subject_tenant_id(PERMIT_TENANT)
            .subject_type("user")
            .build()
            .expect("authenticated context")
    }

    /// A `DeclarationSource` resolving the queried meter to a declaration
    /// naming `fold`.
    fn declaration_with_fold(fold: AggregationFold) -> Arc<dyn DeclarationSource> {
        fake_declaration_source_with_fold(fold.as_str())
    }

    /// A `DeclarationSource` that never resolves any meter.
    fn no_such_declaration() -> Arc<dyn DeclarationSource> {
        fake_declaration_source_not_found()
    }

    /// A `Service` over `plugin`, resolving every meter through
    /// `declaration`, wired against [`DenyOneSubjectResolver`] so
    /// [`permit_ctx`] and [`deny_ctx`] reach opposite PDP decisions through
    /// one fixture.
    fn service_with(
        plugin: Arc<dyn UsageCollectorPluginV2>,
        declaration: Arc<dyn DeclarationSource>,
    ) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(declaration)
            .with_resolver(DenyOneSubjectResolver::new(DENY_SUBJECT))
            .build(plugin, SUFFIX)
    }

    #[tokio::test]
    async fn a_reconciliation_read_resolves_the_declaration_and_passes_its_fold_to_the_plugin() {
        // The gear resolves the queried type and takes the declared fold,
        // which reaches the plugin as a parameter. A gear that passed a
        // default would report the wrong branch for every gauge meter.
        let plugin = RecordingReconciliationPlugin::answering_for(AggregationFold::Max);
        let service = service_with(plugin.clone(), declaration_with_fold(AggregationFold::Max));
        service
            .get_reconciliation_metadata(
                &permit_ctx(),
                PERMIT_TENANT,
                fixture_meter(),
                fixture_range(),
            )
            .await
            .expect("a permitted read over a resolvable meter must succeed");
        assert_eq!(plugin.last_fold(), Some(AggregationFold::Max));
    }

    #[tokio::test]
    async fn the_compiled_pdp_scope_reaches_the_plugin() {
        // The compiled scope carries the authorization decision and is the
        // argument a double is likeliest to discard. One caller's scope
        // "looking tenant-shaped" rules nothing out, since a fixed
        // placeholder every permit shares would look the same. So two
        // permitted callers name two different tenants
        // (`DenyOneSubjectResolver` compiles each grant from its own caller's
        // subject tenant), and both that each scope names *that* caller's
        // tenant and that the two disagree are required.
        let plugin = RecordingReconciliationPlugin::answering_for(AggregationFold::Sum);
        let service = service_with(plugin.clone(), declaration_with_fold(AggregationFold::Sum));

        service
            .get_reconciliation_metadata(
                &permit_ctx(),
                PERMIT_TENANT,
                fixture_meter(),
                fixture_range(),
            )
            .await
            .expect("a permitted read must succeed");
        let scope_for_permit_ctx = plugin
            .last_scope()
            .expect("the plugin must receive a scope");
        assert!(
            format!("{scope_for_permit_ctx:?}").contains(&PERMIT_TENANT.to_string()),
            "the plugin must receive the PDP-compiled scope naming THIS caller's own tenant \
             ({PERMIT_TENANT}), not a placeholder: {scope_for_permit_ctx:?}"
        );

        service
            .get_reconciliation_metadata(
                &other_permit_ctx(),
                OTHER_PERMIT_TENANT,
                fixture_meter(),
                fixture_range(),
            )
            .await
            .expect("a second permitted read, under a different tenant, must also succeed");
        let scope_for_other_ctx = plugin
            .last_scope()
            .expect("the plugin must receive a scope");
        assert!(
            format!("{scope_for_other_ctx:?}").contains(&OTHER_PERMIT_TENANT.to_string()),
            "the plugin must receive the PDP-compiled scope naming THIS caller's own tenant \
             ({OTHER_PERMIT_TENANT}), not a placeholder: {scope_for_other_ctx:?}"
        );

        // `ast::Expr` carries no `PartialEq`, so the compiled scopes are
        // compared by the `Debug` rendering the assertions above inspect.
        assert_ne!(
            format!("{scope_for_permit_ctx:?}"),
            format!("{scope_for_other_ctx:?}"),
            "two permitted calls naming different tenants must reach the plugin with two \
             different compiled scopes; a fixed placeholder scope shared by every permit would \
             pass both assertions above and fail only this one"
        );
    }

    #[tokio::test]
    async fn a_denied_read_dispatches_nothing() {
        let plugin = RecordingReconciliationPlugin::answering_for(AggregationFold::Sum);
        let service = service_with(plugin.clone(), declaration_with_fold(AggregationFold::Sum));
        service
            .get_reconciliation_metadata(
                &deny_ctx(),
                FIXTURE_TENANT,
                fixture_meter(),
                fixture_range(),
            )
            .await
            .expect_err("a denial must fail closed");
        assert_eq!(plugin.call_count(), 0, "a denial must dispatch nothing");
    }

    #[tokio::test]
    async fn authorization_precedes_declaration_resolution() {
        // A caller with no permission must not learn whether a GTS type
        // exists. The same order `query_aggregated_usage_records` uses.
        let service = service_with(
            RecordingReconciliationPlugin::answering_for(AggregationFold::Sum),
            no_such_declaration(),
        );
        let err = service
            .get_reconciliation_metadata(
                &deny_ctx(),
                FIXTURE_TENANT,
                fixture_meter(),
                fixture_range(),
            )
            .await
            .expect_err("expected the denial");
        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "an unresolvable meter under a denying PDP must answer the denial, not a 404 that \
             reveals the meter does not exist: got {err:?}"
        );
    }

    /// `tenant_id` is a caller-named selection key and `scope` is the only
    /// thing narrowing it, enforced inside the plugin — which the returned
    /// `ReconciliationMetadata` gives the gear no way to check. So the gear
    /// decides it, and answers the empty report the SPI already requires of
    /// a plugin for a tenant outside the grant.
    ///
    /// Not an error: that would tell a caller which tenants exist.
    #[tokio::test]
    async fn a_tenant_outside_the_grant_dispatches_nothing_and_reports_empty() {
        let plugin = RecordingReconciliationPlugin::answering_for(AggregationFold::Sum);
        let service = service_with(plugin.clone(), declaration_with_fold(AggregationFold::Sum));

        // `permit_ctx` is granted PERMIT_TENANT; FIXTURE_TENANT is another.
        let metadata = service
            .get_reconciliation_metadata(
                &permit_ctx(),
                FIXTURE_TENANT,
                fixture_meter(),
                fixture_range(),
            )
            .await
            .expect("an out-of-scope tenant reports empty rather than failing");

        assert_eq!(
            metadata,
            usage_collector_sdk::v2::ReconciliationMetadata::empty_for(AggregationFold::Sum),
        );
        assert_eq!(
            plugin.call_count(),
            0,
            "nothing may be dispatched for a tenant the grant does not pin",
        );
    }

    #[tokio::test]
    async fn a_plugin_returning_the_wrong_branch_is_an_internal_error() {
        // Without this guard a `SUM` meter answered with an observation count
        // serialises as a valid-looking body.
        let plugin = RecordingReconciliationPlugin::answering_for(AggregationFold::Max); // wrong branch
        let service = service_with(plugin, declaration_with_fold(AggregationFold::Sum));
        let err = service
            .get_reconciliation_metadata(
                &permit_ctx(),
                PERMIT_TENANT,
                fixture_meter(),
                fixture_range(),
            )
            .await
            .expect_err("a branch/fold disagreement is a host-contract breach");
        assert!(
            matches!(err, UsageCollectorError::Internal { .. }),
            "got {err:?}"
        );
    }
}
