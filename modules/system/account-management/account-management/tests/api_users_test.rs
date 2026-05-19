//! HTTP-level E2E tests for the
//! `/account-management/v1/tenants/{tenant_id}/users*` REST surface.
//!
//! Scope: provision / list / deprovision through the real router
//! against the in-memory `FakeIdpPlugin` echo. Service-side username
//! validation and IdP failure mapping are pinned by
//! `domain::user::service_tests`.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::doc_markdown
)]

mod common;

use axum::http::StatusCode;
use tower::ServiceExt;
use uuid::Uuid;

use common::*;

fn build_users_router(h: &Harness) -> axum::Router {
    // `create_user` is fail-closed on a missing `gts.cf.core.am.user.v1~`
    // schema, so the users tests use the user-aware variant of the
    // types-registry helper.
    let services = build_services_full(
        h,
        fake_idp(),
        empty_metadata_registry(),
        types_registry_for_users(),
    );
    build_test_router(&services)
}

// ─── POST /tenants/{id}/users ────────────────────────────────────────

#[tokio::test]
async fn provision_user_returns_201_with_idp_user_dto() {
    let h = setup_sqlite().await.expect("sqlite");
    let root = Uuid::new_v4();
    seed_root(&h, root).await;
    let router = build_users_router(&h);

    let body = serde_json::json!({"username": "alice"});
    let req = json_request(
        "POST",
        &format!("/account-management/v1/tenants/{root}/users"),
        Some(body),
        ctx_for(root),
    );
    let resp = router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = response_body(resp).await;
    assert_eq!(body["username"], "alice");
    assert!(body["id"].is_string(), "id must be present: {body}");
}

#[tokio::test]
async fn provision_user_with_full_profile_returns_201() {
    let h = setup_sqlite().await.expect("sqlite");
    let root = Uuid::new_v4();
    seed_root(&h, root).await;
    let router = build_users_router(&h);

    let body = serde_json::json!({
        "username": "bob",
        "email": "bob@example.com",
        "display_name": "Bob Q.",
    });
    let req = json_request(
        "POST",
        &format!("/account-management/v1/tenants/{root}/users"),
        Some(body),
        ctx_for(root),
    );
    let resp = router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = response_body(resp).await;
    assert_eq!(body["username"], "bob");
    assert_eq!(body["email"], "bob@example.com");
    assert_eq!(body["display_name"], "Bob Q.");
}

// ─── DELETE /tenants/{id}/users/{user_id} ────────────────────────────

#[tokio::test]
async fn deprovision_user_returns_204_no_content() {
    let h = setup_sqlite().await.expect("sqlite");
    let root = Uuid::new_v4();
    seed_root(&h, root).await;
    let router = build_users_router(&h);

    let some_user = Uuid::new_v4();
    let req = json_request(
        "DELETE",
        &format!("/account-management/v1/tenants/{root}/users/{some_user}"),
        None,
        ctx_for(root),
    );
    let resp = router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn deprovision_user_already_absent_returns_204_idempotent() {
    // Per `cpt-cf-account-management-algo-idp-user-operations-contract-deprovision-idempotency-guard`:
    // a second DELETE on a user the IdP already considers absent must
    // still surface 204. The stateful `FakeIdpPlugin::deprovision_user`
    // maps both removed-and-already-absent to `Ok(())` per the SDK
    // trait contract, so two consecutive DELETEs on the same id both
    // see 204 regardless of whether the row was ever provisioned.
    let h = setup_sqlite().await.expect("sqlite");
    let root = Uuid::new_v4();
    seed_root(&h, root).await;
    let router = build_users_router(&h);

    let some_user = Uuid::new_v4();
    let path = format!("/account-management/v1/tenants/{root}/users/{some_user}");

    let req = json_request("DELETE", &path, None, ctx_for(root));
    let resp = router.clone().oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let req = json_request("DELETE", &path, None, ctx_for(root));
    let resp = router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

// ─── GET /tenants/{id}/users ─────────────────────────────────────────

#[tokio::test]
async fn list_users_returns_200_with_page() {
    let h = setup_sqlite().await.expect("sqlite");
    let root = Uuid::new_v4();
    seed_root(&h, root).await;
    let router = build_users_router(&h);

    let req = json_request(
        "GET",
        &format!("/account-management/v1/tenants/{root}/users"),
        None,
        ctx_for(root),
    );
    let resp = router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = response_body(resp).await;
    assert!(body["items"].is_array(), "items must be an array: {body}");
    assert!(
        body["page_info"].is_object(),
        "page_info must be an object: {body}",
    );
}

#[tokio::test]
async fn list_users_filtered_by_user_id_returns_200() {
    // Per the handler's `pagination_for(query)`: when `?user_id=X` is
    // supplied the handler pins `top=1, cursor=None` BEFORE calling
    // the service. With the stateful fake the unknown-uid filter
    // returns an empty page (authoritative absent signal per FEATURE
    // §5.5 DoD); the populated-uid filter is exercised by
    // `user_lifecycle_round_trip_against_stateful_fake` below.
    let h = setup_sqlite().await.expect("sqlite");
    let root = Uuid::new_v4();
    seed_root(&h, root).await;
    let router = build_users_router(&h);

    let probe = Uuid::new_v4();
    let req = json_request(
        "GET",
        &format!("/account-management/v1/tenants/{root}/users?user_id={probe}"),
        None,
        ctx_for(root),
    );
    let resp = router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = response_body(resp).await;
    let items = body["items"].as_array().expect("items array");
    assert!(items.is_empty(), "unknown user_id MUST return empty page");
}

#[tokio::test]
async fn user_lifecycle_round_trip_against_stateful_fake() {
    // End-to-end coverage for the create → list → list-filtered →
    // delete → list-empty round-trip against the stateful in-memory
    // IdP fake. Pre-fix the harness's `FakeIdpPlugin::list_users`
    // returned `Page::empty(50)` and `deprovision_user` silently
    // ignored its argument, so regressions in user_id filtering,
    // list-after-create visibility, or delete cleanup could ship
    // green.
    let h = setup_sqlite().await.expect("sqlite");
    let root = Uuid::new_v4();
    seed_root(&h, root).await;
    let router = build_users_router(&h);

    // POST /users — provision two users.
    for username in ["alice", "bob"] {
        let req = json_request(
            "POST",
            &format!("/account-management/v1/tenants/{root}/users"),
            Some(serde_json::json!({ "username": username })),
            ctx_for(root),
        );
        let resp = router.clone().oneshot(req).await.expect("router");
        assert_eq!(resp.status(), StatusCode::CREATED, "create {username}");
    }

    // GET /users — both visible.
    let req = json_request(
        "GET",
        &format!("/account-management/v1/tenants/{root}/users"),
        None,
        ctx_for(root),
    );
    let resp = router.clone().oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = response_body(resp).await;
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2, "post-create list MUST surface both users");

    let alice_id: Uuid = items
        .iter()
        .find(|u| u["username"] == "alice")
        .and_then(|u| u["id"].as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
        .expect("alice id");

    // GET /users?user_id=<alice> — point-lookup returns exactly one.
    let req = json_request(
        "GET",
        &format!("/account-management/v1/tenants/{root}/users?user_id={alice_id}"),
        None,
        ctx_for(root),
    );
    let resp = router.clone().oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = response_body(resp).await;
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1, "filtered list MUST return one row");
    assert_eq!(items[0]["username"], "alice");

    // DELETE /users/<alice> — 204.
    let req = json_request(
        "DELETE",
        &format!("/account-management/v1/tenants/{root}/users/{alice_id}"),
        None,
        ctx_for(root),
    );
    let resp = router.clone().oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // GET /users?user_id=<alice> — empty after delete.
    let req = json_request(
        "GET",
        &format!("/account-management/v1/tenants/{root}/users?user_id={alice_id}"),
        None,
        ctx_for(root),
    );
    let resp = router.clone().oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = response_body(resp).await;
    let items = body["items"].as_array().expect("items array");
    assert!(
        items.is_empty(),
        "alice MUST be gone after delete; got {items:?}",
    );

    // GET /users — bob still visible.
    let req = json_request(
        "GET",
        &format!("/account-management/v1/tenants/{root}/users"),
        None,
        ctx_for(root),
    );
    let resp = router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = response_body(resp).await;
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1, "bob remains after alice's delete");
    assert_eq!(items[0]["username"], "bob");
}

// ─── Tenant existence ────────────────────────────────────────────────

#[tokio::test]
async fn list_users_for_unknown_tenant_returns_404() {
    let h = setup_sqlite().await.expect("sqlite");
    let root = Uuid::new_v4();
    seed_root(&h, root).await;
    let router = build_users_router(&h);

    let unknown = Uuid::new_v4();
    let req = json_request(
        "GET",
        &format!("/account-management/v1/tenants/{unknown}/users"),
        None,
        ctx_for(root),
    );
    let resp = router.oneshot(req).await.expect("router");
    let (status, _body) = response_problem(resp).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
