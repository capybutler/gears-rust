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
//! Task 3 threads this type through the SDK trait and the Plugin SPI as a
//! typed parameter (DESIGN §3.3 rule 5, scoped to those two Rust surfaces)
//! and through the REST surface as a first-class parameter (DESIGN §3.1
//! "Filter-surface reservation"), replacing the `$filter` conjunct that
//! carries the window today. That is why the covered-period fields are
//! reserved on the filter surface already: a predicate over one would be a
//! second, possibly contradictory, constraint on something the range is
//! about to fix.

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
