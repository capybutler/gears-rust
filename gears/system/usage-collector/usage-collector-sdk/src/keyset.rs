//! The raw read path's typed keyset and page.
//!
//! A module of its own beside [`crate::feed`] for the same reason that one
//! exists: these are a read path's own types, and `models.rs` carries the
//! entity vocabulary rather than one path's machinery.
//!
//! # Why the gateway mints and the plugin does not
//!
//! `cpt-cf-usage-collector-dod-gateway-owned-cursor` requires the storage plugin
//! to *"receive a structured keyset of the last row's sort values"* and *"NOT
//! mint, encode or interpret a wire token"*. [`Keyset`] is that structured
//! value, returned by `UsageCollectorPluginV1::list_usage_records` in place of a
//! page carrying an encoded string; the gateway turns it into a
//! `toolkit_odata::CursorV1`, as it turns a [`crate::feed::FeedPosition`] into
//! one on the feed path.
//!
//! **This type deliberately implements neither `Serialize` nor `Deserialize`.**
//! The wire cursor is the gateway's to mint, so a keyset has no wire form of its
//! own, and the absence is enforced by the compiler. `keyset_tests` pins it.

use thiserror::Error;
use toolkit_odata::SortDir;

use crate::stored::StoredUsageRecord;

/// The largest JSON-encoded [`Keyset`] values array the wire cursor can carry.
///
/// Pinned by `keyset_tests.rs`.
pub const MAX_KEYSET_BYTES: usize = 2243;

/// Why a [`Keyset`] could not be built.
///
/// A dedicated error rather than [`crate::UsageCollectorError`], on
/// [`crate::feed::FeedPositionInvalid`]'s precedent: a plugin builds this
/// value and returns `UsageCollectorPluginError`, so the gear's own error
/// type has no business on a plugin's construction path. A plugin maps
/// either variant to `UsageCollectorPluginError::internal`.
///
/// **That mapping is right for [`Self::Empty`] and known-wrong for
/// [`Self::TooLarge`].** `Empty` is a plugin that did not fill the keyset, so
/// `Internal` is the honest answer. `TooLarge` is reachable from caller input:
/// an attribution value full of JSON-escaped control characters is ingested
/// successfully (see `DIVERGENCES.md` entry 29) and a later read ordering on
/// that field makes this variant, and therefore a **500 the
/// caller provoked**. The fix is upstream in attribution validation, not in this
/// mapping, and it is a permit→deny on ingestion for already-deployed callers —
/// owner-reserved.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeysetInvalid {
    /// The plugin supplied no boundary values.
    #[error(
        "a keyset carries one boundary value per key of the order it was dispatched under, and \
         every admissible order is non-empty: zero values is a plugin that did not fill the \
         keyset rather than a keyset that needs none"
    )]
    Empty,

    /// The plugin's boundary values, JSON-encoded, exceed what the wire
    /// cursor can carry.
    #[error(
        "a keyset whose values encode to {actual} JSON bytes exceeds the {MAX_KEYSET_BYTES}-byte \
         budget the wire cursor can carry; a continuation this large cannot be handed to a \
         consumer"
    )]
    TooLarge {
        /// The JSON-encoded length of the values array, in bytes — see
        /// [`MAX_KEYSET_BYTES`] for why this is not the values' summed
        /// `String::len()`.
        actual: usize,
    },
}

/// The typed last-row sort tuple behind an opaque cursor (DESIGN §3.1).
///
/// Its values are positionally aligned with the keys of the
/// `toolkit_odata::ODataQuery::order` the read was dispatched under — one value
/// per key, in sequence — and `direction` is that order's single direction. The
/// gateway verifies both on return rather than trusting them, neither being
/// expressible in the type.
///
/// Ordering is deliberately not provided, on [`crate::feed::FeedPosition`]'s
/// reasoning: comparing two keysets means comparing them under an order neither
/// carries. Equality is provided so a test can assert a continuation
/// round-tripped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keyset {
    values: Vec<String>,
    direction: SortDir,
}

impl Keyset {
    /// Builds a keyset from the last in-page row's sort values.
    ///
    /// # Errors
    ///
    /// [`KeysetInvalid::Empty`] when `values` is empty, and
    /// [`KeysetInvalid::TooLarge`] when the values' JSON-encoded form (see
    /// [`MAX_KEYSET_BYTES`]) exceeds that bound. The bound is checked here,
    /// where a plugin can still act on it, rather than at encoding time in
    /// the gateway, where the only remaining move is to fail a consumer's
    /// read.
    pub fn new(
        values: impl IntoIterator<Item = impl Into<String>>,
        direction: SortDir,
    ) -> Result<Self, KeysetInvalid> {
        let values: Vec<String> = values.into_iter().map(Into::into).collect();
        if values.is_empty() {
            return Err(KeysetInvalid::Empty);
        }
        // `Vec<String>` has no failure mode under `serde_json`: every element
        // is valid UTF-8, `String`'s and `Vec`'s `Serialize` impls hold no
        // fallible user logic, and the sink is an in-memory buffer. `map_or`
        // still folds the unreachable `Err` into the measurement rather than
        // `expect`ing it away — treating an unmeasurable value as too large is
        // the same fail-closed choice `TooLarge` already makes.
        let actual = serde_json::to_string(&values).map_or(usize::MAX, |encoded| encoded.len());
        if actual > MAX_KEYSET_BYTES {
            return Err(KeysetInvalid::TooLarge { actual });
        }
        Ok(Self { values, direction })
    }

    /// The boundary values, positionally aligned with the dispatched order.
    #[must_use]
    pub fn values(&self) -> &[String] {
        &self.values
    }

    /// The single sort direction the dispatched order carried.
    #[must_use]
    pub fn direction(&self) -> SortDir {
        self.direction
    }
}

/// A page of ledger entries and the keyset that continues it.
///
/// Public fields, and **not** `#[non_exhaustive]`, for the reason
/// [`crate::feed::FeedPage`] gives: an out-of-crate plugin *constructs* this as
/// its SPI return value, and `#[non_exhaustive]` on a struct forbids external
/// struct-literal construction outright, so marking it would break every plugin.
/// The attribute belongs on types external code matches on, which is why
/// `FeedStart` carries it and this does not. **Recorded so a later edit does not
/// "fix" it.**
///
/// # Not `FeedPage<Keyset>`
///
/// Structurally identical, and deliberately a separate type. DESIGN §3.1 defines
/// `FeedPage<C>` as carrying *"Settled entries in feed order"* where everything
/// before the cursor is *"delivered and final"*, and DESIGN §3.10 denies the raw
/// path that guarantee in terms: *"a consumer that must not miss entries reads
/// this and not `list_usage_records`"*. Reusing the feed's type would import a
/// published guarantee onto a path that does not have it.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordPage {
    /// The page's entries, as persisted, in the dispatched order.
    pub items: Vec<StoredUsageRecord>,
    /// The last in-page row's keyset where a further page exists, and
    /// `None` where this page is the last.
    pub next: Option<Keyset>,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "keyset_tests.rs"]
mod keyset_tests;
