//! Usage-collector authorization permissions catalog.
//!
//! Declares every permission the usage-collector can be granted as a
//! well-known GTS instance of [`AuthzPermissionV1`] via [`gts_instance!`]. Each
//! invocation submits an [`InventoryInstance`] to the process-wide
//! `toolkit-gts` inventory; `types-registry::init()` aggregates them at boot.
//!
//! `resource_type` is the concrete ingestion type id from the
//! `usage_collector_sdk` resource const — an exact id, not a wildcard, since
//! `usage_record` is a flat resource with no derived subtypes. There is no
//! usage-type catalog permission family any more: every type declaration is
//! owned by `types-registry`, which authorizes its own surface. `action`
//! values come from `crate::domain::authz::usage_record::actions` — the same
//! constants the `PolicyEnforcer` gate passes — so a catalogued action cannot
//! drift in spelling from the one the gate enforces.
//!
//! Instance id layout: `gts.cf.toolkit.authz.permission.v1~cf.core.uc.<seg>.v1`.
//!
//! [`AuthzPermissionV1`]: toolkit_gts::AuthzPermissionV1
//! [`InventoryInstance`]: toolkit_gts::InventoryInstance
//! [`gts_instance!`]: toolkit_gts::gts_instance

use toolkit_gts::{AuthzPermissionV1, gts_instance};
use usage_collector_sdk::USAGE_RECORD_RESOURCE;

use crate::domain::authz::usage_record;

// ---- usage_record (gts.cf.core.uc.usage_record.v1~) -----------------------

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_create.v1"),
        resource_type: USAGE_RECORD_RESOURCE.to_owned(),
        action: usage_record::actions::CREATE.to_owned(),
        display_name: "Create usage record".to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_get.v1"),
        resource_type: USAGE_RECORD_RESOURCE.to_owned(),
        action: usage_record::actions::GET.to_owned(),
        display_name: "Get usage record".to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_list.v1"),
        resource_type: USAGE_RECORD_RESOURCE.to_owned(),
        action: usage_record::actions::LIST.to_owned(),
        display_name: "List usage records".to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_backfill.v1"),
        resource_type: USAGE_RECORD_RESOURCE.to_owned(),
        action: usage_record::actions::BACKFILL.to_owned(),
        display_name: "Import or withdraw usage records for periods older than the backfill window"
            .to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_read_feed.v1"),
        resource_type: USAGE_RECORD_RESOURCE.to_owned(),
        action: usage_record::actions::READ_FEED.to_owned(),
        display_name: "Read the usage feed".to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_reconcile.v1"),
        resource_type: USAGE_RECORD_RESOURCE.to_owned(),
        action: usage_record::actions::RECONCILE.to_owned(),
        display_name: "Read per-scope reconciliation counters and watermarks".to_owned(),
    }
}

#[cfg(test)]
#[path = "permissions_tests.rs"]
mod permissions_tests;
