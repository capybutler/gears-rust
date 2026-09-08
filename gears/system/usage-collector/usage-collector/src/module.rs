//! `usage-collector` module.

use std::sync::{Arc, OnceLock};

use anyhow::Context;
use async_trait::async_trait;
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;
use usage_collector_sdk::UsageCollectorClientV1;

use crate::api::rest::routes as rest_routes;
use crate::config::UsageCollectorConfig;
use crate::domain::ports::metrics::UsageCollectorMetrics;
use crate::domain::{Service, UsageCollectorLocalClient};

/// Usage Collector gateway module.
///
/// This module:
/// 1. Reads the `[usage-collector]` configuration once at `init` (vendor
///    binding only — every type declaration is owned by `types-registry`,
///    not this gear).
/// 2. Resolves the PDP (`authz-resolver`) hard dependency and builds a
///    [`PolicyEnforcer`].
/// 3. Constructs the domain [`Service`] (embedded `GtsPluginSelector` for
///    lazy storage-plugin resolution; PDP enforcer is passed in at
///    construction).
/// 4. Registers `Arc<dyn UsageCollectorClientV1>` in `ClientHub` for in-process
///    consumers.
///
/// The durable `usage_records` rows live in the bound storage plugin's
/// backend, so the gateway hosts no database of its own and declares no `db`
/// capability. It runs no background lifecycle task either: type resolution
/// is request-driven and TTL-cached in-process (see
/// `crate::domain::type_resolver`), with nothing left to refresh on a timer
/// now that the usage-type catalog is gone.
///
/// The `UsageCollectorPluginSpecV1` schema itself reaches `types-registry`
/// automatically via the `toolkit-gts` link-time inventory — no per-init
/// registration is needed.
#[toolkit::gear(
    name = "usage-collector",
    deps = [types_registry, authz_resolver],
    capabilities = [rest]
)]
#[derive(Default)]
pub struct UsageCollectorModule {
    service: OnceLock<Arc<Service>>,
}

#[async_trait]
impl Gear for UsageCollectorModule {
    #[tracing::instrument(skip_all, fields(vendor))]
    // @cpt-flow:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-constraint-nfr-thresholds:p2
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // 1. Read-once: `[usage_collector].vendor` is read exactly once here;
        //    changing the binding requires a module restart (no runtime
        //    config-change channel).
        // @cpt-begin:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-config-read-once
        let cfg: UsageCollectorConfig = ctx.config_or_default()?;
        // @cpt-end:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-config-read-once
        cfg.validate()?;
        tracing::Span::current().record("vendor", cfg.vendor.as_str());
        info!(vendor = %cfg.vendor);

        // 2. PEP boundary — resolve the PDP (`authz-resolver`) hard dependency
        //    from ClientHub. The collector fails init if no resolver client is
        //    registered; it never serves a permissive or local authorization
        //    decision per
        //    `cpt-cf-usage-collector-principle-pdp-centric-authorization`.
        // @cpt-dod:cpt-cf-usage-collector-dod-foundation-adr-pdp-centric-authorization:p2
        // @cpt-dod:cpt-cf-usage-collector-dod-foundation-principle-fail-closed:p2
        let authz: Arc<dyn AuthZResolverApi> = ctx
            .client_hub()
            .get::<dyn AuthZResolverApi>()
            .with_context(|| format!("{} requires an authz-resolver client", Self::MODULE_NAME))?;
        let enforcer = PolicyEnforcer::new(authz);
        info!(module = Self::MODULE_NAME, "authz-resolver wired");

        // 2b. Observability substrate — declare the operational instruments on
        //     a scoped `Meter` from ToolKit's global `SdkMeterProvider` (OTLP
        //     push; no gear-local exporter or `/metrics` scrape endpoint) per
        //     `cpt-cf-usage-collector-principle-otlp-push-emission`. The
        //     `authz-resolver` client is bound above, so the PDP-readiness
        //     gauge is a constant `1` post-bootstrap (structural binding fact).
        // @cpt-begin:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-meter-bootstrap
        let metrics = crate::infra::metrics::build_default_adapter(cfg.metrics.effective_prefix());
        // @cpt-begin:cpt-cf-usage-collector-algo-foundation-pdp-authorize:p2:inst-algo-pdp-ready-gauge
        metrics.set_pdp_ready(true);
        // @cpt-end:cpt-cf-usage-collector-algo-foundation-pdp-authorize:p2:inst-algo-pdp-ready-gauge
        // @cpt-end:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-meter-bootstrap

        // 3. Construct the plugin-routing domain service (embeds
        //    `GtsPluginSelector`; no types-registry query at init —
        //    storage-plugin resolution is lazy). Durable `usage_records` rows
        //    live in the bound plugin; the service routes ingestion SPI
        //    calls through `ClientHub::try_get_scoped::<dyn UsageCollectorPluginV1>`.
        let hub = ctx.client_hub();
        // The Type Resolver's `types-registry` adapter is built here, at the
        // bootstrap layer, and injected as a finished `Arc<TypeResolver>` —
        // the same shape `metrics` above already uses (`build_default_adapter`
        // built from config, then passed into `new_with_metrics`) — so the
        // domain layer never needs to name the concrete adapter type. The
        // resolver shares the same metrics adapter as the rest of the gear,
        // so `uc_type_resolution_total` lands in the one instrument set.
        let type_resolver = crate::infra::types_registry_source::build_default_resolver(
            Arc::clone(&hub),
            cfg.type_cache_ttl_secs,
            cfg.type_cache_capacity,
            metrics.clone(),
        );
        // Projected before `cfg.vendor` is moved out of `cfg`. The
        // projection is infallible because `cfg.validate()` above already
        // refused a zero or out-of-`i64` bound.
        let covered_period_bounds = cfg.covered_period_bounds();
        let svc = Service::new_with_metrics(
            hub,
            cfg.vendor,
            enforcer,
            metrics,
            type_resolver,
            cfg.metadata_size_cap_bytes,
            covered_period_bounds,
        );

        let svc = Arc::new(svc);
        self.service
            .set(svc.clone())
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;

        // 4. Register local client in ClientHub for in-process consumers.
        let api: Arc<dyn UsageCollectorClientV1> = Arc::new(UsageCollectorLocalClient::new(svc));
        ctx.client_hub().register::<dyn UsageCollectorClientV1>(api);

        Ok(())
    }
}

impl RestApiCapability for UsageCollectorModule {
    /// Mount the FOUNDATION REST surface onto the runtime router.
    ///
    /// Wires the shared substrate execution shape — gateway-resolved
    /// `SecurityContext` acceptance with fail-closed `AuthN`-delegation
    /// rejection, the canonical RFC-9457 `Problem` envelope (via the
    /// host-crate `UsageCollectorError` → `CanonicalError` lift), W3C
    /// trace-context correlation propagation — and registers the
    /// usage-record ingestion routes. There is no usage-type catalog route:
    /// every type declaration is owned by `types-registry`. No module-local
    /// health / liveness / readiness / metrics endpoint is exposed — those
    /// are owned by the `ToolKit` host above the module boundary.
    ///
    /// The runtime calls `register_rest` AFTER `init` per the toolkit
    /// lifecycle contract, so the `OnceLock` read below is infallible in
    /// practice; the `ok_or_else` guard turns a misordered runtime into a
    /// precise bootstrap failure rather than a panic.
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        info!(module = Self::MODULE_NAME, "registering REST surface");

        // The domain Service (plugin binding + PDP/tenant) must be wired
        // before the foundation REST routes mount: every ingestion handler
        // dispatches through this service. Fail closed if init did not
        // complete. The in-process SDK surface (via `ClientHub`) wraps the
        // same `Arc<Service>` in `UsageCollectorLocalClient`, so REST and
        // SDK consumers share a single PDP-gated dispatch path through the
        // service.
        let service = self.service.get().cloned().ok_or_else(|| {
            anyhow::anyhow!("{} module Service not initialized", Self::MODULE_NAME)
        })?;

        let router = rest_routes::register_routes(router, openapi, service);

        info!(module = Self::MODULE_NAME, "REST surface registered");
        Ok(router)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "module_tests.rs"]
mod module_tests;
