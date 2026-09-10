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
pub mod keyset;
pub mod translate;

use usage_collector_sdk::{MetadataFilter, MeterTypeId, TimeRange};

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

/// Push the meter scope and the covered-period range — the predicate **every**
/// ledger read path applies and the only one they fully share.
///
/// `from <= window_end < to`, reading the covered-period **end** alone
/// (`cpt-cf-usage-collector-adr-window-end-selection`). Not overlap, which
/// selects one entry into two adjacent ranges, and not containment, which drops
/// it out of both; a covered period longer than the range is the case that
/// tells the three apart. The predicate never names `window_start`.
///
/// It is hoisted here because getting it wrong fails **two** contract checks at
/// once, not one: `window-end-selection` directly, and `quantity-round-trip`
/// because that check reads its stored entry back over a range one second wide
/// at the period end, while the entry's period began an hour earlier
/// (`DIVERGENCES.md` §F). Transcribed once per path, the second transcription
/// needs its own test to notice drift; read from here, both paths inherit the
/// one that exists.
///
/// Binds the meter and the two bounds onto `ctx` in that order, and pushes the
/// three clause strings onto `clauses`. `time_range` is a typed SPI parameter
/// and is never a `$filter` conjunct — the gateway refuses a predicate naming
/// either bound — so nothing caller-supplied reaches this.
pub fn push_meter_and_range_clauses(
    gts_type_id: &MeterTypeId,
    time_range: TimeRange,
    ctx: &mut SqlCtx,
    clauses: &mut Vec<String>,
) {
    clauses.push(format!(
        "r.gts_type_id = ${}",
        ctx.push(SqlBind::Str(gts_type_id.as_str().to_owned()))
    ));
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
/// column for a grouped metadata dimension, and one statement holding a
/// qualified and an unqualified reference to one column is the drift the alias
/// constant exists to stop. What holds this half of that to it is
/// `every_fragment_qualifies_its_columns_with_the_alias_the_from_clause_declares`
/// in `query/aggregate_tests.rs`, which drains this builder into the fragment
/// list it walks and refuses a bare ledger column — the same test the two
/// siblings above name.
///
/// An empty value set matches nothing (the gateway rejects it, but be
/// defensive): a `FALSE` clause is emitted so the result is empty rather than
/// unfiltered.
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

/// Hard upper bound on the page size either list path will request from
/// `PostgreSQL` in a single `fetch_all`, regardless of the caller's `$top`.
///
/// This is a **defense-in-depth backstop**, not the primary cap: the
/// usage-collector core gateway already rejects `$top > 1000` with
/// `400 InvalidArgument` (its own `MAX_PAGE_SIZE`) before any plugin call.
/// The value is kept in lock-step with that gateway cap so this clamp is
/// never reached in normal operation — it only bites if the plugin is ever
/// driven by a different or buggy caller, preventing an unbounded
/// full-result-set read (a resource/DoS hazard) at the persistence boundary.
pub const MAX_PAGE_SIZE: u64 = 1000;

/// Resolve the effective `LIMIT` for a list query: the caller's `$top`
/// (`requested`) when present, else `default_page_size`, clamped to the
/// `[1, MAX_PAGE_SIZE]` range.
///
/// Clamping (rather than rejecting) is correct here because the plugin is
/// pure persistence and must not own HTTP-policy `4xx`s — the reject already
/// lives in the core. Keyset pagination degrades gracefully under a clamp:
/// the look-ahead + `next_cursor` still yield a correct, resumable page, just
/// a smaller one than an out-of-contract caller asked for.
///
/// The **lower** bound of 1 is not cosmetic. A resolved page size of 0 would
/// drive `LIMIT 0+1 = 1` then `truncate(0)` on the look-ahead read — leaving
/// `rows.last()` `None` on a non-empty table and 500-ing the list path at that
/// guard, before `encode_next_cursor` is reached. The REST surface cannot
/// deliver a 0 (the toolkit `OData` extractor rejects a zero page size — in
/// either the `$top` or the `limit` spelling — with `InvalidLimit` before the
/// handler runs), but an
/// in-process SDK caller hands the plugin an `ODataQuery` directly and
/// reaches neither that check nor the core gateway's
/// `prepare_list_query`. Flooring to the smallest legal page (1)
/// keeps the look-ahead invariant intact without the plugin minting a `4xx`.
#[must_use]
pub fn effective_page_size(requested: Option<u64>, default_page_size: u64) -> u64 {
    requested
        .unwrap_or(default_page_size)
        .clamp(1, MAX_PAGE_SIZE)
}

#[cfg(test)]
#[path = "query_tests.rs"]
mod query_tests;
