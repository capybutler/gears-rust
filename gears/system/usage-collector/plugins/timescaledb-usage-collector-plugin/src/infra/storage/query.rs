//! Injection-safe `OData` → SQL translation foundation.
//!
//! Pure (no DB) logic that turns a validated `toolkit_odata` filter AST into a
//! parameterized `PostgreSQL` `WHERE` fragment plus an ordered list of binds.
//! Every SQL identifier is drawn from a closed allowlist
//! ([`translate::record_column`]); every value is bound (`$N`), never
//! interpolated.
//!
//! This root module holds what **both ledger read paths** share — the page-size
//! clamp below, the table alias, the covered-period predicate and the metadata
//! side channel — rather than either path's own module. `list` reaching into
//! `aggregate` for the alias would be the wrong shape, and a second spelling in
//! either caller is the drift these exist to prevent.

pub mod aggregate;
pub mod bind;
pub mod feed;
pub mod keyset;
pub mod reconciliation;
pub mod rollup;
pub mod translate;

use usage_collector_sdk::{MetadataFilter, TimeRange};
use uuid::Uuid;

use bind::SqlBind;
use translate::SqlCtx;

/// The ledger table and the alias every fragment in this crate qualifies its
/// columns with.
///
/// It exists so the alias is **one constant every read path reads** rather than
/// a convention several files independently honour — a caller spelling its own
/// `FROM` can pick a different one with no compile error and invalid SQL at
/// runtime. Both collection paths build their `FROM` from this, and
/// [`aggregate::withdrawal_exclusion_clause`], [`aggregate::fold_select_expr`]
/// and [`aggregate::dimension_select_expr`] all bind to the alias it declares.
#[must_use]
pub fn ledger_from_clause() -> &'static str {
    "usage_records r"
}

/// Push the meter-identity predicate alone — `gts_type_uuid` plus its
/// partition-key subquery — with no covered-period bound.
///
/// Factored out of [`push_meter_and_range_clauses`] for [`reconciliation`]'s
/// watermark read (spec §9.3's S3): the watermarks are unbounded by the
/// request's range, so that statement needs the meter scope alone. Every other
/// read path wants both and calls [`push_meter_and_range_clauses`], which
/// composes this with its own range clauses.
///
/// Binds the meter onto `ctx` once, and pushes its clause strings onto
/// `clauses`. The partition-key subquery reuses the meter's bind rather than
/// binding it a second time.
pub fn push_meter_clause(gts_type_uuid: Uuid, ctx: &mut SqlCtx, clauses: &mut Vec<String>) {
    let meter = ctx.push(SqlBind::Uuid(gts_type_uuid));
    clauses.push(format!("r.gts_type_uuid = ${meter}"));
    // The same meter as its partition key, which is what excludes every other
    // type's chunks: a predicate on `gts_type_uuid` alone excludes none. The
    // subquery runs once as an InitPlan and the chunks it rules out are skipped
    // at runtime. A type that was never written has no key, so it yields NULL,
    // matches no row and excludes every chunk — the ordinary empty result, with
    // no sentinel and no lookup ahead of the statement.
    clauses.push(format!(
        "r.type_key = (SELECT k.type_key FROM usage_type_key k WHERE k.gts_type_uuid = ${meter})"
    ));
}

/// Push the meter scope and the covered-period range — the predicate **every**
/// ledger read path applies and the only one they fully share.
///
/// `from <= window_end < to`, reading the covered-period **end** alone
/// (`cpt-cf-usage-collector-adr-window-end-selection`). Not overlap, which
/// selects one entry into two adjacent ranges, and not containment, which drops
/// it out of both; a covered period longer than the range is the case that tells
/// the three apart. The predicate never names `window_start`.
///
/// It is hoisted here because getting it wrong fails more than one contract
/// check: `window-end-selection` directly, and `quantity-round-trip` because that
/// check reads its stored entry back over a range one second wide at the period
/// end, while the entry's period began an hour earlier. Transcribed once per
/// path, the second transcription needs its own test to notice drift; read from
/// here, both paths inherit the one that exists.
///
/// Binds the meter and the two bounds onto `ctx` in that order, and pushes
/// [`push_meter_clause`]'s clauses then the range bounds onto `clauses`.
/// `time_range` is a typed SPI parameter and is never a `$filter` conjunct — the
/// gateway refuses a predicate naming either bound — so nothing caller-supplied
/// reaches this.
pub fn push_meter_and_range_clauses(
    gts_type_uuid: Uuid,
    time_range: TimeRange,
    ctx: &mut SqlCtx,
    clauses: &mut Vec<String>,
) {
    push_meter_clause(gts_type_uuid, ctx, clauses);
    clauses.push(format!(
        "r.window_end >= ${}",
        ctx.push(SqlBind::DateTime(time_range.lower_inclusive()))
    ));
    clauses.push(format!(
        "r.window_end < ${}",
        ctx.push(SqlBind::DateTime(time_range.upper_exclusive()))
    ));
}

/// Append the metadata side-channel filters as parameterized `WHERE` clauses.
///
/// Shared by both collection paths so both expand the side channel identically:
/// AND across filters, OR within one filter's values
/// (`r.metadata ->> $key IN ($v1, $v2, …)`). The key and every value are bound
/// via `ctx` (`$N`); only the `r.metadata ->>` shape is interpolated, so this is
/// injection-safe.
///
/// The column is qualified with the alias [`ledger_from_clause`] declares, like
/// every other fragment: `aggregate` also emits a *presence guard* over the same
/// column for a grouped metadata dimension, and one statement holding a qualified
/// and an unqualified reference to one column is the drift the alias constant
/// exists to stop. Holding this half to it is
/// `every_fragment_qualifies_its_columns_with_the_alias_the_from_clause_declares`
/// in `query/aggregate_tests.rs`.
///
/// "Like every other fragment" is not literally every one: a translated `$filter`
/// reads `tenant_id = $4`, because [`translate::record_column`] returns bare
/// column names, and the list `SELECT`'s own column list is bare for the same
/// reason. Both are correct — one table is in scope in the outer `WHERE`, so the
/// alias is implicit. What the qualification buys here is the fragments that are
/// *not* implicit: the withdrawal exclusion opens a second `usage_records` as
/// `w`, and inside a statement holding two, an unqualified column is the one
/// thing that cannot be read off the page.
///
/// An empty value set matches nothing (the gateway rejects it, but be
/// defensive): a `FALSE` clause is emitted so the result is empty rather than
/// unfiltered.
// @cpt-algo:cpt-cf-uc-plugin-algo-metadata-filter-composition:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-metadata-filter-semantics:p1
pub fn push_metadata_filter_clauses(
    metadata_filter: &[MetadataFilter],
    ctx: &mut SqlCtx,
    clauses: &mut Vec<String>,
) {
    for mf in metadata_filter {
        if mf.values().is_empty() {
            clauses.push("FALSE".to_owned());
            continue;
        }
        let key_n = ctx.push(SqlBind::Str(mf.key().as_str().to_owned()));
        let placeholders = mf
            .values()
            .iter()
            .map(|v| format!("${}", ctx.push(SqlBind::Str(v.clone()))))
            .collect::<Vec<_>>();
        clauses.push(format!(
            "r.metadata ->> ${key_n} IN ({})",
            placeholders.join(", ")
        ));
    }
}

/// Domain-owned (`crate::domain::ports::MAX_PAGE_SIZE`), re-exported here so
/// the ledger query builders below can name it unqualified.
pub use crate::domain::ports::MAX_PAGE_SIZE;

/// Resolve the effective `LIMIT` for a list query: the caller's `$top`
/// (`requested`) when present, else `default_page_size`, clamped to the
/// `[1, MAX_PAGE_SIZE]` range.
///
/// Clamping (rather than rejecting) is correct here because the plugin is pure
/// persistence and must not own HTTP-policy `4xx`s — the reject already lives in
/// the core. Keyset pagination degrades gracefully under a clamp: the
/// look-ahead and `next_cursor` still yield a correct, resumable page, just a
/// smaller one than an out-of-contract caller asked for.
///
/// The **lower** bound of 1 is not cosmetic. A resolved page size of 0 would
/// drive `LIMIT 0+1 = 1` then `truncate(0)` on the look-ahead read — leaving
/// `rows.last()` `None` on a non-empty table and 500-ing the list path at
/// `build_list_page`'s own "non-empty page lost its tail" guard, before a
/// `Keyset` is ever built. The REST surface cannot deliver a 0 (the toolkit
/// `OData` extractor rejects a zero page size with `InvalidLimit` before the
/// handler runs), but an in-process SDK caller hands the plugin an `ODataQuery`
/// directly and reaches neither that check nor the core gateway's
/// `prepare_list_query`.
#[must_use]
pub fn effective_page_size(requested: Option<u64>, default_page_size: u64) -> u64 {
    requested
        .unwrap_or(default_page_size)
        .clamp(1, MAX_PAGE_SIZE)
}

#[cfg(test)]
#[path = "query_tests.rs"]
mod query_tests;
