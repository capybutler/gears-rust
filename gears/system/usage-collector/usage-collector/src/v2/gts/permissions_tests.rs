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
/// The single home for the id set: the tests below all read it, so the set
/// cannot be stated in two places and drift between them.
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
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_create.v2"),
        usage_record::actions::CREATE,
    ),
    (
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_get.v2"),
        usage_record::actions::GET,
    ),
    (
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_list.v2"),
        usage_record::actions::LIST,
    ),
    (
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_backfill.v2"),
        usage_record::actions::BACKFILL,
    ),
    (
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_read_feed.v2"),
        usage_record::actions::READ_FEED,
    ),
    (
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.uc.usage_record_reconcile.v2"),
        usage_record::actions::RECONCILE,
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
                        // Generation 1 registers `cf.core.uc.usage_*.v1` into the same
                        // inventory; this catalog is generation 2's, so select only `.v2`.
                        && seg.ver_major_opt() == Some(2)
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
    let catalogued: std::collections::BTreeMap<&str, serde_json::Value> = uc_permission_instances()
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
