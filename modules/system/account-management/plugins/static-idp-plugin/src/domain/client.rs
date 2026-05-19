//! `IdpPluginClient` impl — kept separate from service.rs so domain state (Service) is reviewable independently of the SDK contract glue.

use async_trait::async_trait;
use modkit_odata::Page;
use modkit_security::SecurityContext;

use account_management_sdk::{
    IdpDeprovisionFailure, IdpDeprovisionTenantRequest, IdpDeprovisionUserRequest,
    IdpListUsersRequest, IdpPluginClient, IdpProvisionFailure, IdpProvisionResult,
    IdpProvisionTenantRequest, IdpProvisionUserRequest, IdpUser, IdpUserOperationFailure,
};

use super::service::Service;

#[async_trait]
impl IdpPluginClient for Service {
    async fn provision_tenant(
        &self,
        _ctx: &SecurityContext,
        req: &IdpProvisionTenantRequest,
    ) -> Result<IdpProvisionResult, IdpProvisionFailure> {
        // Return non-empty deterministic metadata so AM's `Some` arm
        // in `activate_tenant` / `upsert_idp_metadata` is exercised by
        // every E2E flow that wires this plugin. A real provider would
        // place vendor-issued identifiers here; the echo plugin
        // surfaces a pure-function projection of the request inputs.
        Ok(IdpProvisionResult::new(Some(Self::echo_tenant_metadata(
            req,
        ))))
    }

    async fn deprovision_tenant(
        &self,
        _ctx: &SecurityContext,
        _req: &IdpDeprovisionTenantRequest,
    ) -> Result<(), IdpDeprovisionFailure> {
        Ok(())
    }

    async fn provision_user(
        &self,
        _ctx: &SecurityContext,
        req: &IdpProvisionUserRequest,
    ) -> Result<IdpUser, IdpUserOperationFailure> {
        let tenant_id = req.tenant_context.tenant_id;
        let user = Self::echo_user(tenant_id, &req.payload);
        self.record_user(tenant_id, user.clone());
        Ok(user)
    }

    async fn deprovision_user(
        &self,
        _ctx: &SecurityContext,
        req: &IdpDeprovisionUserRequest,
    ) -> Result<(), IdpUserOperationFailure> {
        // Both `removed` and `already-absent` are success per the trait
        // doc on `deprovision_user`; AM does not distinguish them.
        let _ = self.forget_user(req.tenant_context.tenant_id, req.user_id);
        Ok(())
    }

    async fn list_users(
        &self,
        _ctx: &SecurityContext,
        req: &IdpListUsersRequest,
    ) -> Result<Page<IdpUser>, IdpUserOperationFailure> {
        // The per-tenant snapshot is a `HashMap` view; iteration order
        // is non-deterministic. Sort by `user.id` so the cursor
        // (offset-based) is stable across calls — a paginated client
        // walking the snapshot must observe a deterministic sequence,
        // otherwise the same row could surface on two pages or be
        // skipped entirely.
        let mut snapshot = self.snapshot_users(req.tenant_context.tenant_id, req.user_id_filter);
        snapshot.sort_by_key(|u| u.id);

        // Opaque offset-based cursor: the wire token is the decimal
        // offset into the sorted snapshot for the NEXT page. Robust
        // enough for a dev plugin (real providers use vendor-native
        // tokens / `next_token` / `last_id` keys, which AM forwards
        // unchanged per the [`IdpUserPagination::cursor`] contract).
        // A non-numeric cursor surfaces as a `Validation` failure so a
        // hostile / buggy client cannot smuggle arbitrary state into
        // the plugin's request handler.
        let offset: usize = match req.pagination.cursor() {
            None => 0,
            Some(raw) => raw
                .parse::<usize>()
                .map_err(|err| IdpUserOperationFailure::Rejected {
                    detail: format!(
                        "static-idp-plugin: cursor must be a non-negative decimal offset \
                         into the per-tenant user snapshot (got {raw:?}): {err}"
                    ),
                })?,
        };
        let total = snapshot.len();
        let top = req.pagination.top() as usize;
        let start = offset.min(total);
        let end = start.saturating_add(top).min(total);
        let items: Vec<IdpUser> = snapshot.drain(start..end).collect();
        let next_cursor = if end < total {
            Some(end.to_string())
        } else {
            None
        };
        // `prev_cursor` is suppressed when the current page is empty.
        // Without this guard an over-paginated cursor (`offset > total`)
        // would yield `items = []` yet emit a backwards token, leaving a
        // client that blindly follows `prev_cursor` jumping back from
        // "past the end" instead of receiving a clean empty terminator.
        let prev_cursor = if start > 0 && !items.is_empty() {
            let prev = start.saturating_sub(top);
            Some(prev.to_string())
        } else {
            None
        };
        Ok(Page::new(
            items,
            modkit_odata::PageInfo {
                next_cursor,
                prev_cursor,
                limit: u64::from(req.pagination.top()),
            },
        ))
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod pagination_tests;
