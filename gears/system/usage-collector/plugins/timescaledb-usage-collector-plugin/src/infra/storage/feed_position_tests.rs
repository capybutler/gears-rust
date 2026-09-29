use uuid::Uuid;

use super::*;

// A compile-time guard rather than a runtime assertion: both operands are
// consts, so there is no state a run could vary, and the check belongs where
// it fails the build instead of a test. The SDK's own `contract::fixtures`
// uses this shape for the same reason.
const _: () = assert!(FEED_POSITION_BYTES <= usage_collector_sdk::MAX_FEED_POSITION_BYTES);

#[test]
fn a_position_round_trips() {
    let id = Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
    let position = encode_position(7_777_777_777, id);
    assert_eq!(
        decode_position(&position),
        Ok((7_777_777_777, id)),
        "the codec is this plugin's alone and nothing else reads its bytes, so \
         a round trip is the whole of its contract"
    );
    // An extreme: the generic property holds for values at both ends of the
    // space, not only the tested mid-range.
    let position = encode_position(u64::MAX, MAX_UUID);
    assert_eq!(
        decode_position(&position),
        Ok((u64::MAX, MAX_UUID)),
        "even at the boundaries the round trip is total"
    );
}

#[test]
fn a_position_is_twenty_four_bytes_whatever_it_encodes() {
    // `feed-position-bounded` compares a position's size across subscriptions
    // of different breadth. This encoding names no subscription at all, so the
    // check passes by construction — and this is the assertion that says so,
    // rather than the argument in the doc comment.
    for (xact_id, id) in [
        (0_u64, Uuid::nil()),
        (1, Uuid::from_u128(1)),
        (u64::MAX, MAX_UUID),
        (3, Uuid::from_u128(u128::MAX - 1)),
    ] {
        assert_eq!(
            encode_position(xact_id, id).len(),
            FEED_POSITION_BYTES,
            "every position is one fixed width: {xact_id}, {id}"
        );
    }
}

#[test]
fn the_encoding_orders_the_way_the_feed_does() {
    // Big-endian so the encoding's bytewise order is `(xact_id, id)` order.
    // This is a property of *this* codec, not something the SDK relies on:
    // `FeedPosition` provides no `Ord`, deliberately. It is asserted because
    // the plugin's own decode and the SQL row-value comparison have to agree
    // about which of two positions is greater.
    //
    // The xact_id values 1 and 256 straddle a byte boundary: `BE(1) = [0,…,0,1]`
    // and `BE(256) = [0,…,1,0]`, so bytewise comparison holds; under little-endian
    // those would invert to `LE(1) = [1,0,…]` and `LE(256) = [0,1,…]`, which would
    // fail the assertion and red the test.
    let lower = encode_position(1, Uuid::from_u128(9));
    let by_xact = encode_position(256, Uuid::from_u128(0));
    let by_id = encode_position(1, Uuid::from_u128(10));
    assert!(lower.as_bytes() < by_xact.as_bytes(), "xact_id leads");
    assert!(lower.as_bytes() < by_id.as_bytes(), "id breaks the tie");
}

#[test]
fn a_position_of_the_wrong_length_is_refused_rather_than_truncated() {
    // Review Focus 2. `FeedStart::After` and `until` both carry a position a
    // caller hands back, and the SPI puts "a position it did not issue" under
    // `Internal`. A decode that indexed a short slice would panic, and one
    // that ignored trailing bytes would resume from a position nobody issued.
    for len in [1_usize, 8, 16, 23, 25, 64] {
        let position = FeedPosition::new(vec![0_u8; len]).expect("inside the SDK bound");
        let refused = decode_position(&position);
        assert!(
            refused.is_err(),
            "a {len}-byte position is not one this plugin issued, so it must be \
             refused: {refused:?}"
        );
        let detail = refused.unwrap_err();
        assert!(
            detail.contains(&len.to_string()) && detail.contains("24"),
            "the detail names the width received and the width expected, so an \
             operator can tell a foreign cursor from a corrupted one: {detail}"
        );
    }
}

#[test]
fn the_max_uuid_is_the_greatest_uuid_postgres_can_compare() {
    // The head position's tie-breaker. PostgreSQL compares `uuid` bytewise, and
    // `Uuid::as_bytes` is the same big-endian layout, so all-0xff is the
    // greatest value on both sides of the wire.
    assert_eq!(MAX_UUID.as_bytes(), &[0xff_u8; 16]);
    assert_eq!(MAX_UUID, Uuid::from_u128(u128::MAX));
}
