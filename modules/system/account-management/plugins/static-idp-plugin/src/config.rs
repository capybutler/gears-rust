//! Configuration for the static `IdP` plugin.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticIdpPluginConfig {
    /// Vendor name for GTS instance registration.
    pub vendor: String,

    /// Plugin priority (lower = higher priority).
    pub priority: i16,
}

impl Default for StaticIdpPluginConfig {
    fn default() -> Self {
        Self {
            vendor: "cyberfabric".to_owned(),
            priority: 100,
        }
    }
}
