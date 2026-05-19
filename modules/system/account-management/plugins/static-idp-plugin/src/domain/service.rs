//! Domain service for the static `IdP` plugin.
//!
//! In-memory echo: every operation succeeds with a deterministic
//! payload derived from its inputs. Provisioned users are retained in
//! a per-tenant `HashMap` so `list_users` and the
//! `?user_id_filter=<id>` existence-check shape both observe what
//! `provision_user` just wrote -- a real provider would expose the
//! same lifecycle. State lives in-process and is dropped on restart,
//! matching the dev-only contract of every other `static-*-plugin`.

use std::collections::HashMap;

use modkit_macros::domain_model;
use parking_lot::Mutex;
use serde_json::{Value, json};
use uuid::Uuid;

use account_management_sdk::{IdpNewUser, IdpProvisionTarget, IdpProvisionTenantRequest, IdpUser};

/// In-memory per-tenant user cache that backs every `IdpPluginClient` method on this plugin.
#[domain_model]
pub struct Service {
    users: Mutex<HashMap<Uuid, HashMap<Uuid, IdpUser>>>,
}

impl Service {
    #[must_use]
    pub fn new() -> Self {
        Self {
            users: Mutex::new(HashMap::new()),
        }
    }

    /// Build the deterministic echo payload returned from
    /// `provision_tenant`'s `IdpProvisionResult::metadata`.
    ///
    /// Surfaces every field a real provider would normally bind to the
    /// tenant -- `tenant_id`, `tenant_name`, chained `tenant_type`,
    /// the `root` / `child` discriminator (with `parent_id` on the
    /// child arm), and any client-supplied `provisioning_metadata`
    /// echoed back verbatim. The shape is pure-function of the input
    /// request so cross-restart E2E suites can pin byte-for-byte
    /// equality, and AM's `Some(metadata)` activation branch is
    /// exercised on every create-tenant flow.
    #[must_use]
    pub fn echo_tenant_metadata(req: &IdpProvisionTenantRequest) -> Value {
        // `IdpProvisionTarget` is `#[non_exhaustive]`; the wildcard
        // arm collapses any future variant to the `Child` shape with a
        // null `parent_id` so additions to the SDK enum cannot break
        // the echo plugin until it is intentionally taught about them.
        let (target, parent_id) = match &req.target {
            IdpProvisionTarget::Root => ("root", Value::Null),
            IdpProvisionTarget::Child { parent_id } => ("child", json!(parent_id)),
            _ => ("unknown", Value::Null),
        };
        json!({
            "echo": true,
            "tenant_id": req.tenant_id,
            "tenant_name": req.name,
            "tenant_type": req.tenant_type.as_ref(),
            "target": target,
            "parent_id": parent_id,
            "provisioning_metadata": req.metadata.clone().unwrap_or(Value::Null),
        })
    }

    /// Build the echo `IdpUser` returned from `provision_user`.
    ///
    /// The IdP-issued `id` is derived deterministically from the
    /// tenant scope and the supplied username via `UUIDv5` so repeated
    /// calls for the same `(tenant_id, username)` pair return the
    /// same identifier. This matches what real providers expose
    /// (stable user UUID per tenant scope) and keeps E2E assertions
    /// reproducible across server restarts.
    #[must_use]
    pub fn echo_user(tenant_id: Uuid, payload: &IdpNewUser) -> IdpUser {
        let namespace = Uuid::new_v5(&Uuid::NAMESPACE_DNS, tenant_id.as_bytes());
        let id = Uuid::new_v5(&namespace, payload.username.as_bytes());

        let mut user = IdpUser::new(id, payload.username.clone());
        if let Some(email) = &payload.email {
            user = user.with_email(email.clone());
        }
        if let Some(display_name) = &payload.display_name {
            user = user.with_display_name(display_name.clone());
        }
        user
    }

    /// Record a provisioned user in the per-tenant cache and return
    /// the projection AM consumers see. Idempotent: re-provisioning
    /// the same `(tenant_id, username)` pair overwrites the previous
    /// row with the freshly-echoed payload.
    pub fn record_user(&self, tenant_id: Uuid, user: IdpUser) {
        self.users
            .lock()
            .entry(tenant_id)
            .or_default()
            .insert(user.id, user);
    }

    /// Forget a user in this tenant scope. Returns `true` if the user
    /// was present, mirroring the "removed vs already-absent"
    /// distinction the `IdP` contract does NOT propagate to AM (AM
    /// treats both as success).
    pub fn forget_user(&self, tenant_id: Uuid, user_id: Uuid) -> bool {
        let mut guard = self.users.lock();
        let Some(scope) = guard.get_mut(&tenant_id) else {
            return false;
        };
        let removed = scope.remove(&user_id).is_some();
        if scope.is_empty() {
            guard.remove(&tenant_id);
        }
        removed
    }

    /// Snapshot the per-tenant user map for listing. Optionally
    /// narrows to a single `user_id` so the existence-check shape
    /// (`?user_id_filter=<id>`) returns either an empty list or a
    /// single-element list.
    #[must_use]
    pub fn snapshot_users(&self, tenant_id: Uuid, user_id_filter: Option<Uuid>) -> Vec<IdpUser> {
        let guard = self.users.lock();
        let Some(scope) = guard.get(&tenant_id) else {
            return Vec::new();
        };
        match user_id_filter {
            Some(uid) => scope.get(&uid).cloned().into_iter().collect(),
            None => scope.values().cloned().collect(),
        }
    }
}

impl Default for Service {
    fn default() -> Self {
        Self::new()
    }
}
