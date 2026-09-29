//! The plugin's own `FeedPosition` encoding.
//!
//! Opaque to the gateway and never on the wire (the gear's `DESIGN.md` §3.1):
//! a position's internal structure is the storage plugin's own choice, which is
//! why the SDK models it as a byte sequence, bounds its size and claims nothing
//! about its meaning. This module is that choice for the `TimescaleDB` backend.
//!
//! It is the feed order of this plugin's `docs/DESIGN.md` §4.1 item 3 — the
//! inserting transaction's `xid8`, then the entry `id` breaking ties inside one
//! transaction — and nothing else. **Its width does not grow with a
//! subscription's breadth**, which is the substantive obligation §3.1 places on
//! a position and what the `feed-position-bounded` contract check exists to
//! catch; a plugin keying a position per tenant fails that even well inside the
//! SDK's byte bound.

use usage_collector_sdk::FeedPosition;
use uuid::Uuid;

/// The width of every position this plugin issues: a `u64` and a `uuid`.
///
/// One fixed width rather than a minimum, so
/// [`decode_position`] can refuse anything else as a position this plugin did
/// not issue.
pub const FEED_POSITION_BYTES: usize = 8 + 16;

/// The greatest `uuid`, which the head position uses as its tie-breaker.
///
/// `PostgreSQL` compares `uuid` bytewise and [`Uuid::as_bytes`] is the same
/// big-endian layout, so this is the greatest value on both sides: no stored
/// `id` sorts above it within one `xact_id`.
pub const MAX_UUID: Uuid = Uuid::from_u128(u128::MAX);

/// Encode one feed position.
///
/// Big-endian on the `xact_id` so the encoding's **bytewise** order is the
/// feed's order. That is a property of this codec rather than of
/// [`FeedPosition`], which provides no `Ord` deliberately — but the plugin's own
/// decode and the page statement's `(xact_id, id)` row-value comparison have to
/// agree about which of two positions is greater, and one layout for both is
/// how they do.
///
/// # Panics
///
/// Never. [`FeedPosition::new`] refuses only an empty sequence and one above
/// `MAX_FEED_POSITION_BYTES`, and [`FEED_POSITION_BYTES`] is neither — which is
/// asserted by `a_position_is_twenty_four_bytes_whatever_it_encodes` rather
/// than argued here alone.
#[must_use]
pub fn encode_position(xact_id: u64, id: Uuid) -> FeedPosition {
    let mut bytes = Vec::with_capacity(FEED_POSITION_BYTES);
    bytes.extend_from_slice(&xact_id.to_be_bytes());
    bytes.extend_from_slice(id.as_bytes());
    // Split into separate let binding to scope the `expect_used` suppression to
    // the binding alone, not the entire function body.
    #[allow(clippy::expect_used)]
    let position = FeedPosition::new(bytes).expect("a 24-byte position is inside the SDK bound");
    position
}

/// Decode one feed position, or say why it is not one this plugin issued.
///
/// # Errors
///
/// A `String` for any width but [`FEED_POSITION_BYTES`], naming both the width
/// received and the width expected. The caller lifts it to
/// `UsageCollectorPluginError::Internal`, which is where the SPI puts "a
/// position it did not issue" — a host-contract breach rather than a retryable
/// fault or a caller-visible outcome.
///
/// There is no other failure mode: every 24-byte sequence decodes to some
/// `(u64, Uuid)` pair. A pair naming a transaction that never existed is not
/// detectable here and needs no detection — it selects no rows.
pub fn decode_position(position: &FeedPosition) -> Result<(u64, Uuid), String> {
    let bytes = position.as_bytes();
    if bytes.len() != FEED_POSITION_BYTES {
        return Err(format!(
            "a feed position of {} bytes is not one this backend issued: its \
             positions are exactly {FEED_POSITION_BYTES} bytes",
            bytes.len(),
        ));
    }
    let mut xact_id = [0_u8; 8];
    xact_id.copy_from_slice(&bytes[..8]);
    let mut id = [0_u8; 16];
    id.copy_from_slice(&bytes[8..]);
    Ok((u64::from_be_bytes(xact_id), Uuid::from_bytes(id)))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "feed_position_tests.rs"]
mod feed_position_tests;
