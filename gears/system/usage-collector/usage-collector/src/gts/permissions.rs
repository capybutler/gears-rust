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

#[cfg(test)]
mod tests {
    use toolkit_gts::{GtsId, InventoryInstance, gts_id};

    use super::usage_record;

    const PERMISSION_TYPE_ID: &str = gts_id!("cf.toolkit.authz.permission.v1~");
    /// Usage-collector instance-segment coordinates (`cf.core.uc`) — the
    /// vendor / package / namespace every UC permission instance's concrete
    /// segment carries. Matched structurally against the parsed GTS segment
    /// (not by raw-string prefix), so a lookalike namespace cannot slip through.
    const UC_VENDOR: &str = "cf";
    const UC_PACKAGE: &str = "core";
    const UC_NAMESPACE: &str = "uc";

    /// One per `(resource_type, action)` in the usage-collector PEP
    /// vocabulary — the grantable set, which is the catalog's job to
    /// publish — each id paired with the verb it actually grants.
    ///
    /// The single home for the id set: the three tests below all read it,
    /// so the set cannot be stated in two places and drift between them.
    ///
    /// **The pairing is the load-bearing half.** The id set and its size
    /// are pinned below and the action spellings are pinned in
    /// `authz_tests`, but neither crosses id to action: a `gts_instance!`
    /// block pairing the backfill id with `actions::CREATE` satisfies every
    /// one of those checks. An operator granting a permission by its
    /// displayed name would then receive a different verb than the name
    /// promises — here, `create` behind a name offering the elevated
    /// import.
    ///
    /// `usage_record_backfill` was declared ahead of the route that passes
    /// it, so an operator could grant the elevated action before an import
    /// job existed. `Service::backfill_usage_records` passes it now, for an
    /// entry whose covered period ends beyond the configured window.
    const EXPECTED_ID_ACTIONS: &[(&str, &str)] = &[
        (
            gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_create.v1"),
            usage_record::actions::CREATE,
        ),
        (
            gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_get.v1"),
            usage_record::actions::GET,
        ),
        (
            gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_list.v1"),
            usage_record::actions::LIST,
        ),
        (
            gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_backfill.v1"),
            usage_record::actions::BACKFILL,
        ),
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
                    })
            })
            .collect()
    }

    #[test]
    fn all_uc_permissions_registered_in_inventory() {
        let entries = uc_permission_instances();
        assert_eq!(
            entries.len(),
            EXPECTED_ID_ACTIONS.len(),
            "expected {} usage-collector permission instances; found {}: {:?}",
            EXPECTED_ID_ACTIONS.len(),
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
        for (expected, _) in EXPECTED_ID_ACTIONS {
            assert!(
                actual.contains(expected),
                "missing permission id: {expected}"
            );
        }
        assert_eq!(actual.len(), EXPECTED_ID_ACTIONS.len());
    }

    /// Crosses id to action, which nothing else does.
    ///
    /// The catalogued verb is read back off the registered instance's own
    /// payload — the JSON `types-registry` aggregates and a role editor
    /// ultimately renders — rather than off the constant the block was
    /// written with, so the assertion travels the same path an operator's
    /// grant does.
    #[test]
    fn each_permission_id_grants_the_verb_its_entry_names() {
        let catalogued: std::collections::BTreeMap<&str, serde_json::Value> =
            uc_permission_instances()
                .iter()
                .map(|e| (e.instance_id, (e.payload_fn)()))
                .collect();

        for (id, expected_action) in EXPECTED_ID_ACTIONS {
            let payload = catalogued
                .get(id)
                .unwrap_or_else(|| panic!("no registered instance for {id}"));
            assert_eq!(
                payload.get("action").and_then(serde_json::Value::as_str),
                Some(*expected_action),
                "permission {id} is catalogued against the wrong verb; an \
                 operator granting it by its displayed name would receive \
                 that verb instead of the one the name promises",
            );
        }
    }
}
