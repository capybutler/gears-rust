//! Generation-2 bootstrap for the `usage-collector` gear.

use std::sync::Arc;

use anyhow::Context;
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use toolkit::GearCtx;
use tracing::info;
use usage_collector_sdk::v2::UsageCollectorClientV2;

use crate::config::UsageCollectorConfig;
use crate::v2::domain::ports::metrics::UsageCollectorMetrics;
use crate::v2::domain::{Service, UsageCollectorLocalClient};
use crate::v2::infra::storage::declaration_mirror::DbDeclarationMirror;

/// Generation 2 wiring used by the main gear module.
pub struct V2Wiring {
    /// Generation 2's domain service.
    pub service: Arc<Service>,
}

/// Build generation 2's domain service and register its `ClientHub` client.
///
/// # Errors
///
/// Returns an error if the configuration is invalid, if no `authz-resolver`
/// client is registered, or if no database is configured for the gear.
// @cpt-dod:cpt-cf-usage-collector-dod-no-identity-enrichment:p3
// @cpt-algo:cpt-cf-usage-collector-algo-opaque-identifier-handling:p3
// @cpt-flow:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1
// @cpt-dod:cpt-cf-usage-collector-dod-plugin-vendor-configuration:p1
// @cpt-dod:cpt-cf-usage-collector-constraint-nfr-thresholds:p2
pub fn init_v2(ctx: &GearCtx) -> anyhow::Result<V2Wiring> {
    // No `types-registry` query runs here; plugin resolution is lazy.
    // @cpt-begin:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-no-init-query
    // Vendor binding is read once at startup.
    // @cpt-begin:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-read-init
    let cfg: UsageCollectorConfig = ctx.config_or_default()?;
    cfg.validate()?;
    // @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-read-init
    info!(vendor = %cfg.vendor, "usage-collector generation 2 initializing");

    // Fail closed if the PDP client is unavailable.
    // @cpt-dod:cpt-cf-usage-collector-adr-pdp-centric-authorization:p2
    // @cpt-dod:cpt-cf-usage-collector-principle-fail-closed:p2
    let authz: Arc<dyn AuthZResolverApi> = ctx
        .client_hub()
        .get::<dyn AuthZResolverApi>()
        .with_context(|| "usage-collector v2 requires an authz-resolver client")?;
    let enforcer = PolicyEnforcer::new(authz);
    info!("authz-resolver wired for usage-collector generation 2");

    // Generation 2 owns the declaration mirror table.
    let db = ctx.db_required()?;

    // Metrics are global OTLP instruments; no gear-local exporter.
    // @cpt-begin:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-binding-meter-bootstrap
    let metrics = crate::v2::infra::metrics::build_default_adapter(
        cfg.metrics.effective_prefix(),
        cfg.max_batch_records,
    );
    // @cpt-algo:cpt-cf-usage-collector-algo-readiness-signal-derivation:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-readiness-signals:p2
    // @cpt-begin:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-algo-pdp-ready-gauge
    metrics.set_pdp_ready(true);
    // @cpt-end:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-algo-pdp-ready-gauge
    // @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-binding-meter-bootstrap

    // Storage-plugin resolution is lazy; durable usage rows live in the plugin.
    let hub = ctx.client_hub();
    // Forward and reverse resolvers share one registry adapter and mirror.
    let (type_resolver, reverse_resolver) =
        crate::v2::infra::types_registry_source::build_default_resolvers(
            Arc::clone(&hub),
            Arc::new(DbDeclarationMirror::new(db)),
            cfg.type_cache_ttl_secs,
            cfg.type_cache_capacity,
            metrics.clone(),
        );
    // Project before `cfg.vendor` is moved out of `cfg`.
    let service_config = cfg.service_config();
    let svc = Service::new_with_metrics(
        hub,
        cfg.vendor,
        enforcer,
        metrics,
        type_resolver,
        reverse_resolver,
        service_config,
    );

    let svc = Arc::new(svc);

    // Register local client in ClientHub for in-process consumers.
    let api: Arc<dyn UsageCollectorClientV2> =
        Arc::new(UsageCollectorLocalClient::new(Arc::clone(&svc)));
    ctx.client_hub().register::<dyn UsageCollectorClientV2>(api);

    // @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-no-init-query
    // @cpt-begin:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-return
    Ok(V2Wiring { service: svc })
    // @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-return
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "module_tests.rs"]
mod module_tests;
