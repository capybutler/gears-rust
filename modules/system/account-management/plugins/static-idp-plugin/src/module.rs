//! Glue wiring the echo Service to `IdpPluginClient` and publishing the GTS instance — see crate root (lib.rs) for behaviour and prod-safety warnings.

use std::sync::{Arc, OnceLock};

use account_management_sdk::{IdpPluginClient, IdpPluginSpecV1};
use async_trait::async_trait;
use modkit::Module;
use modkit::context::ModuleCtx;
use modkit::gts::PluginV1;
use tracing::{info, warn};
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

use crate::config::StaticIdpPluginConfig;
use crate::domain::Service;

/// Static `IdP` plugin module.
///
/// Registers the permissive echo [`Service`] as the process-wide
/// `IdpPluginClient` so Account Management's bootstrap saga and tenant
/// lifecycle flows succeed without a real `IdP` deployment.
///
/// Account Management resolves the plugin via an **unscoped**
/// `ClientHub::get::<dyn IdpPluginClient>()` lookup, so this plugin
/// registers itself unscoped — the GTS instance is still published to
/// `types-registry` for catalogue visibility, but the client trait
/// object is the load-bearing artefact.
#[modkit::module(
    name = "static-idp-plugin",
    deps = ["types-registry"]
)]
pub struct StaticIdpPlugin {
    service: OnceLock<Arc<Service>>,
}

impl Default for StaticIdpPlugin {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Module for StaticIdpPlugin {
    async fn init(&self, ctx: &ModuleCtx) -> anyhow::Result<()> {
        warn!(
            "Static IdP plugin is running in echo mode - every provision/deprovision \
             succeeds without contacting a real IdP. Do NOT use this plugin in production."
        );

        let cfg: StaticIdpPluginConfig = ctx.config_or_default()?;
        info!(
            vendor = %cfg.vendor,
            priority = cfg.priority,
            "Loaded plugin configuration"
        );

        let service = Arc::new(Service::new());

        // Build registration payload and instance id for this plugin.
        let (instance_id, instance_json) = PluginV1::<IdpPluginSpecV1>::build_registration(
            "cf.builtin.static_idp.plugin.v1",
            cfg.vendor.clone(),
            cfg.priority,
        )?;

        // Publish to types-registry for catalogue visibility.
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json.clone()]).await?;
        // Idempotent restart: treat `AlreadyExists` as success only when
        // the stored spec matches our current serialized instance; fail
        // otherwise so a stale registration under the same ID surfaces
        // immediately rather than masking a config drift. Mirrors the
        // pattern in `account_management::module::init` for the AM-
        // owned TR plugin.
        for result in &results {
            if let RegisterResult::Err { error, .. } = result {
                if error.is_already_exists() {
                    let existing =
                        registry
                            .get_instance(instance_id.as_ref())
                            .await
                            .map_err(|e| {
                                anyhow::anyhow!("static-idp-plugin: verify existing instance: {e}")
                            })?;
                    if existing.object != instance_json {
                        return Err(anyhow::anyhow!(
                            "static-idp-plugin: instance `{instance_id}` already registered \
                             with a different spec",
                        ));
                    }
                } else {
                    return Err(anyhow::anyhow!(
                        "static-idp-plugin: registration failed: {error}"
                    ));
                }
            }
        }

        self.service
            .set(service.clone())
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;

        // AM consumes the IdP plugin via an unscoped `get` (see
        // `account_management::module::Module::init` — single IdP
        // plugin per process).
        let api: Arc<dyn IdpPluginClient> = service;
        ctx.client_hub().register::<dyn IdpPluginClient>(api);

        info!(instance_id = %instance_id);
        Ok(())
    }
}
