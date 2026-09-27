//! The usage feed's position, page, start mode and subscription.
//!
//! DESIGN §3.1 defines all four. The feed is the replay-safe read path a
//! charging consumer uses instead of `list_usage_records`, and these are the
//! types its two Rust surfaces speak: the SDK client trait over the wire
//! cursor, and the Plugin SPI over the plugin's own [`FeedPosition`].

use thiserror::Error;

use crate::models::{MeterTypeId, UsageRecord};

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

/// Where a feed read begins — **named**, never inferred from an absent
/// position.
///
/// DESIGN §3.1: the gateway compiles a request carrying no wire cursor to
/// [`FeedStart::Oldest`] rather than passing an absent position down, because
/// "oldest retained" and "no position supplied" are different instructions and
/// only one of them is a start.
///
/// `#[non_exhaustive]` per DESIGN §2.2's additive-evolution constraint: a start
/// mode admitted later is a variant rather than a further argument, so a plugin
/// matches this with a wildcard arm and answers
/// [`crate::error::UsageCollectorPluginError::Internal`] there. Neither v1
/// variant begins at the head — the feed serves charging consumers, for which
/// skipping retained history is never a correct start.
///
/// Generic over the position each surface speaks: `FeedStart<&CursorV1>` on the
/// SDK client trait, `FeedStart<FeedPosition>` on the Plugin SPI.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FeedStart<P> {
    /// The oldest entry the subscription still retains. Never the head, and
    /// never refused on the retention floor.
    Oldest,
    /// Continue after a position the feed issued.
    After(P),
}

/// Settled entries in feed order, plus the cursor that continues them.
///
/// Everything before `next` is delivered and final under the compiled scope the
/// read ran under (DESIGN §3.1).
///
/// Generic over the cursor kind, which is what keeps a plugin's position off the
/// wire: the SPI returns `FeedPage<FeedPosition>` and the SDK client and REST
/// return `FeedPage<CursorV1>`, so handing a raw position to a consumer is a
/// type error rather than a review obligation. DESIGN §3.1 (SPI) and §3.3
/// (wire) both describe a type named `FeedPage` but require it to carry a
/// `FeedPosition` and a `CursorV1` respectively; the type parameter is what
/// lets one Rust type satisfy both descriptions, instantiated differently per
/// surface — the same resolution [`FeedStart`] uses for its own position
/// parameter.
// No `Eq`: a derive bounds type parameters only, so `Eq` here would require
// `Vec<UsageRecord>: Eq` unconditionally, and `UsageRecord` derives `PartialEq`
// without `Eq` (`models.rs:944`). `PartialEq` is what the page needs and all it
// can have.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedPage<C> {
    /// The page's entries, in feed order.
    pub entries: Vec<UsageRecord>,
    /// The continuation. `Some` on every page of a live read, short pages
    /// included; `None` once a bounded replay has reached its `until`.
    pub next: Option<C>,
}

/// The set of GTS types one consumer reads.
///
/// It bounds that consumer's pages and its cursor (DESIGN §3.1). Held as a
/// sorted, deduplicated sequence: a set has no repeats, and fixing the order
/// here means a plugin keying a position across the whole subscription sees the
/// same subscription whatever order the caller listed it in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedSubscription(Vec<MeterTypeId>);

/// Why a [`FeedSubscription`] could not be built.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum FeedSubscriptionInvalid {
    /// The caller named no GTS type.
    #[error(
        "a feed subscription must name at least one GTS type: a subscription reading nothing has \
         no page to serve"
    )]
    Empty,
}

impl FeedSubscription {
    /// Builds a subscription, deduplicating and ordering the types.
    ///
    /// # Errors
    ///
    /// [`FeedSubscriptionInvalid::Empty`] when `types` yields nothing.
    pub fn new(
        types: impl IntoIterator<Item = MeterTypeId>,
    ) -> Result<Self, FeedSubscriptionInvalid> {
        let mut types: Vec<MeterTypeId> = types.into_iter().collect();
        // `MeterTypeId` implements neither `Ord` nor `PartialOrd`, deliberately
        // and with a rustdoc saying so: the `GtsTypeId` it wraps implements
        // neither, and the note tells a caller needing an order to delegate to
        // the string form. That is what this does, rather than adding a trait
        // impl to a shared type for one caller's benefit.
        types.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        types.dedup_by(|a, b| a.as_str() == b.as_str());
        if types.is_empty() {
            return Err(FeedSubscriptionInvalid::Empty);
        }
        Ok(Self(types))
    }

    /// The subscribed types, sorted and deduplicated.
    ///
    /// This is the slice the Plugin SPI's `subscription` parameter takes.
    #[must_use]
    pub fn types(&self) -> &[MeterTypeId] {
        &self.0
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "feed_tests.rs"]
mod feed_tests;
