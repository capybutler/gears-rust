//! Pin the cursor-walk contract on the static `IdP` plugin's
//! `list_users` so a snapshot larger than `top` stays fully reachable
//! across page hops. Previously the plugin truncated to `top` and
//! signalled `next_cursor: None` unconditionally, silently dropping
//! every row past the first page; the regression guard below pins the
//! new offset-based cursor walk end-to-end.

use std::collections::HashSet;

use account_management_sdk::{
    IdpDeprovisionTenantRequest, IdpDeprovisionUserRequest, IdpListUsersRequest, IdpNewUser,
    IdpPluginClient, IdpProvisionTenantRequest, IdpProvisionUserRequest, IdpTenantContext,
    IdpUserOperationFailure, IdpUserPagination,
};
use modkit_security::SecurityContext;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::domain::Service;

fn ctx() -> SecurityContext {
    SecurityContext::anonymous()
}

const TENANT_TYPE: &str = "gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~";

fn tenant_type() -> gts::GtsSchemaId {
    gts::GtsSchemaId::new(TENANT_TYPE)
}

fn tenant_ctx(tenant_id: Uuid) -> IdpTenantContext {
    IdpTenantContext::new(tenant_id, "static-idp-plugin-test", tenant_type(), None)
}

fn req(tenant_id: Uuid, top: u32, cursor: Option<&str>) -> IdpListUsersRequest {
    let pagination =
        IdpUserPagination::new(top, cursor.map(str::to_owned)).expect("pagination shape is valid");
    IdpListUsersRequest::new(tenant_ctx(tenant_id), pagination)
}

fn seed(svc: &Service, tenant_id: Uuid, count: usize) {
    for i in 0..count {
        let payload = IdpNewUser::new(format!("user-{i:03}"));
        let user = Service::echo_user(tenant_id, &payload);
        svc.record_user(tenant_id, user);
    }
}

#[tokio::test]
async fn empty_snapshot_returns_empty_page_without_cursors() {
    let svc = Service::new();
    let page = svc
        .list_users(&ctx(), &req(Uuid::new_v4(), 50, None))
        .await
        .expect("empty list");
    assert!(page.items.is_empty());
    assert!(page.page_info.next_cursor.is_none());
    assert!(page.page_info.prev_cursor.is_none());
}

#[tokio::test]
async fn page_size_at_least_snapshot_returns_one_page_no_next() {
    let svc = Service::new();
    let tenant = Uuid::new_v4();
    seed(&svc, tenant, 3);
    let page = svc
        .list_users(&ctx(), &req(tenant, 10, None))
        .await
        .expect("page");
    assert_eq!(page.items.len(), 3);
    assert!(page.page_info.next_cursor.is_none());
    assert!(page.page_info.prev_cursor.is_none());
}

#[tokio::test]
async fn cursor_walk_covers_full_snapshot_without_loss_or_duplication() {
    let svc = Service::new();
    let tenant = Uuid::new_v4();
    seed(&svc, tenant, 7);

    let top = 3;
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut cursor: Option<String> = None;
    let mut pages: usize = 0;
    loop {
        let page = svc
            .list_users(&ctx(), &req(tenant, top, cursor.as_deref()))
            .await
            .expect("paged list");
        assert!(
            !page.items.is_empty(),
            "every page in the walk MUST carry at least one row"
        );
        for user in &page.items {
            assert!(
                seen.insert(user.id),
                "cursor walk produced a duplicate user id {} across pages",
                user.id,
            );
        }
        pages += 1;
        match page.page_info.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
        assert!(pages < 10, "cursor walk failed to terminate");
    }
    assert_eq!(seen.len(), 7, "cursor walk MUST surface every seeded user");
    assert_eq!(pages, 3, "7 rows at top=3 -> 3 pages (3 + 3 + 1)");
}

#[tokio::test]
async fn over_paginated_cursor_returns_empty_page_with_no_cursors() {
    // A cursor past `total` MUST collapse to a clean terminator:
    // empty items, no `next_cursor`, no `prev_cursor`. Previously the
    // empty page still emitted a backwards token because `start > 0`
    // was the only guard; a client that blindly followed it would
    // walk backwards from "past the end" instead of stopping.
    let svc = Service::new();
    let tenant = Uuid::new_v4();
    seed(&svc, tenant, 3);
    let page = svc
        .list_users(&ctx(), &req(tenant, 2, Some("99")))
        .await
        .expect("over-paginated cursor must still succeed");
    assert!(
        page.items.is_empty(),
        "over-paginated cursor MUST yield zero rows"
    );
    assert!(
        page.page_info.next_cursor.is_none(),
        "over-paginated page MUST NOT carry a forward cursor"
    );
    assert!(
        page.page_info.prev_cursor.is_none(),
        "over-paginated page MUST NOT carry a backwards cursor (empty terminator)"
    );
}

#[tokio::test]
async fn invalid_cursor_surfaces_as_rejected() {
    let svc = Service::new();
    let tenant = Uuid::new_v4();
    seed(&svc, tenant, 1);
    let err = svc
        .list_users(&ctx(), &req(tenant, 10, Some("not-a-number")))
        .await
        .expect_err("non-decimal cursor MUST be rejected");
    assert!(
        matches!(err, IdpUserOperationFailure::Rejected { .. }),
        "expected Rejected on malformed cursor, got {err:?}",
    );
}

// ── provision_tenant ──────────────────────────────────────────────────

#[tokio::test]
async fn provision_tenant_root_returns_echo_metadata() {
    let svc = Service::new();
    let tenant_id = Uuid::new_v4();
    let request = IdpProvisionTenantRequest::for_root(tenant_id, "root-corp", tenant_type());

    let result = svc
        .provision_tenant(&ctx(), &request)
        .await
        .expect("provision ok");
    let metadata = result
        .metadata
        .expect("provision_tenant MUST emit Some metadata");

    assert_eq!(metadata["echo"], json!(true));
    assert_eq!(metadata["tenant_id"], json!(tenant_id));
    assert_eq!(metadata["tenant_name"], json!("root-corp"));
    assert_eq!(metadata["tenant_type"], json!(TENANT_TYPE));
    assert_eq!(metadata["target"], json!("root"));
    assert_eq!(metadata["parent_id"], Value::Null);
    assert_eq!(metadata["provisioning_metadata"], Value::Null);
}

#[tokio::test]
async fn provision_tenant_child_carries_parent_id_and_echoed_provisioning_metadata() {
    let svc = Service::new();
    let tenant_id = Uuid::new_v4();
    let parent_id = Uuid::new_v4();
    let request = IdpProvisionTenantRequest::new(tenant_id, parent_id, "acme", tenant_type())
        .with_metadata(json!({"realm": "acme-keycloak", "region": "eu-west-1"}));

    let result = svc
        .provision_tenant(&ctx(), &request)
        .await
        .expect("provision ok");
    let metadata = result
        .metadata
        .expect("provision_tenant MUST emit Some metadata");

    assert_eq!(metadata["target"], json!("child"));
    assert_eq!(metadata["parent_id"], json!(parent_id));
    assert_eq!(
        metadata["provisioning_metadata"],
        json!({"realm": "acme-keycloak", "region": "eu-west-1"}),
        "provisioning_metadata MUST be echoed verbatim",
    );
}

#[tokio::test]
async fn provision_tenant_is_deterministic_across_invocations() {
    let svc = Service::new();
    let tenant_id = Uuid::new_v4();
    let parent_id = Uuid::new_v4();
    let request = IdpProvisionTenantRequest::new(tenant_id, parent_id, "acme", tenant_type());

    let a = svc.provision_tenant(&ctx(), &request).await.expect("first");
    let b = svc
        .provision_tenant(&ctx(), &request)
        .await
        .expect("second");
    assert_eq!(
        a.metadata, b.metadata,
        "echo metadata MUST be a pure function of the input request"
    );
}

// ── deprovision_tenant ────────────────────────────────────────────────

#[tokio::test]
async fn deprovision_tenant_always_succeeds() {
    let svc = Service::new();
    let request = IdpDeprovisionTenantRequest::new(tenant_ctx(Uuid::new_v4()));
    svc.deprovision_tenant(&ctx(), &request)
        .await
        .expect("deprovision MUST succeed");
}

// ── provision_user ────────────────────────────────────────────────────

#[tokio::test]
async fn provision_user_records_user_and_returns_deterministic_id() {
    let svc = Service::new();
    let tenant_id = Uuid::new_v4();
    let payload = IdpNewUser::new("alice")
        .with_email("alice@example.com")
        .with_display_name("Alice");
    let request = IdpProvisionUserRequest::new(tenant_ctx(tenant_id), payload);

    let user_a = svc.provision_user(&ctx(), &request).await.expect("first");
    let user_b = svc.provision_user(&ctx(), &request).await.expect("second");

    assert_eq!(user_a.id, user_b.id, "same input MUST yield same UUIDv5");
    assert_eq!(user_a.username, "alice");
    assert_eq!(user_a.email.as_deref(), Some("alice@example.com"));
    assert_eq!(user_a.display_name.as_deref(), Some("Alice"));

    // The user must be observable through list_users.
    let page = svc
        .list_users(&ctx(), &req(tenant_id, 10, None))
        .await
        .expect("list");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, user_a.id);
}

#[tokio::test]
async fn provision_user_different_tenants_yield_different_ids() {
    let svc = Service::new();
    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();
    let payload = IdpNewUser::new("alice");
    let ua = svc
        .provision_user(
            &ctx(),
            &IdpProvisionUserRequest::new(tenant_ctx(tenant_a), payload.clone()),
        )
        .await
        .expect("a");
    let ub = svc
        .provision_user(
            &ctx(),
            &IdpProvisionUserRequest::new(tenant_ctx(tenant_b), payload),
        )
        .await
        .expect("b");
    assert_ne!(
        ua.id, ub.id,
        "tenant scope MUST namespace the derived user id"
    );
}

#[tokio::test]
async fn provision_user_re_provision_overwrites_with_new_payload() {
    let svc = Service::new();
    let tenant_id = Uuid::new_v4();
    let req_one = IdpProvisionUserRequest::new(
        tenant_ctx(tenant_id),
        IdpNewUser::new("bob").with_email("bob@old.example.com"),
    );
    let req_two = IdpProvisionUserRequest::new(
        tenant_ctx(tenant_id),
        IdpNewUser::new("bob")
            .with_email("bob@new.example.com")
            .with_display_name("Bob"),
    );

    let first = svc.provision_user(&ctx(), &req_one).await.expect("first");
    let second = svc.provision_user(&ctx(), &req_two).await.expect("second");
    assert_eq!(first.id, second.id);

    let page = svc
        .list_users(&ctx(), &req(tenant_id, 10, None))
        .await
        .expect("list");
    assert_eq!(
        page.items.len(),
        1,
        "re-provision MUST overwrite, not append"
    );
    assert_eq!(
        page.items[0].email.as_deref(),
        Some("bob@new.example.com"),
        "post-overwrite snapshot MUST reflect the new payload"
    );
    assert_eq!(page.items[0].display_name.as_deref(), Some("Bob"));
}

// ── deprovision_user ──────────────────────────────────────────────────

#[tokio::test]
async fn deprovision_user_removes_existing_user() {
    let svc = Service::new();
    let tenant_id = Uuid::new_v4();
    let payload = IdpNewUser::new("carol");
    let user = svc
        .provision_user(
            &ctx(),
            &IdpProvisionUserRequest::new(tenant_ctx(tenant_id), payload),
        )
        .await
        .expect("provision");

    svc.deprovision_user(
        &ctx(),
        &IdpDeprovisionUserRequest::new(tenant_ctx(tenant_id), user.id),
    )
    .await
    .expect("deprovision");

    let page = svc
        .list_users(&ctx(), &req(tenant_id, 10, None))
        .await
        .expect("list");
    assert!(
        page.items.is_empty(),
        "deprovision MUST remove the row from the per-tenant cache"
    );
}

#[tokio::test]
async fn deprovision_user_is_idempotent_when_already_absent() {
    let svc = Service::new();
    let tenant_id = Uuid::new_v4();
    // Never provisioned — the call still resolves to Ok per the SDK
    // contract (`removed` and `already-absent` are both success).
    svc.deprovision_user(
        &ctx(),
        &IdpDeprovisionUserRequest::new(tenant_ctx(tenant_id), Uuid::new_v4()),
    )
    .await
    .expect("absent deprovision MUST be Ok");
}

// ── user_id_filter existence-check ────────────────────────────────────

#[tokio::test]
async fn list_users_with_user_id_filter_returns_single_row_or_empty() {
    let svc = Service::new();
    let tenant_id = Uuid::new_v4();
    let user = svc
        .provision_user(
            &ctx(),
            &IdpProvisionUserRequest::new(tenant_ctx(tenant_id), IdpNewUser::new("dave")),
        )
        .await
        .expect("provision");

    // Hit: filter on the known id.
    let hit_pagination = IdpUserPagination::new(50, None).expect("pagination");
    let hit = svc
        .list_users(
            &ctx(),
            &IdpListUsersRequest::new(tenant_ctx(tenant_id), hit_pagination)
                .with_user_id_filter(user.id),
        )
        .await
        .expect("filtered list hit");
    assert_eq!(hit.items.len(), 1);
    assert_eq!(hit.items[0].id, user.id);

    // Miss: filter on an unknown id.
    let miss_pagination = IdpUserPagination::new(50, None).expect("pagination");
    let miss = svc
        .list_users(
            &ctx(),
            &IdpListUsersRequest::new(tenant_ctx(tenant_id), miss_pagination)
                .with_user_id_filter(Uuid::new_v4()),
        )
        .await
        .expect("filtered list miss");
    assert!(
        miss.items.is_empty(),
        "user_id_filter on absent id MUST surface an empty page"
    );
}
