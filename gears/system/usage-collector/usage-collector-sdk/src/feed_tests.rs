//! Unit tests for the feed types.

use std::collections::HashSet;

use super::{FeedPosition, FeedPositionInvalid, MAX_FEED_POSITION_BYTES};

#[test]
fn a_position_at_the_byte_bound_is_accepted() {
    let bytes = vec![7_u8; MAX_FEED_POSITION_BYTES];

    let position = FeedPosition::new(bytes.clone()).expect("the bound itself is admissible");

    assert_eq!(position.as_bytes(), &bytes[..]);
    assert_eq!(position.len(), MAX_FEED_POSITION_BYTES);
    assert!(
        !position.is_empty(),
        "new() never admits an empty sequence, so a built position is never empty"
    );
}

#[test]
fn a_position_one_byte_over_the_bound_is_refused() {
    let bytes = vec![7_u8; MAX_FEED_POSITION_BYTES + 1];

    let refused = FeedPosition::new(bytes).expect_err(
        "a position past the bound cannot fit the wire cursor, so it must be refused where it \
         is built rather than where it is encoded",
    );

    assert!(matches!(
        refused,
        FeedPositionInvalid::TooLarge { actual } if actual == MAX_FEED_POSITION_BYTES + 1
    ));
}

#[test]
fn an_empty_position_is_refused() {
    let refused = FeedPosition::new(Vec::new()).expect_err(
        "a position is a point a plugin issued; zero bytes is a plugin that forgot to fill it, \
         not an encoding",
    );

    assert!(matches!(refused, FeedPositionInvalid::Empty));
}

#[test]
fn positions_compare_and_hash_equal_exactly_when_their_bytes_are_equal() {
    let one = FeedPosition::new(vec![0, 1]).expect("two bytes are admissible");
    let same_bytes = FeedPosition::new(vec![0, 1]).expect("same bytes");
    let different_bytes = FeedPosition::new(vec![0, 2]).expect("two bytes are admissible");

    assert_eq!(one, same_bytes);
    assert_ne!(one, different_bytes);

    let mut positions = HashSet::new();
    positions.insert(one);
    assert!(
        positions.contains(&same_bytes),
        "equal positions must hash equally to work as a map key"
    );
    assert!(!positions.contains(&different_bytes));
}
