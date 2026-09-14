//! The rollup read path: which aggregate queries `usage_rollup_1h` can answer
//! exactly, and how their range splits into whole hours and partial edges.
//!
//! A query is eligible only when every predicate and grouping it carries is a
//! rollup grain column, so filtering rollup rows gives what filtering ledger
//! rows would. `origin`, `entry_type` and `invalidates` are never in the grain,
//! because each can tell a record from its invalidation, and signed netting is
//! exact only when a filter selects both or neither.

use time::OffsetDateTime;
use toolkit_odata::ast;
use usage_collector_sdk::{AggregationDimension, AggregationFold, MetadataFilter, TimeRange};

use super::translate::filter_fields;

/// One rollup bucket, in nanoseconds. `time_bucket(INTERVAL '1 hour', …)`
/// aligns buckets to Unix-epoch UTC hours, and so does this.
pub const BUCKET_NANOS: i128 = 3_600_000_000_000;

/// The filter fields a rollup row carries. `gts_type_id` is also in the grain,
/// but it is a typed parameter and never a filter field.
const GRAIN_FILTER_FIELDS: [&str; 1] = ["tenant_id"];

/// A read range split at whole hours: `[from, whole_from)` and
/// `[whole_to, to)` are the ledger-read edges, `[whole_from, whole_to)` the
/// rollup-read middle. `whole_from < whole_to` always holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HourSplit {
    pub from: OffsetDateTime,
    pub whole_from: OffsetDateTime,
    pub whole_to: OffsetDateTime,
    pub to: OffsetDateTime,
}

impl HourSplit {
    /// Whether `[from, whole_from)` holds any instant.
    #[must_use]
    pub fn has_lower_edge(self) -> bool {
        self.from < self.whole_from
    }

    /// Whether `[whole_to, to)` holds any instant.
    #[must_use]
    pub fn has_upper_edge(self) -> bool {
        self.whole_to < self.to
    }
}

/// Why a query is served by the ledger scan rather than the rollup. The label
/// is the bounded `reason` of `uc_timescaledb_aggregate_path_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    Fold,
    MetadataFilter,
    GroupBy,
    FilterField,
    SubHourRange,
}

impl FallbackReason {
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Fold => "fold",
            Self::MetadataFilter => "metadata_filter",
            Self::GroupBy => "group_by",
            Self::FilterField => "filter_field",
            Self::SubHourRange => "sub_hour_range",
        }
    }
}

fn floor_hour(t: OffsetDateTime) -> Option<OffsetDateTime> {
    let n = t.unix_timestamp_nanos();
    OffsetDateTime::from_unix_timestamp_nanos(n.div_euclid(BUCKET_NANOS) * BUCKET_NANOS).ok()
}

fn ceil_hour(t: OffsetDateTime) -> Option<OffsetDateTime> {
    if t.unix_timestamp_nanos().rem_euclid(BUCKET_NANOS) == 0 {
        Some(t)
    } else {
        floor_hour(t)?.checked_add(time::Duration::hours(1))
    }
}

/// Split `range` at whole UTC hours, or `None` when it covers no whole hour.
///
/// The edges keep the exact bounds, so the edge scan binds the same instants the
/// full scan would, and the two paths round a sub-microsecond bound the same way.
#[must_use]
pub fn hour_split(range: TimeRange) -> Option<HourSplit> {
    let from = range.lower_inclusive();
    let to = range.upper_exclusive();
    let whole_from = ceil_hour(from)?;
    let whole_to = floor_hour(to)?;
    (whole_from < whole_to).then_some(HourSplit {
        from,
        whole_from,
        whole_to,
        to,
    })
}

/// Whether the rollup can answer this aggregate exactly, and its hour split if
/// so. The checks run in the order of [`FallbackReason`]'s variants, so a
/// query with several disqualifications reports the first.
///
/// # Errors
///
/// Returns the first [`FallbackReason`] that applies.
pub fn rollup_eligible(
    fold: AggregationFold,
    filter: Option<&ast::Expr>,
    metadata_filter: &[MetadataFilter],
    group_by: &[AggregationDimension],
    range: TimeRange,
) -> Result<HourSplit, FallbackReason> {
    if !matches!(fold, AggregationFold::Sum | AggregationFold::Count) {
        return Err(FallbackReason::Fold);
    }
    if !metadata_filter.is_empty() {
        return Err(FallbackReason::MetadataFilter);
    }
    if !matches!(group_by, [] | [AggregationDimension::TenantId]) {
        return Err(FallbackReason::GroupBy);
    }
    if let Some(expr) = filter {
        // A filter off the schema falls back too, so the scan reports the same
        // translation error it always has.
        let fields = filter_fields(expr).map_err(|_| FallbackReason::FilterField)?;
        if !fields.iter().all(|f| GRAIN_FILTER_FIELDS.contains(f)) {
            return Err(FallbackReason::FilterField);
        }
    }
    hour_split(range).ok_or(FallbackReason::SubHourRange)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "rollup_tests.rs"]
mod rollup_tests;
