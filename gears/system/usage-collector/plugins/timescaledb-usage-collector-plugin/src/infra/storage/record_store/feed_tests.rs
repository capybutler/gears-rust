//! Unit tests for the feed page orchestration's pure half.
//!
//! Steps 2 to 4 themselves need an open snapshot transaction, so they are
//! pinned against a live backend by `tests/feed_page_integration_pg.rs`; what
//! is testable without one is the subscription dedup every page statement binds.

use super::dedup_subscription_types;

#[test]
fn feed_subscription_type_dedup_preserves_first_seen_order() {
    let a = uuid::Uuid::from_u128(1);
    let b = uuid::Uuid::from_u128(2);
    let c = uuid::Uuid::from_u128(3);

    assert_eq!(dedup_subscription_types(&[b, a, b, c, a]), vec![b, a, c]);
}
