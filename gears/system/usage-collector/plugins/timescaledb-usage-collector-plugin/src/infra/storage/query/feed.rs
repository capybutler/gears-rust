//! The feed page's statement, and the retention-mark probe beside it.
//!
//! The single producer of both, on the principle the keyset builder already
//! follows: one function owns a path's SQL, so a column name or a bind order
//! cannot be stated twice and drift once.
//!
//! The order is `(xact_id, id)` over the subscribed types, served **per
//! type** from `usage_records_feed_idx (gts_type_uuid, xact_id, id)` (this
//! plugin's `docs/DESIGN.md` §3.7) and combined by a bounded sort. The
//! compiled scope is `AND`ed on and filters rows without changing the order,
//! the horizon or the marks (§3.6, Scope).
//!
//! **Why per-type equality, not one `ANY($1)`.**
//! `tests/feed_page_query_plan_pg.rs` found, against a live plan, that
//! `PostgreSQL` derives no index pathkeys past a `ScalarArrayOpExpr` on a
//! leading index column — so a single `gts_type_uuid = ANY($1)` statement sorts
//! its whole remaining range before `LIMIT`, however narrow the page. An
//! equality (`gts_type_uuid = t.gts`, one row of `unnest($1::uuid[])` per
//! lateral iteration) keeps `usage_records_feed_idx`'s pathkeys, so each
//! iteration is a streaming, already-ordered read that stops at its own
//! `LIMIT`. [`build_feed_page_sql`]'s own doc has the statement shape and the
//! per-clause reasoning.
//!
//! **`FEED_COLUMNS`, not `RECORD_COLUMNS`**: it carries `xact_id`, the column
//! this statement's own `ORDER BY` and every continuation it mints need, beside
//! everything `RECORD_COLUMNS` reads. Both constants are `pub(crate)` for this
//! reader, alongside `ENTRY_TYPE_ENUM`.

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
     WHERE gts_type_uuid = ANY($1) AND (xact_id, id) > (($2)::xid8, $3) LIMIT 1";

/// Build the page statement and the scope binds that follow its fixed ones.
///
/// # Shape: a lateral join over the subscription, not `ANY($1)`
///
/// ```text
/// SELECT s.* FROM unnest($1::uuid[]) AS t(gts) CROSS JOIN LATERAL (
///     SELECT <FEED_COLUMNS> FROM usage_records
///     WHERE gts_type_uuid = t.gts AND <the same bounds as before>
///     ORDER BY xact_id, id LIMIT <limit>
/// ) AS s
/// ORDER BY s.xact_id_text::xid8, s.id LIMIT <limit>
/// ```
///
/// Chosen over per-type `UNION ALL` branches because the statement's text stays
/// one fixed shape regardless of subscription width: no builder loop, no
/// branch-count-dependent test rewrite, no empty-subscription special case. Each
/// lateral iteration binds `gts_type_uuid` to an **equality** against one row of
/// `unnest($1::uuid[])`, which — unlike a single `= ANY($1)` — keeps
/// `usage_records_feed_idx`'s pathkeys, so `PostgreSQL` walks each type's slice
/// of the index in order and the inner `LIMIT` stops the walk early (confirmed
/// against `actual rows`/`loops` in `EXPLAIN ANALYZE`). The outer sort then
/// combines at most `subscription_width × limit` rows rather than sorting the
/// whole backlog.
///
/// **The inner `LIMIT` equals the outer one, and that equality is the whole
/// correctness argument, not an optimisation.** A row in the global top
/// `limit` has at most `limit − 1` rows ahead of it *within its own type*, so
/// per-type top-`limit` then combine yields exactly the global top `limit`. A
/// smaller inner limit could silently drop a row the outer sort needed.
///
/// **The outer `ORDER BY` casts `xact_id_text` back to `xid8`, rather than
/// sorting the text.** The lateral alias `s` exposes exactly `FEED_COLUMNS`'s
/// columns — no bare `xact_id`, `xid8` having no `sqlx` `Decode`, which is also
/// why [`FeedRecordRow::xact_id`](crate::infra::storage::entity::FeedRecordRow)
/// decodes from the cast name. Sorting on `xact_id_text` bare would compare
/// digit strings lexicographically, the digit-crossing hazard
/// `CHUNK_HIGHEST_POSITIONS_SQL`'s own alias exists to avoid; the cast back
/// recovers `xid8`'s numeric comparison for one cast rather than a second raw
/// column threaded through just for sorting.
///
/// **The outer projection is `s.*`, on purpose**: it reproduces every alias the
/// inner `SELECT` produces without restating the column list, which keeps
/// `FeedRecordRow`'s by-name decode working unchanged.
///
/// **An empty subscription behaves as it does today.** `unnest` of an empty
/// array yields no rows for `t`, so the `CROSS JOIN LATERAL` yields none
/// either — the same empty page `gts_type_uuid = ANY('{}')` already produced,
/// with no special case needed.
///
/// **The caller must pass a subscription with no repeated type.**
/// `unnest($1::uuid[])` yields one lateral iteration per array *element*, not
/// per distinct value, so a type named twice delivers its matching rows twice.
/// `PgRecordStore::feed_page` deduplicates before binding here, and the SDK's
/// `FeedSubscription::new` sorts and dedups before that; this statement itself
/// does not guard against one.
///
/// Bind order does not multiply with the lateral: the subscription array is
/// `$1`, the settled horizon `$2`, then — present only when `after` is — the
/// position's two components, then — present only when `until` is — that
/// bound's two, then the scope fragment's own binds. Each appears exactly once
/// in the rendered text, inside the lateral body. Every `xid8` value binds as
/// `text` and is cast on the **parameter**; casting the column instead would
/// defeat `usage_records_feed_idx` and, on the rendered digits, misorder two
/// ids of different lengths.
///
/// `LIMIT` is rendered rather than bound, as the keyset builder renders its
/// clamped page size: a `u64` this crate owns after the adapter validated it
/// against the published bound, never caller text. Rendered twice — inner
/// per-type and outer combined — always the same value.
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
// @cpt-dod:cpt-cf-uc-plugin-dod-feed-order:p1
pub fn build_feed_page_sql(
    after: Option<(u64, Uuid)>,
    until: Option<(u64, Uuid)>,
    scope: &ast::Expr,
    limit: u64,
) -> Result<(String, Vec<SqlBind>), String> {
    let mut clauses = vec![
        "gts_type_uuid = t.gts".to_owned(),
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
            "SELECT s.* FROM unnest($1::uuid[]) AS t(gts) CROSS JOIN LATERAL \
             (SELECT {columns} FROM usage_records WHERE {where_clause} \
             ORDER BY xact_id, id LIMIT {limit}) AS s \
             ORDER BY s.xact_id_text::xid8, s.id LIMIT {limit}",
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
