//! GTS plugin spec for usage-collector storage-plugin discovery and binding.

use toolkit::gts::PluginV1;
use toolkit_gts::gts_id;
use toolkit_gts::gts_type_schema;

/// Canonical GTS resource type for the **ingestion** surface — the wire
/// `resource_type` carried by [`crate::UsageCollectorError`] envelopes about
/// a usage record (`create` / `deactivate` / `list` / `aggregate`).
pub const USAGE_RECORD_RESOURCE: &str = gts_id!("cf.core.uc.usage_record.v1~");

/// GTS plugin specification for usage-collector storage backends.
// @cpt-dod:cpt-cf-usage-collector-dod-foundation-contract-storage-plugin:p1
// @cpt-dod:cpt-cf-usage-collector-dod-foundation-contract-gts-registry:p1
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.uc.plugin.v1~"),
    description = "Usage Collector plugin specification",
    properties = "",
)]
pub struct UsageCollectorPluginSpecV1;
