//! The read-path covered-period range.
//!
//! Every read path takes a mandatory bounded range, and one rule says which
//! entries it selects: `from <= window_end < to`
//! (`cpt-cf-usage-collector-adr-window-end-selection`). The rule reads the
//! period **end** and nothing else, so it needs no case for a point event
//! (`window_start == window_end`), it partitions the entries so adjacent
//! ranges sum without double counting, and it lets a plugin serve a range
//! from a rollup keyed on one column.
//!
//! The type travels as a typed parameter on the SDK trait and the Plugin
//! SPI (DESIGN §3.3 rule 5, scoped to those two Rust surfaces) and as a
//! first-class parameter on the REST surface (DESIGN §3.1 "Filter-surface
//! reservation") — `from` / `to` query parameters on the raw path,
//! `AggregationRequest.time_range` in the aggregate body. It is never a
//! `$filter` conjunct, which is why the covered-period fields are reserved
//! on the filter surface: a predicate over one would be a second, possibly
//! contradictory, constraint on something the range already fixes.

use time::{OffsetDateTime, UtcOffset};

use crate::error::UsageCollectorError;

/// A validated, UTC-normalized read range: `[from, to)`.
///
/// Constructed through [`TimeRange::new`], which is the only way to obtain
/// one — the fields are private, so an unordered range cannot reach a
/// storage plugin. `Copy`, so it passes by value on every surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    from: OffsetDateTime,
    to: OffsetDateTime,
}

impl TimeRange {
    /// Creates a range after normalizing both bounds to UTC and checking
    /// that it is strictly ordered.
    ///
    /// Normalization moves the offset, never the instant, so a caller that
    /// sends `13:00:00+01:00` and one that sends `12:00:00Z` obtain the same
    /// range — the same equivalence the covered period gets on ingestion.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorError::InvalidArgument`] when `to <= from`.
    pub fn new(from: OffsetDateTime, to: OffsetDateTime) -> Result<Self, UsageCollectorError> {
        let from = from.to_offset(UtcOffset::UTC);
        let to = to.to_offset(UtcOffset::UTC);
        if to <= from {
            return Err(UsageCollectorError::invalid_time_range(from, to));
        }
        Ok(Self { from, to })
    }

    /// The inclusive lower bound, in UTC.
    ///
    /// Named `lower_inclusive` / `upper_exclusive` rather than `from` / `to`
    /// — the wire parameters are named `from` and `to`, but a bare `from`
    /// reads as the `From` trait's conversion convention, and inclusivity
    /// is exactly the thing an off-by-one gets wrong, so it belongs in the
    /// name rather than a doc comment nobody reads at the call site.
    #[must_use]
    pub fn lower_inclusive(self) -> OffsetDateTime {
        self.from
    }

    /// The exclusive upper bound, in UTC.
    #[must_use]
    pub fn upper_exclusive(self) -> OffsetDateTime {
        self.to
    }

    /// The range's canonical text form, `<from>~<to>`, each bound rendered
    /// as its nanoseconds since the Unix epoch.
    ///
    /// One spelling, because this value ends up inside an opaque pagination
    /// cursor. The gear's read path folds it into the fingerprint a keyset
    /// continuation is bound to, and that fingerprint is compared across a
    /// page boundary: the caller's next
    /// request recomputes the string and it has to come out identical, so a
    /// second spelling that ordered or padded the bounds differently would
    /// refuse every cursor minted under the first. Same "two spellings of
    /// one rule" hazard [`Self::contains_window_end`] exists to avoid, one
    /// level up.
    ///
    /// The round trip is what makes the value usable as a fingerprint: the
    /// gear hands the string to the plugin on `ODataQuery::filter_hash`,
    /// the plugin mints it into `CursorV1::f`, and the follow-up request
    /// renders it again from its own `from` / `to`. Nanoseconds since the
    /// epoch make that reproducible for free — the rendering is
    /// instant-based, so it is offset-invariant as well as lossless, and
    /// two spellings of one instant render identically. That is the same
    /// equivalence [`TimeRange::new`]'s UTC normalization already
    /// establishes.
    #[must_use]
    pub fn canonical_form(self) -> String {
        // Full nanosecond resolution, deliberately NOT
        // `crate::id::canonical_period_bound`'s fixed six-digit-microsecond
        // form. That form truncates — it exists so the identity derivation
        // reads a frozen width over what is persisted — while
        // `TimeRange::new` applies no precision check at all, because a read
        // range derives no identity and refusing a caller who passed
        // `now()` with nanoseconds would be hostile for nothing. Composing
        // the two would collapse two ranges differing only below the
        // microsecond onto one fingerprint, so a cursor minted under either
        // would validate against the other — defeating the protection the
        // fingerprint exists to provide. Injectivity over what the caller
        // sent is this function's requirement; a frozen width over what is
        // persisted is the derivation's. They are not the same requirement
        // and they must not share a function.
        //
        // Keeping them apart also leaves `canonical_period_bound`'s
        // `debug_assert!` on a `0..=9999` year covering exactly the
        // ingestion path it was argued for: `TimeRange::new` validates
        // ordering only, so an in-process caller can build a range outside
        // that year span and would trip an assert nobody justified for the
        // read path.
        format!(
            "{}~{}",
            self.from.unix_timestamp_nanos(),
            self.to.unix_timestamp_nanos(),
        )
    }

    /// Does this range select an entry whose covered period ends at
    /// `window_end`?
    ///
    /// The reference spelling of `from <= window_end < to`: every
    /// in-process implementation calls this rather than re-deriving it, a
    /// SQL-backed plugin restates the same predicate in its own `WHERE`
    /// clause (it cannot call a Rust method), and the slice-6 Plugin SPI
    /// contract suite is what holds the two spellings to the same boundary.
    ///
    /// Comparison is instant-based: [`OffsetDateTime`]'s `Ord` normalizes
    /// its operand into the receiver's offset first, so `window_end` in any
    /// offset compares correctly without an explicit conversion here.
    #[must_use]
    pub fn contains_window_end(self, window_end: OffsetDateTime) -> bool {
        self.from <= window_end && window_end < self.to
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "time_range_tests.rs"]
mod time_range_tests;
