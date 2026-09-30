//! The feed page's statement, and the retention-mark probe beside it.
//!
//! The single producer of both, on the principle the keyset builder already
//! follows: one function owns a path's SQL, so a column name or a bind order
//! cannot be stated twice and drift once.
//!
//! The order is `(xact_id, id)` over the subscribed types, served from
//! `usage_records_feed_idx (gts_type_id, xact_id, id)` (this plugin's
//! `docs/DESIGN.md` §3.7). The compiled scope is `AND`ed on and filters rows
//! without changing the order, the horizon or the marks (§3.6, Scope).
//!
//! **`FEED_COLUMNS`, not `RECORD_COLUMNS`.** Task 3 of this slice pointed this
//! builder at `RECORD_COLUMNS` because the split into a feed-specific column
//! list had not landed yet; Task 5 is the split, and this module now names
//! `FEED_COLUMNS`, which carries `xact_id` — the column this statement's own
//! `ORDER BY` and every continuation this module mints need — beside
//! everything `RECORD_COLUMNS` already read. Both constants are `pub(crate)`
//! for this reader alongside `ENTRY_TYPE_ENUM`, which is `pub(crate)` in the
//! same file for the same reason.

use toolkit_odata::ast;
use uuid::Uuid;

use crate::infra::storage::query::bind::SqlBind;
use crate::infra::storage::query::translate::{SqlCtx, translate_scope};

/// Whether a retention mark of any subscribed type stands above a position.
///
/// It selects no row data — only whether such a mark exists — and is keyed on
/// the subscribed types alone. The mark is per GTS type and **ignores the
/// compiled scope**, which §3.6's Refusal granularity argues is the granularity
/// the gateway's rule names rather than a shortfall: removal and age are both
/// read over the subscription's types, so the refusal can be conservative for a
/// cursor whose own scope lost nothing, and it never serves a truncated range.
pub const MARK_ABOVE_SQL: &str = "SELECT 1 FROM usage_feed_retention_marks \
     WHERE gts_type_id = ANY($1) AND (xact_id, id) > (($2)::xid8, $3) LIMIT 1";

/// Build the page statement and the scope binds that follow its fixed ones.
///
/// Bind order, which the caller applies in exactly this sequence: the
/// subscription array at `$1`, the settled horizon at `$2`, then — present only
/// when `after` is — the position's two components, then — present only when
/// `until` is — that bound's two, then the scope fragment's own binds. Every
/// `xid8` value binds as `text` and is cast on the **parameter**; casting the
/// column instead would defeat `usage_records_feed_idx` and, on the rendered
/// digits, misorder two ids of different lengths.
///
/// `LIMIT` is rendered rather than bound, as the keyset builder renders its
/// clamped page size: it is a `u64` this crate owns after the adapter has
/// validated it against the published bound, never caller text.
///
/// **`after` absent is a first read, and it carries no position predicate at
/// all** — no band, no floor, no age threshold (ruling D1; §3.6 First read:
/// "There is no start lookup and no age threshold"). Completeness still holds,
/// for the reason it holds on a continuation: an unsettled entry has an id no
/// smaller than the horizon, so it sorts after every entry the page reads.
///
/// # Errors
///
/// The translator's own `String` when the compiled scope names a column outside
/// the allowlist or a shape it cannot render. Returned before the caller takes
/// a connection, so an unrenderable scope never opens a transaction.
pub fn build_feed_page_sql(
    after: Option<(u64, Uuid)>,
    until: Option<(u64, Uuid)>,
    scope: &ast::Expr,
    limit: u64,
) -> Result<(String, Vec<SqlBind>), String> {
    let mut clauses = vec![
        "gts_type_id = ANY($1)".to_owned(),
        "xact_id < ($2)::xid8".to_owned(),
    ];
    let mut next = 3;
    if after.is_some() {
        clauses.push(format!(
            "(xact_id, id) > ((${xact})::xid8, ${id})",
            xact = next,
            id = next + 1,
        ));
        next += 2;
    }
    if until.is_some() {
        clauses.push(format!(
            "(xact_id, id) <= ((${xact})::xid8, ${id})",
            xact = next,
            id = next + 1,
        ));
        next += 2;
    }
    let mut ctx = SqlCtx::new(next);
    clauses.push(translate_scope(scope, &mut ctx)?);
    Ok((
        format!(
            "SELECT {columns} FROM usage_records WHERE {where_clause} \
             ORDER BY xact_id, id LIMIT {limit}",
            columns = super::super::record_store::FEED_COLUMNS,
            where_clause = clauses.join(" AND "),
        ),
        ctx.binds,
    ))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "feed_tests.rs"]
mod feed_tests;
