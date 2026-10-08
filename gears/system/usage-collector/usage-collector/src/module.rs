//! `usage-collector` module.

use std::sync::{Arc, OnceLock};

use anyhow::Context;
use async_trait::async_trait;
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use sea_orm_migration::MigrationTrait;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::DatabaseCapability;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;
use usage_collector_sdk::UsageCollectorClientV1;

use crate::api::rest::routes as rest_routes;
use crate::config::UsageCollectorConfig;
use crate::domain::ports::metrics::UsageCollectorMetrics;
use crate::domain::{Service, UsageCollectorLocalClient};
use crate::infra::declaration_mirror::DbDeclarationMirror;

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
/// Every durable `usage_records` row still lives wholly in the bound storage
/// plugin's own backend — that entry ledger is never this gear's concern.
/// But the gear now declares the `db` capability and owns one durable table
/// of its own: the DESIGN §3.7 declaration mirror, a cache of resolved GTS
/// type declarations that lets a lost declaration be restored to
/// `types-registry` rather than failing every later resolution closed
/// (`cpt-cf-usage-collector-adr-declaration-rehydration`). The mirror is
/// temporary — ADR statement 7 retires it, and this `db` capability with it,
/// once `types-registry` gains persistent storage on this gear's own
/// resolution path. It runs no background lifecycle task either: type
/// resolution is request-driven and TTL-cached in-process (see
/// `crate::domain::type_resolver`), with nothing left to refresh on a timer
/// now that the usage-type catalog is gone.
///
/// The `UsageCollectorPluginSpecV1` schema itself reaches `types-registry`
/// automatically via the `toolkit-gts` link-time inventory — no per-init
/// registration is needed.
///
/// **`deps = [types_registry, authz_resolver]` below names the type
/// registry and the PDP** — two of the three names
/// `cpt-cf-usage-collector-dod-no-identity-enrichment`'s second sentence
/// bounds the outbound dependency set to. The third, the bound storage
/// plugin, is deliberately absent from `deps`: it resolves dynamically
/// through the embedded `GtsPluginSelector` and `ClientHub` (this struct's
/// own module doc, above), never as a hard gear dependency.
///
/// **Fix round 1 corrects two mechanism claims this comment used to make,
/// both wrong, found independently by two reviewers.**
///
/// 1. `deps` is **not** a reachability bound. Per `#[gear]`'s own doc
///    (`libs/toolkit-macros/src/lib.rs`, "Gear dependencies (`deps`)"),
///    `deps` is a gear-to-gear **linking and bootstrap-graph** declaration —
///    it keeps a dependency's `inventory::submit!` registration alive
///    through the linker and orders `init`. It says nothing about which
///    clients this gear can *reach* at runtime. `ClientHub::get<T>` is keyed
///    by Rust type alone and has no relationship to `deps`: any gear holding
///    a `ClientHub` handle can fetch any client registered under it,
///    `deps`-listed or not — which is exactly how this gear reaches the
///    storage plugin at all despite the plugin never appearing in `deps`.
/// 2. **A `Cargo.toml` read cannot prove a transitive dependency absent.**
///    It was claimed that "only a full manifest read can" rule out a
///    smuggled fourth target; a manifest read sees **direct** dependencies
///    only. The transitive question needs the resolved dependency graph,
///    which is what
///    `data_classification_tests::the_gear_s_transitive_dependency_graph_names_no_identity_or_people_directory_crate`
///    now reads directly (`cargo tree`), rather than being asserted here.
///
/// Steps 4 ("no reverse lookup") and 5 ("no enrich") of
/// `cpt-cf-usage-collector-algo-opaque-identifier-handling` are **not**
/// proven by this attribute either, for the same reason: reachability is a
/// `ClientHub` property, not a `deps` property. They rest on the same two
/// dependency-graph pins (direct and transitive) finding no identity,
/// directory, account or profile crate anywhere in either graph — an
/// absence of the *means*, not a logical impossibility derived from `deps`.
/// (That algo's other eight steps are realized at
/// `domain::authz::AttributionTupleKey`, a different seam, marked at its
/// own declaration.)
// @cpt-dod:cpt-cf-usage-collector-dod-no-identity-enrichment:p3
// @cpt-algo:cpt-cf-usage-collector-algo-opaque-identifier-handling:p3
#[toolkit::gear(
    name = "usage-collector",
    deps = [types_registry, authz_resolver],
    capabilities = [rest, db]
)]
#[derive(Default)]
pub struct UsageCollectorModule {
    service: OnceLock<Arc<Service>>,
}

#[async_trait]
impl Gear for UsageCollectorModule {
    #[tracing::instrument(skip_all, fields(vendor))]
    // @cpt-flow:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-plugin-vendor-configuration:p1
    // @cpt-dod:cpt-cf-usage-collector-constraint-nfr-thresholds:p2
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // No `types-registry` query runs anywhere in this function — plugin
        // resolution is lazy (`Service::get_plugin`, first real dispatch).
        // @cpt-begin:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-no-init-query
        // 1. Read-once: `[usage_collector].vendor` is read exactly once here;
        //    changing the binding requires a module restart (no runtime
        //    config-change channel). Validated before anything is wired.
        // @cpt-begin:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-read-init
        let cfg: UsageCollectorConfig = ctx.config_or_default()?;
        cfg.validate()?;
        // @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-read-init
        tracing::Span::current().record("vendor", cfg.vendor.as_str());
        info!(vendor = %cfg.vendor);

        // 2. PEP boundary — resolve the PDP (`authz-resolver`) hard dependency
        //    from ClientHub. The collector fails init if no resolver client is
        //    registered; it never serves a permissive or local authorization
        //    decision per
        //    `cpt-cf-usage-collector-principle-pdp-centric-authorization`.
        // @cpt-dod:cpt-cf-usage-collector-adr-pdp-centric-authorization:p2
        // @cpt-dod:cpt-cf-usage-collector-principle-fail-closed:p2
        let authz: Arc<dyn AuthZResolverApi> = ctx
            .client_hub()
            .get::<dyn AuthZResolverApi>()
            .with_context(|| format!("{} requires an authz-resolver client", Self::MODULE_NAME))?;
        let enforcer = PolicyEnforcer::new(authz);
        info!(module = Self::MODULE_NAME, "authz-resolver wired");

        // The declaration mirror's database. `db_required` rather than `db`:
        // ADR-0015 statement 2 says the gear owns one mirror table and its
        // Consequences say the gear "gains its first durable table and its
        // first database dependency", unconditionally. A configured database
        // is a deployment obligation; an UNAVAILABLE one is a runtime
        // condition statements 3 and 4 already handle — a failed write is
        // counted and the entry served, a failed restore fails closed.
        let db = ctx.db_required()?;

        // 2b. Observability substrate — declare the operational instruments on
        //     a scoped `Meter` from ToolKit's global `SdkMeterProvider` (OTLP
        //     push; no gear-local exporter or `/metrics` scrape endpoint) per
        //     `cpt-cf-usage-collector-principle-otlp-push-emission`. The
        //     `authz-resolver` client is bound above, so the PDP-readiness
        //     gauge is a constant `1` post-bootstrap (structural binding fact).
        // @cpt-begin:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-binding-meter-bootstrap
        let metrics = crate::infra::metrics::build_default_adapter(
            cfg.metrics.effective_prefix(),
            cfg.max_batch_records,
        );
        // @cpt-algo:cpt-cf-usage-collector-algo-readiness-signal-derivation:p2
        // @cpt-dod:cpt-cf-usage-collector-dod-readiness-signals:p2
        // @cpt-begin:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-algo-pdp-ready-gauge
        metrics.set_pdp_ready(true);
        // @cpt-end:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-algo-pdp-ready-gauge
        // @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-binding-meter-bootstrap

        // 3. Construct the plugin-routing domain service (embeds
        //    `GtsPluginSelector`; no types-registry query at init —
        //    storage-plugin resolution is lazy). Durable `usage_records` rows
        //    live in the bound plugin; the service routes ingestion SPI
        //    calls through `ClientHub::try_get_scoped::<dyn UsageCollectorPluginV1>`.
        let hub = ctx.client_hub();
        // The `types-registry` adapter is built here, at the bootstrap layer,
        // and both resolvers over it are injected as finished objects — the
        // same shape `metrics` above already uses (`build_default_adapter`
        // built from config, then passed into `new_with_metrics`) — so the
        // domain layer never needs to name the concrete adapter type. They
        // share the same metrics adapter as the rest of the gear, so
        // `uc_type_resolution_total` lands in the one instrument set, and the
        // same mirror, so a declaration resolved one way populates the cache
        // the other way reads.
        let (type_resolver, reverse_resolver) =
            crate::infra::types_registry_source::build_default_resolvers(
                Arc::clone(&hub),
                Arc::new(DbDeclarationMirror::new(db)),
                cfg.type_cache_ttl_secs,
                cfg.type_cache_capacity,
                metrics.clone(),
            );
        // Projected before `cfg.vendor` is moved out of `cfg`. The
        // projection is infallible because `cfg.validate()` above already
        // refused a zero or out-of-`i64` bound.
        let covered_period_bounds = cfg.covered_period_bounds();
        let max_batch_records = cfg.max_batch_records;
        // The `[usage_collector.ingestion_quota]` block reaches the service
        // verbatim; `cfg.validate()` above has already refused a
        // `burst_entries` below `max_batch_records`, so the service can
        // assume a maximal batch is admissible and the rejection's retry
        // delay is finite (DESIGN §3.2 / §3.8).
        let ingestion_quota = cfg.ingestion_quota;
        let unavailable_retry_after_secs = cfg.unavailable_retry_after_secs;
        let target_not_converged_retry_after_secs = cfg.target_not_converged_retry_after_secs;
        let svc = Service::new_with_metrics(
            hub,
            cfg.vendor,
            enforcer,
            metrics,
            type_resolver,
            cfg.metadata_size_cap_bytes,
            covered_period_bounds,
            max_batch_records,
            ingestion_quota,
            unavailable_retry_after_secs,
            target_not_converged_retry_after_secs,
            reverse_resolver,
        );

        let svc = Arc::new(svc);
        self.service
            .set(svc.clone())
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;

        // 4. Register local client in ClientHub for in-process consumers.
        let api: Arc<dyn UsageCollectorClientV1> = Arc::new(UsageCollectorLocalClient::new(svc));
        ctx.client_hub().register::<dyn UsageCollectorClientV1>(api);

        // @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-no-init-query
        // Step 8's own RETURN: a running gear whose entry ledger is served by
        // the selected backend, with no gear-side code aware of which one it
        // is — `Service`'s `get_plugin` resolves that lazily per call.
        // @cpt-begin:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-return
        Ok(())
        // @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-return
    }
}

impl DatabaseCapability for UsageCollectorModule {
    fn migrations(&self) -> Vec<Box<dyn MigrationTrait>> {
        // The declaration mirror of DESIGN §3.7 — one table, and temporary
        // (cpt-cf-usage-collector-adr-declaration-rehydration statement 7).
        crate::infra::declaration_mirror::migrations()
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
