//! Mini-chat authorization permissions catalog.
//!
//! Declares every permission mini-chat can be granted as a well-known GTS
//! instance of [`AuthzPermissionV1`] via the typed form of [`gts_instance!`].
//! Each call spells out the full `instance_id`
//! (`gts.cf.modkit.authz.permission.v1~<segment>`) — the macro emits a
//! compile-time assert that the literal's prefix matches
//! `<AuthzPermissionV1 as GtsSchema>::SCHEMA_ID` exactly, so a typo in the
//! prefix is a build error rather than a silent runtime mismatch. Each
//! invocation submits an [`InventoryInstance`] entry to the global
//! inventory collector; `types-registry::init()` picks them up at startup
//! and validates each payload against the `AuthzPermissionV1` schema.
//!
//! `action` values come from `crate::domain::service::actions` — the same
//! constants the PEP sees at `access_scope(...)` time.
//!
//! `resource_type` values are **wildcard patterns** (GTS §3.5) covering
//! the full mini_chat-derived subtree under each `ai_chat` base. At
//! evaluation time the PEP sends a concrete type id (from
//! `crate::domain::service::resources::*.name`); the PDP matches it
//! against these wildcards. This keeps the catalog forward-compatible:
//! if tomorrow someone derives `...~cf.core.mini_chat.chat.v1~vendor.ext.v1~`,
//! the existing permissions still cover it without a catalog edit.
//!
//! The typed struct literal gives compile-time field-name and type
//! checking; the `id` field is auto-injected by the macro.
//!
//! Instance ID layout (level-2, underscore marks the empty namespace slot):
//!
//! ```text
//! gts.cf.modkit.authz.permission.v1~cf.mini_chat._.<permission_name>.v1
//! ```
//!
//! [`AuthzPermissionV1`]: modkit_gts::AuthzPermissionV1
//! [`InventoryInstance`]: modkit_gts::InventoryInstance
//! [`gts_instance!`]: modkit_gts::gts_instance

use crate::domain::service::actions;
use modkit_gts::{AuthzPermissionV1, gts_instance};

/// Wildcard `resource_type` for permissions over any mini-chat chat.
///
/// Covers `gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~` (the
/// concrete type the PEP sends) plus any future derivation under that
/// subtree.
const CHAT_RESOURCE_TYPE_WILDCARD: &str = "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.*";

/// Wildcard `resource_type` for permissions over any mini-chat model.
const MODEL_RESOURCE_TYPE_WILDCARD: &str = "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.*";

/// Wildcard `resource_type` for permissions over any mini-chat user-quota.
const USER_QUOTA_RESOURCE_TYPE_WILDCARD: &str =
    "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.*";

// =====================================================================
//                       CHAT resource permissions
//           gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~
// =====================================================================

gts_instance! {
    AuthzPermissionV1 {
        id: "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_create.v1",
        resource_type: CHAT_RESOURCE_TYPE_WILDCARD.to_owned(),
        action: actions::CREATE.to_owned(),
        display_name: "Create chat".to_owned(),    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_read.v1",
        resource_type: CHAT_RESOURCE_TYPE_WILDCARD.to_owned(),
        action: actions::READ.to_owned(),
        display_name: "Read chat".to_owned(),    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_list.v1",
        resource_type: CHAT_RESOURCE_TYPE_WILDCARD.to_owned(),
        action: actions::LIST.to_owned(),
        display_name: "List chats".to_owned(),    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_update.v1",
        resource_type: CHAT_RESOURCE_TYPE_WILDCARD.to_owned(),
        action: actions::UPDATE.to_owned(),
        display_name: "Update chat".to_owned(),    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_delete.v1",
        resource_type: CHAT_RESOURCE_TYPE_WILDCARD.to_owned(),
        action: actions::DELETE.to_owned(),
        display_name: "Delete chat".to_owned(),    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_list_messages.v1",
        resource_type: CHAT_RESOURCE_TYPE_WILDCARD.to_owned(),
        action: actions::LIST_MESSAGES.to_owned(),
        display_name: "List chat messages".to_owned(),    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_send_message.v1",
        resource_type: CHAT_RESOURCE_TYPE_WILDCARD.to_owned(),
        action: actions::SEND_MESSAGE.to_owned(),
        display_name: "Send chat message".to_owned(),    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_read_turn.v1",
        resource_type: CHAT_RESOURCE_TYPE_WILDCARD.to_owned(),
        action: actions::READ_TURN.to_owned(),
        display_name: "Read chat turn".to_owned(),    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_retry_turn.v1",
        resource_type: CHAT_RESOURCE_TYPE_WILDCARD.to_owned(),
        action: actions::RETRY_TURN.to_owned(),
        display_name: "Retry chat turn".to_owned(),    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_edit_turn.v1",
        resource_type: CHAT_RESOURCE_TYPE_WILDCARD.to_owned(),
        action: actions::EDIT_TURN.to_owned(),
        display_name: "Edit chat turn".to_owned(),    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_delete_turn.v1",
        resource_type: CHAT_RESOURCE_TYPE_WILDCARD.to_owned(),
        action: actions::DELETE_TURN.to_owned(),
        display_name: "Delete chat turn".to_owned(),    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_upload_attachment.v1",
        resource_type: CHAT_RESOURCE_TYPE_WILDCARD.to_owned(),
        action: actions::UPLOAD_ATTACHMENT.to_owned(),
        display_name: "Upload attachment".to_owned(),    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_read_attachment.v1",
        resource_type: CHAT_RESOURCE_TYPE_WILDCARD.to_owned(),
        action: actions::READ_ATTACHMENT.to_owned(),
        display_name: "Read attachment".to_owned(),    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_delete_attachment.v1",
        resource_type: CHAT_RESOURCE_TYPE_WILDCARD.to_owned(),
        action: actions::DELETE_ATTACHMENT.to_owned(),
        display_name: "Delete attachment".to_owned(),    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_set_reaction.v1",
        resource_type: CHAT_RESOURCE_TYPE_WILDCARD.to_owned(),
        action: actions::SET_REACTION.to_owned(),
        display_name: "Set reaction".to_owned(),    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_delete_reaction.v1",
        resource_type: CHAT_RESOURCE_TYPE_WILDCARD.to_owned(),
        action: actions::DELETE_REACTION.to_owned(),
        display_name: "Delete reaction".to_owned(),    }
}

// =====================================================================
//                       MODEL resource permissions
//          gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~
// =====================================================================

gts_instance! {
    AuthzPermissionV1 {
        id: "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.model_list.v1",
        resource_type: MODEL_RESOURCE_TYPE_WILDCARD.to_owned(),
        action: actions::LIST.to_owned(),
        display_name: "List models".to_owned(),    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.model_read.v1",
        resource_type: MODEL_RESOURCE_TYPE_WILDCARD.to_owned(),
        action: actions::READ.to_owned(),
        display_name: "Read model".to_owned(),    }
}

// =====================================================================
//                    USER_QUOTA resource permissions
//    gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~
// =====================================================================

gts_instance! {
    AuthzPermissionV1 {
        id: "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.user_quota_read.v1",
        resource_type: USER_QUOTA_RESOURCE_TYPE_WILDCARD.to_owned(),
        action: actions::READ.to_owned(),
        display_name: "Read user quota".to_owned(),    }
}

#[cfg(test)]
mod tests {
    use super::{
        CHAT_RESOURCE_TYPE_WILDCARD, MODEL_RESOURCE_TYPE_WILDCARD,
        USER_QUOTA_RESOURCE_TYPE_WILDCARD, actions,
    };
    use crate::domain::service::resources;
    use modkit_gts::{InventoryInstance, all_inventory_instances, all_inventory_type_schemas};
    use std::sync::Arc;
    use types_registry::config::TypesRegistryConfig;
    use types_registry::domain::local_client::TypesRegistryLocalClient;
    use types_registry::domain::service::TypesRegistryService;
    use types_registry::infra::InMemoryGtsRepository;
    use types_registry_sdk::{InstanceQuery, RegisterResult, TypesRegistryClient};

    const PERMISSION_TYPE_ID: &str = "gts.cf.modkit.authz.permission.v1~";
    #[allow(unknown_lints, de0901_gts_string_pattern)]
    const INSTANCE_PREFIX: &str = "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.";

    /// Expected set of permission instance ids for mini-chat. One entry per
    /// `(resource, action)` tuple the module exposes at PEP call-sites.
    const EXPECTED_PERMISSION_IDS: &[&str] = &[
        "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_create.v1",
        "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_read.v1",
        "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_list.v1",
        "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_update.v1",
        "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_delete.v1",
        "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_list_messages.v1",
        "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_send_message.v1",
        "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_read_turn.v1",
        "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_retry_turn.v1",
        "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_edit_turn.v1",
        "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_delete_turn.v1",
        "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_upload_attachment.v1",
        "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_read_attachment.v1",
        "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_delete_attachment.v1",
        "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_set_reaction.v1",
        "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.chat_delete_reaction.v1",
        "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.model_list.v1",
        "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.model_read.v1",
        "gts.cf.modkit.authz.permission.v1~cf.mini_chat._.user_quota_read.v1",
    ];

    fn mini_chat_permission_instances() -> Vec<&'static InventoryInstance> {
        inventory::iter::<InventoryInstance>
            .into_iter()
            .filter(|e| e.instance_id.starts_with(INSTANCE_PREFIX))
            .collect()
    }

    // =====================================================================
    //           Macro-level sanity (inventory, no types-registry)
    // =====================================================================

    #[test]
    fn all_mini_chat_permissions_are_registered_in_inventory() {
        let entries = mini_chat_permission_instances();
        assert_eq!(
            entries.len(),
            19,
            "expected 19 mini-chat permission instances; found {}: {:?}",
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
    fn permission_resource_types_use_known_wildcards() {
        let known: std::collections::BTreeSet<&'static str> = [
            CHAT_RESOURCE_TYPE_WILDCARD,
            MODEL_RESOURCE_TYPE_WILDCARD,
            USER_QUOTA_RESOURCE_TYPE_WILDCARD,
        ]
        .into_iter()
        .collect();

        for entry in mini_chat_permission_instances() {
            let payload = (entry.payload_fn)();
            let rt = payload["resource_type"]
                .as_str()
                .expect("resource_type string");
            assert!(
                known.contains(rt),
                "permission {} uses unknown resource_type {:?}; expected one of {:?}",
                entry.instance_id,
                rt,
                known
            );
        }
    }

    #[test]
    fn wildcards_cover_runtime_concrete_resource_types() {
        // GTS §3.5 wildcard semantics: `<prefix>.*` matches anything that
        // starts with `<prefix>.`. At evaluation time the PEP sends a
        // concrete type id (from `resources::*.name`); the PDP matches it
        // against the permission's wildcard. Verify that each of our
        // wildcards actually covers the corresponding runtime concrete.
        fn covers(wildcard: &str, concrete: &str) -> bool {
            wildcard
                .strip_suffix('*')
                .is_some_and(|prefix| concrete.starts_with(prefix))
        }

        for (wildcard, concrete, label) in [
            (CHAT_RESOURCE_TYPE_WILDCARD, resources::CHAT.name(), "CHAT"),
            (
                MODEL_RESOURCE_TYPE_WILDCARD,
                resources::MODEL.name(),
                "MODEL",
            ),
            (
                USER_QUOTA_RESOURCE_TYPE_WILDCARD,
                resources::USER_QUOTA.name(),
                "USER_QUOTA",
            ),
        ] {
            assert!(
                covers(wildcard, concrete),
                "{label} wildcard {wildcard:?} must cover runtime concrete {concrete:?} - otherwise a PDP lookup that sees the concrete type won't match this permission"
            );
        }
    }

    #[test]
    fn actions_are_drawn_from_domain_actions_module() {
        let domain_actions: std::collections::BTreeSet<&'static str> = [
            actions::CREATE,
            actions::READ,
            actions::LIST,
            actions::UPDATE,
            actions::DELETE,
            actions::LIST_MESSAGES,
            actions::SEND_MESSAGE,
            actions::READ_TURN,
            actions::RETRY_TURN,
            actions::EDIT_TURN,
            actions::DELETE_TURN,
            actions::UPLOAD_ATTACHMENT,
            actions::READ_ATTACHMENT,
            actions::DELETE_ATTACHMENT,
            actions::SET_REACTION,
            actions::DELETE_REACTION,
        ]
        .into_iter()
        .collect();

        for entry in mini_chat_permission_instances() {
            let payload = (entry.payload_fn)();
            let action = payload["action"].as_str().expect("action string");
            assert!(
                domain_actions.contains(action),
                "{} uses action {:?} not declared in domain::service::actions",
                entry.instance_id,
                action
            );
        }
    }

    // =====================================================================
    //       End-to-end via TypesRegistryClient (SDK trait) —
    //       mirrors the real `types-registry::init()` /
    //       `post_init()` seeding + readiness flow.
    // =====================================================================

    /// Spins up an in-memory `TypesRegistryService`, exposes it as a
    /// `dyn TypesRegistryClient` (SDK trait), and seeds it with every
    /// schema + well-known instance from the process-wide GTS inventory —
    /// including mini-chat's 19 permissions declared via `gts_instance!`.
    /// Then commits readiness (schema validation happens here).
    async fn seed_registry_via_sdk() -> Arc<dyn TypesRegistryClient> {
        let cfg = TypesRegistryConfig::default();
        let repo = Arc::new(InMemoryGtsRepository::new(cfg.to_gts_config()));
        let service = Arc::new(TypesRegistryService::new(repo, cfg));
        let client: Arc<dyn TypesRegistryClient> =
            Arc::new(TypesRegistryLocalClient::new(service.clone()));

        let type_schemas = all_inventory_type_schemas().expect("inventory type schemas collect");
        let instances = all_inventory_instances().expect("inventory instances collect");
        let mut entries = type_schemas;
        entries.extend(instances);

        let results = client.register(entries).await.expect("register() batch");
        RegisterResult::ensure_all_ok(&results)
            .expect("every inventory schema + instance registers cleanly");

        // Readiness commit: runs cross-entity schema validation. This is the
        // moment a malformed permission payload would be rejected.
        service
            .switch_to_ready()
            .expect("registry switches to ready");

        client
    }

    #[tokio::test]
    async fn all_permissions_retrievable_via_registry_client_get() {
        let client = seed_registry_via_sdk().await;

        for id in EXPECTED_PERMISSION_IDS {
            let entity = client
                .get_instance(id)
                .await
                .unwrap_or_else(|e| panic!("get_instance({id}) via registry SDK failed: {e}"));
            assert_eq!(entity.id.as_ref(), *id, "returned entity has wrong id");

            // Payload must carry the four AuthzPermissionV1 fields populated
            // by our typed struct + macro-injected `id`.
            let obj = entity.object.as_object().expect("object is a JSON object");
            assert_eq!(
                obj.get("id").and_then(|v| v.as_str()),
                Some(*id),
                "{id}: payload.id mismatch"
            );
            for field in ["resource_type", "action", "display_name"] {
                let s = obj
                    .get(field)
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| panic!("{id}: missing/non-string `{field}`"));
                assert!(!s.is_empty(), "{id}: `{field}` is empty");
            }
        }
    }

    #[tokio::test]
    async fn permissions_listable_via_registry_client_list_by_pattern() {
        let client = seed_registry_via_sdk().await;

        let listed = client
            .list_instances(InstanceQuery::new().with_pattern(format!("{INSTANCE_PREFIX}*")))
            .await
            .expect("list_instances() via registry SDK");

        let ids: std::collections::BTreeSet<String> =
            listed.iter().map(|e| e.id.as_ref().to_owned()).collect();
        let expected: std::collections::BTreeSet<String> = EXPECTED_PERMISSION_IDS
            .iter()
            .map(|s| (*s).to_owned())
            .collect();

        assert_eq!(
            ids, expected,
            "pattern-list did not return exactly the 19 mini-chat permissions"
        );
    }

    #[tokio::test]
    async fn missing_permission_returns_not_found_via_sdk() {
        let client = seed_registry_via_sdk().await;

        let result = client
            .get_instance("gts.cf.modkit.authz.permission.v1~cf.mini_chat._.nonexistent.v1")
            .await;
        assert!(
            result.is_err() && result.unwrap_err().is_not_found(),
            "unknown permission id must produce NotFound"
        );
    }
}
