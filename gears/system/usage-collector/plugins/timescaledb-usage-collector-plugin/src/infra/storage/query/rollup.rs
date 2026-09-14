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
use usage_collector_sdk::{
    AggregationDimension, AggregationFold, MetadataFilter, MeterTypeId, TimeRange,
};

use super::translate::{SqlBind, SqlCtx, filter_fields, translate_scope};
use super::{aggregate::aggregate_limit_clause, ledger_from_clause};

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

/// The signed quantity of one ledger entry, as the rollup view sums it.
const SIGNED_VALUE: &str = "CASE WHEN r.invalidates IS NULL THEN r.value ELSE -r.value END";
/// The signed count of one ledger entry, as the rollup view sums it.
const SIGNED_COUNT: &str = "CASE WHEN r.invalidates IS NULL THEN 1 ELSE -1 END";

/// The meter and its partition-key subquery, both reading the meter's bind at
/// `$meter`. Both halves of the statement start with these, so chunk exclusion
/// applies to the view's materialised half and to the ledger edges alike.
fn meter_scope(meter: usize) -> Vec<String> {
    vec![
        format!("r.gts_type_id = ${meter}"),
        format!(
            "r.type_key = (SELECT k.type_key FROM usage_type_key k WHERE k.gts_type_id = ${meter})"
        ),
    ]
}

/// A rendered rollup aggregate: the statement, its binds in placeholder order,
/// and how many leading key columns each result row carries.
#[derive(Debug)]
pub struct RollupStatement {
    pub sql: String,
    pub binds: Vec<SqlBind>,
    pub dim_count: usize,
}

/// Render the aggregate for an eligible query: whole hours from
/// `usage_rollup_1h`, partial edge hours from the ledger, summed in one
/// statement.
///
/// The result shape matches the scan's exactly (spec §6.4). A grouped query
/// drops a group whose count nets to zero, because the scan never forms a group
/// from withdrawn entries alone. An ungrouped `SUM` is `NULL` over a selection
/// whose count nets to zero, and an ungrouped `COUNT` is `0`. Both tests read
/// the count, never the sum, so a genuine zero or negative total still returns.
///
/// Precondition: [`rollup_eligible`] returned `Ok(split)` for these inputs. The
/// filter is rendered once per half with its own placeholders. Its bare column
/// names are valid against both `FROM` clauses, since each half reads one
/// relation.
///
/// # Errors
///
/// Returns an error string for a fold other than `Sum` or `Count`, or when the
/// filter cannot be translated.
pub fn build_rollup_aggregate_sql(
    gts_type_id: &MeterTypeId,
    split: HourSplit,
    fold: AggregationFold,
    filter: Option<&ast::Expr>,
    group_by: &[AggregationDimension],
) -> Result<RollupStatement, String> {
    let grouped = !group_by.is_empty();
    let fold_expr = match (fold, grouped) {
        (AggregationFold::Sum, false) => {
            "(CASE WHEN COALESCE(SUM(c), 0) = 0 THEN NULL ELSE SUM(s) END)::numeric"
        }
        (AggregationFold::Count, false) => "COALESCE(SUM(c), 0)::numeric",
        (AggregationFold::Sum, true) => "SUM(s)::numeric",
        (AggregationFold::Count, true) => "SUM(c)::numeric",
        (other, _) => return Err(format!("the rollup does not serve the {other} fold")),
    };
    let dim = if grouped {
        "r.tenant_id::text AS d, "
    } else {
        ""
    };

    let mut ctx = SqlCtx::new(1);
    let meter = ctx.push(SqlBind::Str(gts_type_id.as_str().to_owned()));

    let mut rollup_where = meter_scope(meter);
    rollup_where.push(format!(
        "r.bucket >= ${}",
        ctx.push(SqlBind::DateTime(split.whole_from))
    ));
    rollup_where.push(format!(
        "r.bucket < ${}",
        ctx.push(SqlBind::DateTime(split.whole_to))
    ));
    if let Some(expr) = filter {
        rollup_where.push(translate_scope(expr, &mut ctx)?);
    }
    let mut parts = vec![format!(
        "SELECT {dim}r.sum_value AS s, r.count_value AS c FROM usage_rollup_1h r WHERE {}",
        rollup_where.join(" AND ")
    )];

    let mut edges = Vec::new();
    if split.has_lower_edge() {
        let a = ctx.push(SqlBind::DateTime(split.from));
        let b = ctx.push(SqlBind::DateTime(split.whole_from));
        edges.push(format!("(r.window_end >= ${a} AND r.window_end < ${b})"));
    }
    if split.has_upper_edge() {
        let a = ctx.push(SqlBind::DateTime(split.whole_to));
        let b = ctx.push(SqlBind::DateTime(split.to));
        edges.push(format!("(r.window_end >= ${a} AND r.window_end < ${b})"));
    }
    if !edges.is_empty() {
        let mut edge_where = meter_scope(meter);
        edge_where.push(format!("({})", edges.join(" OR ")));
        if let Some(expr) = filter {
            edge_where.push(translate_scope(expr, &mut ctx)?);
        }
        let group = if grouped { " GROUP BY 1" } else { "" };
        parts.push(format!(
            "SELECT {dim}SUM({SIGNED_VALUE})::numeric AS s, SUM({SIGNED_COUNT})::bigint AS c \
             FROM {} WHERE {}{group}",
            ledger_from_clause(),
            edge_where.join(" AND ")
        ));
    }

    let dim_count = usize::from(grouped);
    let (outer_dim, tail) = if grouped {
        (
            "d, ",
            format!(
                " GROUP BY 1 HAVING SUM(c) <> 0{}",
                aggregate_limit_clause(dim_count)
            ),
        )
    } else {
        ("", String::new())
    };
    Ok(RollupStatement {
        sql: format!(
            "WITH parts AS ({}) SELECT {outer_dim}{fold_expr} FROM parts{tail}",
            parts.join(" UNION ALL ")
        ),
        binds: ctx.binds,
        dim_count,
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "rollup_tests.rs"]
mod rollup_tests;
