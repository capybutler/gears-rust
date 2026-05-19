//! Handler-side unit tests for the user-ops REST surface.
//!
//! Scope: pin the [`super::pagination_for`] override branches that
//! make the documented `GET /tenants/{id}/users?user_id=<X>` happy
//! path work without the caller having to know about
//! `UserService::list_users`' `top == 1` invariant. Service-level
//! pins (see `domain::user::service_tests`) cover the reject-side of
//! the same invariant; this file covers the handler-side route-around.

use account_management_sdk::IdpUserPagination;
use uuid::Uuid;

use super::{ListUsersQuery, pagination_for};
use crate::domain::error::DomainError;

fn user_id() -> Uuid {
    Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap()
}

#[test]
fn pagination_for_unfiltered_default_keeps_default_top_and_no_cursor() {
    // Sanity-check the unfiltered branch: omitting both `limit` and
    // `cursor` yields the documented default page (`DEFAULT_TOP=50`,
    // no cursor) and reaches the service unchanged.
    let query = ListUsersQuery {
        user_id: None,
        limit: None,
        cursor: None,
    };
    let pagination =
        pagination_for(&query, IdpUserPagination::MAX_TOP).expect("default unfiltered is valid");
    assert_eq!(pagination.top(), IdpUserPagination::DEFAULT_TOP);
    assert_eq!(pagination.cursor(), None);
}

#[test]
fn pagination_for_unfiltered_forwards_caller_limit_and_cursor() {
    // Unfiltered listing carries the caller's pagination knobs
    // verbatim; the C1 override only applies to the filtered shape.
    let query = ListUsersQuery {
        user_id: None,
        limit: Some(10),
        cursor: Some("opaque-token".to_owned()),
    };
    let pagination =
        pagination_for(&query, IdpUserPagination::MAX_TOP).expect("explicit unfiltered is valid");
    assert_eq!(pagination.top(), 10);
    assert_eq!(pagination.cursor(), Some("opaque-token"));
}

#[test]
fn pagination_for_filtered_pins_top_to_one_when_caller_omits_limit() {
    // The documented happy path for the existence check:
    // `GET ?user_id=<X>` with no `limit`. Without the handler-side
    // override, this would surface as 400 because `UserService::list_users`
    // rejects `top != 1` for filtered calls. After the override the
    // service sees `top=1` and the spec-compliant client never has
    // to know `top=1` is required.
    let query = ListUsersQuery {
        user_id: Some(user_id()),
        limit: None,
        cursor: None,
    };
    let pagination =
        pagination_for(&query, IdpUserPagination::MAX_TOP).expect("filtered default is valid");
    assert_eq!(pagination.top(), 1);
    assert_eq!(pagination.cursor(), None);
}

#[test]
fn pagination_for_filtered_with_limit_is_rejected_as_validation() {
    // Regression guard: the pre-fix handler silently overrode
    // caller-supplied `limit` when combined with `user_id`. That hid
    // a caller misunderstanding behind a successful response. The
    // handler now rejects the combination with `Validation` (HTTP
    // 400) so the client sees the constraint loudly.
    let query = ListUsersQuery {
        user_id: Some(user_id()),
        limit: Some(10),
        cursor: None,
    };
    let err = pagination_for(&query, IdpUserPagination::MAX_TOP)
        .expect_err("user_id + limit MUST be rejected");
    let DomainError::Validation { detail } = err else {
        panic!("expected DomainError::Validation, got `{err:?}`");
    };
    assert!(
        detail.contains("user_id") && detail.contains("limit"),
        "detail must name both rejected fields: `{detail}`",
    );
}

#[test]
fn pagination_for_filtered_with_cursor_is_rejected_as_validation() {
    // Same regression guard as the `limit` companion: a continuation
    // token on a filtered call would let the IdP plugin step past
    // the matching row and turn the lookup into a false negative.
    // Pre-fix the handler silently dropped the cursor; it now
    // rejects the combination with 400.
    let query = ListUsersQuery {
        user_id: Some(user_id()),
        limit: None,
        cursor: Some("opaque-token".to_owned()),
    };
    let err = pagination_for(&query, IdpUserPagination::MAX_TOP)
        .expect_err("user_id + cursor MUST be rejected");
    let DomainError::Validation { detail } = err else {
        panic!("expected DomainError::Validation, got `{err:?}`");
    };
    assert!(
        detail.contains("user_id") && detail.contains("cursor"),
        "detail must name both rejected fields: `{detail}`",
    );
}

#[test]
fn pagination_for_unfiltered_oversized_top_is_clamped_not_rejected() {
    // Pre-fix the handler propagated `IdpUserPagination::new`'s
    // `TopExceedsMax` constructor rejection as `DomainError::Validation`
    // (HTTP 400) when the caller passed `limit > MAX_TOP`. The
    // operator-cap clamp from finding #10 now subsumes that path:
    // the handler clamps `limit` to `max_top` (defaulting to the
    // SDK's `MAX_TOP = 200`) BEFORE invoking the SDK constructor,
    // so `limit = MAX_TOP + 1` lands as `MAX_TOP` and the call
    // succeeds. This matches the documented "raise the cap silently
    // up to the operator ceiling" semantics used by every other AM
    // listing endpoint (tenant children, conversions, metadata).
    let query = ListUsersQuery {
        user_id: None,
        limit: Some(IdpUserPagination::MAX_TOP + 1),
        cursor: None,
    };
    let pagination = pagination_for(&query, IdpUserPagination::MAX_TOP)
        .expect("oversized limit must clamp, not reject");
    assert_eq!(pagination.top(), IdpUserPagination::MAX_TOP);
}

#[test]
fn pagination_for_unfiltered_caller_limit_is_clamped_to_operator_max_top() {
    // Operator-tunable per-deployment cap: a deployment that set
    // `cfg.listing.max_top = 25` MUST see callers receive at most
    // 25 rows even if they requested 200, keeping the user listing
    // consistent with the tenant / metadata / conversion endpoints.
    // Regression guard for finding #10 (inconsistent max_top): the
    // pre-fix handler always honored the caller-supplied `limit` up
    // to the SDK's absolute ceiling (200) and ignored the operator
    // knob.
    let query = ListUsersQuery {
        user_id: None,
        limit: Some(200),
        cursor: None,
    };
    let pagination = pagination_for(&query, 25).expect("clamp is valid");
    assert_eq!(pagination.top(), 25, "caller limit MUST clamp to max_top");
}

#[test]
fn pagination_for_unfiltered_default_top_is_clamped_to_operator_max_top() {
    // Same clamp applies when the caller does NOT supply `limit`:
    // the SDK's `DEFAULT_TOP = 50` must also collapse to the
    // operator cap so a tighter `cfg.listing.max_top` is honored
    // uniformly.
    let query = ListUsersQuery {
        user_id: None,
        limit: None,
        cursor: None,
    };
    let pagination = pagination_for(&query, 10).expect("clamp default is valid");
    assert_eq!(pagination.top(), 10, "DEFAULT_TOP MUST clamp to max_top");
}

#[test]
fn pagination_for_unfiltered_zero_limit_propagates_validation_error() {
    // `top=0` would turn the listing into a false-negative empty
    // page on providers that honor the literal value (see
    // `IdpUserPagination` docs). The SDK constructor rejects it; the
    // handler converts the rejection into 400 at the boundary.
    let query = ListUsersQuery {
        user_id: None,
        limit: Some(0),
        cursor: None,
    };
    let err = pagination_for(&query, IdpUserPagination::MAX_TOP).expect_err("top=0 must fail");
    let DomainError::Validation { detail } = err else {
        panic!("expected DomainError::Validation, got `{err:?}`");
    };
    assert!(
        detail.contains("invalid pagination"),
        "detail must surface the SDK constructor error: `{detail}`",
    );
}
