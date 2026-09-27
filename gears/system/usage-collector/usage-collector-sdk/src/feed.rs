//! The usage feed's position, page, start mode and subscription.
//!
//! DESIGN §3.1 defines all four. The feed is the replay-safe read path a
//! charging consumer uses instead of `list_usage_records`, and these are the
//! types its two Rust surfaces speak: the SDK client trait over the wire
//! cursor, and the Plugin SPI over the plugin's own [`FeedPosition`].

use thiserror::Error;

/// The largest [`FeedPosition`] a plugin may issue, in bytes.
///
/// The gateway carries a position inside a `toolkit_odata::CursorV1`, and
/// `usage-collector-v1.yaml` caps that wire token at `maxLength: 4096`
/// characters. base64url encodes 512 bytes as 684 characters, which leaves
/// more than 3400 for the cursor's own order and filter-hash fields and its
/// JSON framing. So 512 is a bound a plugin can plan against and the wire can
/// always carry.
///
/// DESIGN §3.1 puts the substantive obligation on top of this number: a
/// position's encoded size "may not grow with a subscription's breadth". A
/// plugin keying a position per tenant fails that even well inside this bound,
/// which is what the `feed-position-bounded` contract check exists to catch.
pub const MAX_FEED_POSITION_BYTES: usize = 512;

/// A point in the feed's order, issued and interpreted by the storage plugin
/// alone.
///
/// Opaque to the gateway and never on the wire (DESIGN §3.1). Its internal
/// structure is the plugin's own choice, which is why this is a byte sequence
/// rather than a struct: the SDK bounds its size and its ordering and claims
/// nothing about its meaning.
///
/// Ordering is deliberately not provided: bytewise order on the plugin's own
/// encoding is not the feed's order, and the feed's order is the plugin's
/// alone to realise. A plugin that needs to compare its own positions decodes
/// them first, rather than reaching for an `Ord` this type would otherwise
/// hand it. `Eq` and `Hash` remain because a position still needs to key a
/// map and be compared for equality.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FeedPosition(Vec<u8>);

/// Why a [`FeedPosition`] could not be built.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum FeedPositionInvalid {
    /// The plugin supplied no bytes.
    #[error(
        "a feed position must carry at least one byte: zero bytes is a plugin that did not fill \
         the position rather than a position that needs none"
    )]
    Empty,

    /// The plugin supplied more bytes than the wire cursor can carry.
    #[error(
        "a feed position of {actual} bytes exceeds the {MAX_FEED_POSITION_BYTES}-byte bound the \
         wire cursor can carry; a position this large cannot be handed to a consumer"
    )]
    TooLarge {
        /// How many bytes the plugin supplied.
        actual: usize,
    },
}

impl FeedPosition {
    /// Builds a position from a plugin's own encoding.
    ///
    /// # Errors
    ///
    /// [`FeedPositionInvalid::Empty`] when `bytes` is empty, and
    /// [`FeedPositionInvalid::TooLarge`] when it exceeds
    /// [`MAX_FEED_POSITION_BYTES`]. The bound is checked here, where a plugin
    /// can still act on it, rather than at encoding time in the gateway,
    /// where the only remaining move is to fail a consumer's read.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, FeedPositionInvalid> {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return Err(FeedPositionInvalid::Empty);
        }
        if bytes.len() > MAX_FEED_POSITION_BYTES {
            return Err(FeedPositionInvalid::TooLarge {
                actual: bytes.len(),
            });
        }
        Ok(Self(bytes))
    }

    /// The plugin's encoding, borrowed.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// How many bytes the position encodes to.
    ///
    /// This is the figure the `feed-position-bounded` check compares across
    /// subscriptions of different breadth.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Always `false`: [`FeedPosition::new`] never admits an empty
    /// sequence, so there is no stored state this can vary on.
    ///
    /// Present because `clippy::len_without_is_empty` asks for it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Exposes the plugin's own encoding to generic `impl AsRef<[u8]>` sinks, the
/// same way [`as_bytes`](FeedPosition::as_bytes) does directly.
impl AsRef<[u8]> for FeedPosition {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "feed_tests.rs"]
mod feed_tests;
