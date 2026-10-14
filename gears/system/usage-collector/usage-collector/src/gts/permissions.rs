//! Usage-collector authorization permissions catalog.
//!
//! Declares every permission the usage-collector can be granted as a
//! well-known GTS instance of [`AuthzPermissionV1`] via [`gts_instance!`]. Each
//! invocation submits an [`InventoryInstance`] to the process-wide
//! `toolkit-gts` inventory; `types-registry::init()` aggregates them at boot.
//!
//! `resource_type` values are the concrete catalog / ingestion type ids from
//! the `usage_collector_sdk` resource consts — exact ids, not wildcards, since
//! these are flat platform-global (`usage_type`) and ingestion (`usage_record`)
//! resources with no derived subtypes. `action` values come from
//! `crate::domain::authz::{usage_type, usage_record}::actions` — the same
//! constants the `PolicyEnforcer` gate passes — so the catalog cannot drift
//! from what the REST surface actually enforces.
//!
//! Instance id layout: `gts.cf.toolkit.authz.permission.v1~cf.core.uc.<seg>.v1`.
//!
//! [`AuthzPermissionV1`]: toolkit_gts::AuthzPermissionV1
//! [`InventoryInstance`]: toolkit_gts::InventoryInstance
//! [`gts_instance!`]: toolkit_gts::gts_instance

use toolkit_gts::{AuthzPermissionV1, gts_instance};
use usage_collector_sdk::{USAGE_RECORD_RESOURCE, USAGE_TYPE_RESOURCE};

use crate::domain::authz::{usage_record, usage_type};

// ---- usage_type (gts.cf.core.uc.usage_type.v1~) ---------------------------

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_type_create.v1"),
        resource_type: USAGE_TYPE_RESOURCE.to_owned(),
        action: usage_type::actions::CREATE.to_owned(),
        display_name: "Create usage type".to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_type_get.v1"),
        resource_type: USAGE_TYPE_RESOURCE.to_owned(),
        action: usage_type::actions::GET.to_owned(),
        display_name: "Get usage type".to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_type_list.v1"),
        resource_type: USAGE_TYPE_RESOURCE.to_owned(),
        action: usage_type::actions::LIST.to_owned(),
        display_name: "List usage types".to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_type_delete.v1"),
        resource_type: USAGE_TYPE_RESOURCE.to_owned(),
        action: usage_type::actions::DELETE.to_owned(),
        display_name: "Delete usage type".to_owned(),
    }
}

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
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_deactivate.v1"),
        resource_type: USAGE_RECORD_RESOURCE.to_owned(),
        action: usage_record::actions::DEACTIVATE.to_owned(),
        display_name: "Deactivate usage record".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use toolkit_gts::{GtsId, InventoryInstance, gts_id};

    const PERMISSION_TYPE_ID: &str = gts_id!("cf.toolkit.authz.permission.v1~");
    /// Usage-collector instance-segment coordinates (`cf.core.uc`) — the
    /// vendor / package / namespace every UC permission instance's concrete
    /// segment carries. Matched structurally against the parsed GTS segment
    /// (not by raw-string prefix), so a lookalike namespace cannot slip through.
    const UC_VENDOR: &str = "cf";
    const UC_PACKAGE: &str = "core";
    const UC_NAMESPACE: &str = "uc";

    /// One per `(resource_type, action)` the usage-collector REST/PEP surface
    /// enforces.
    const EXPECTED_PERMISSION_IDS: &[&str] = &[
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_type_create.v1"),
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_type_get.v1"),
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_type_list.v1"),
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_type_delete.v1"),
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_create.v1"),
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_get.v1"),
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_list.v1"),
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_deactivate.v1"),
    ];

    fn uc_permission_instances() -> Vec<&'static InventoryInstance> {
        inventory::iter::<InventoryInstance>
            .into_iter()
            .filter(|e| {
                // Parse the instance id through the GTS grammar rather than
                // slicing the raw string: select concrete permission instances
                // (type id == `PERMISSION_TYPE_ID`) whose derivation segment
                // sits in the usage-collector namespace.
                let Ok(parsed) = GtsId::try_new(e.instance_id) else {
                    return false;
                };
                parsed.get_type_id().as_deref() == Some(PERMISSION_TYPE_ID)
                    && parsed.segments().last().is_some_and(|seg| {
                        seg.vendor() == UC_VENDOR
                            && seg.package() == UC_PACKAGE
                            && seg.namespace() == UC_NAMESPACE
                            // Generation 2 registers `cf.core.uc.usage_record_*.v2` into the same
                            // inventory; this catalog is generation 1's, so select only `.v1`.
                            && seg.ver_major_opt() == Some(1)
                    })
            })
            .collect()
    }

    #[test]
    fn all_uc_permissions_registered_in_inventory() {
        let entries = uc_permission_instances();
        assert_eq!(
            entries.len(),
            EXPECTED_PERMISSION_IDS.len(),
            "expected {} usage-collector permission instances; found {}: {:?}",
            EXPECTED_PERMISSION_IDS.len(),
            entries.len(),
            entries.iter().map(|e| e.instance_id).collect::<Vec<_>>()
        );
        for entry in &entries {
            assert_eq!(
                entry.type_id, PERMISSION_TYPE_ID,
                "instance {} derived wrong type_id",
                entry.instance_id
            );
        }
    }

    #[test]
    fn uc_permission_inventory_covers_every_expected_id() {
        let actual: std::collections::BTreeSet<&str> = uc_permission_instances()
            .iter()
            .map(|e| e.instance_id)
            .collect();
        for expected in EXPECTED_PERMISSION_IDS {
            assert!(
                actual.contains(expected),
                "missing permission id: {expected}"
            );
        }
        assert_eq!(actual.len(), EXPECTED_PERMISSION_IDS.len());
    }
}
