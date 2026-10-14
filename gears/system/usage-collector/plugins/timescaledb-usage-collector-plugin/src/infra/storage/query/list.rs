//! The two ledger read statements — the scoped point lookup and the keyset
//! page — and the page assembly that turns the look-ahead read into what the
//! caller gets.
//!
//! The single producer of each, on the principle [`super::feed`] and
//! [`super::keyset`] already follow: one function owns a path's SQL, so a
//! column name or a bind order cannot be stated twice and drift once. Both
//! statements select `RECORD_COLUMNS`, read their meter and covered period
//! through [`push_meter_and_range_clauses`], and translate every scope through
//! [`translate_scope`] — so neither can drift from `aggregate` on the
//! obligations all three are held to.
//!
//! Pure (no DB): `build_get_sql` and `build_list_sql` return a statement
//! and its binds in placeholder order, `build_list_page` takes rows already
//! fetched. What is left in
//! [`super::super::record_store`]'s `get` and `list` is the round trip between
//! them, which is why both halves are tested without a database.
//!
//! **Neither path excludes a withdrawn pair, and neither ever will.** The
//! asymmetry with [`super::aggregate`] is deliberate: a total that counts one
//! half of a withdrawn pair is a wrong total, and that is a derived view; this
//! is the ledger itself. The SPI states it outright — a withdrawn pair MUST be
//! returned as persisted from both `get` and `list_usage_records`.

use time::format_description::well_known::Rfc3339;
use toolkit_odata::{ODataQuery, SortDir, ast};
use uuid::Uuid;

use usage_collector_sdk::v2::{
    Keyset, MetadataFilter, RecordPage, TimeRange, UsageCollectorPluginError,
    is_keyset_safe_record_field,
};

use crate::infra::storage::columns::RECORD_COLUMNS;
use crate::infra::storage::entity::UsageRecordRow;
use crate::infra::storage::mapper::record_row_to_model;
use crate::infra::storage::query::keyset::{keyset_predicate, render_order_by, uniform_dir};
use crate::infra::storage::query::translate::{
    SqlBind, SqlCtx, record_column, record_field_kind, translate_scope,
};
use crate::infra::storage::query::{
    ledger_from_clause, push_metadata_filter_clauses, push_meter_and_range_clauses,
};

/// Default page size when the caller omits `$top` (`query.limit`).
pub(crate) const DEFAULT_PAGE_SIZE: u64 = 100;

/// Extract a single order-field value from a row as its cursor-key string.
///
/// Inverse of [`cursor_key_to_bind`](super::keyset::cursor_key_to_bind):
/// the `uuid` columns render via [`Uuid`]'s [`Display`](std::fmt::Display) — the
/// canonical hyphenated lower-case form — the `timestamptz` bounds
/// as RFC 3339, and the text columns as-is — each the spelling that helper
/// parses back for the field's declared kind, so a minted boundary re-binds to
/// the value it was read from.
///
/// **The arms are [`usage_collector_sdk::v2::KEYSET_SAFE_RECORD_FIELDS`], and that
/// is the whole rule.** A field off that list is one the SDK does not carry as
/// an attribute of every entry in its own right: either it can be absent (a
/// `NULL` compares as `NULL` inside the row-value tuple and silently drops the
/// row), or it is derived from one that can. `None` here means "not a keyset key
/// on the row" — a refusal to mint, not a missing value.
///
/// One half of a two-sided map: [`record_column`] resolves an order field to the
/// column the `ORDER BY` and the keyset tuple render from, and this resolves the
/// same field to the value the boundary carries. The two-sided coupling test in
/// `list_tests.rs` is what couples them.
fn record_row_key(row: &UsageRecordRow, field: &str) -> Option<String> {
    match field {
        "id" => Some(row.id.to_string()),
        "window_start" => row.window_start.format(&Rfc3339).ok(),
        "window_end" => row.window_end.format(&Rfc3339).ok(),
        "tenant_id" => Some(row.tenant_id.to_string()),
        "resource_id" => Some(row.resource_id.clone()),
        "resource_type" => Some(row.resource_type.clone()),
        "origin" => Some(row.origin.clone()),
        "accepted_at" => row.accepted_at.format(&Rfc3339).ok(),
        _ => None,
    }
}

/// Build the point-lookup SQL and the binds that go with it: the asked-for
/// `id` at `$1`, conjoined with the caller's compiled PDP scope.
///
/// **The scope is the whole filter beyond the `id`.** This path carries no
/// caller-supplied `$filter`, and nothing above the SPI re-checks the row that
/// comes back, so a lookup selecting on `id` alone would answer with any row
/// whose `id` a caller can name — an existence oracle over every tenant's
/// entries.
///
/// The scope itself is translated by [`translate_scope`], which every read path
/// shares: it owns both allowlist gates, the bind numbering, and the
/// parentheses that let the fragment be conjoined safely. What is left here is
/// statement assembly.
///
/// Binds start at `$2` because the `id` occupies `$1`, which is why
/// `PgRecordStore::get` binds the `id` first. This is the only seeded caller
/// on a production path; both collection paths seed at 1, their leading meter
/// value going through the same [`SqlCtx`] as everything else. Only the ordered
/// bind values are returned, not the [`SqlCtx`]: the statement is finished.
///
/// # Errors
///
/// Propagates [`translate_scope`]'s refusal unchanged. A scope that fails to
/// translate and is dropped instead leaves `WHERE id = $1` — a translation
/// failure turned into an authorization bypass — so this returns `Err` rather
/// than a partial statement, and it is the only producer of this path's SQL.
// @cpt-algo:cpt-cf-uc-plugin-algo-scoped-point-lookup:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-scoped-point-lookup:p1
pub(crate) fn build_get_sql(scope: &ast::Expr) -> Result<(String, Vec<SqlBind>), String> {
    // `$1` is the `id`; every scope bind therefore starts at `$2`.
    let mut ctx = SqlCtx::new(2);
    let fragment = translate_scope(scope, &mut ctx)?;
    Ok((
        format!("SELECT {RECORD_COLUMNS} FROM usage_records WHERE id = $1 AND {fragment}"),
        ctx.binds,
    ))
}

/// Build the keyset page's SQL and the binds that go with it, and be the only
/// producer of this path's statement.
///
/// The `WHERE` is assembled in bind order: the meter at `$1`, the covered
/// period's two bounds at `$2` and `$3`, then the caller's composed `$filter`,
/// the metadata side channel, and the keyset continuation. Identifiers come
/// from the [`record_column`] allowlist, the static [`RECORD_COLUMNS`], and the
/// literal column names the shared builders write — `r.gts_type_uuid` and
/// `r.window_end` from [`push_meter_and_range_clauses`], `r.metadata ->>` from
/// [`push_metadata_filter_clauses`]. None is caller input, and every
/// caller-derived value is bound, save the clamped page size, which is a `u64`
/// this function renders itself into the `LIMIT`.
///
/// **Selection reads the covered-period end alone** — `from <= window_end < to`,
/// per `usage_collector_sdk::v2::TimeRange::contains_window_end`, which is where
/// that rule lives. The predicate never names `window_start`. `time_range` is a
/// typed parameter and deliberately not reachable through `query.filter`.
///
/// **No withdrawal exclusion, and none belongs here** — this module's own doc
/// says why, and `PgRecordStore::get` carries no exclusion either.
///
/// `query.order` is rendered as handed, and the keyset tuple is built from the
/// same `query.order` in the same field order a few lines above, so the two
/// cannot name different columns or disagree about direction — which is what
/// makes a continuation resume from the boundary the previous page ended on.
///
/// # Errors
///
/// Returns an error string when the composed `$filter` cannot be translated
/// (propagated from [`translate_scope`] unchanged, and never dropped — a
/// dropped scope leaves the read unscoped), or when an order or keyset field
/// is off the allowlist or not keyset-safe, or when the order is empty or
/// mixed-direction. A caller must propagate it: this is the only producer of
/// the statement, so a partial one is never returned in its place.
// @cpt-algo:cpt-cf-uc-plugin-algo-keyset-seek-page:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-keyset-seek-pagination:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-no-filter-widening-no-offset:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-entries-as-persisted:p1
pub(crate) fn build_list_sql(
    gts_type_uuid: Uuid,
    time_range: TimeRange,
    query: &ODataQuery,
    metadata_filter: &[MetadataFilter],
    keyset: Option<&Keyset>,
    limit: u64,
) -> Result<(String, Vec<SqlBind>), String> {
    let mut ctx = SqlCtx::new(1);
    let mut clauses: Vec<String> = Vec::new();

    // The meter scope and the covered-period range, from the one spelling both
    // read paths share. Read rather than transcribed, so `aggregate` cannot
    // drift from this on the ADR obligation both are held to.
    push_meter_and_range_clauses(gts_type_uuid, time_range, &mut ctx, &mut clauses);

    // The composed `$filter`: the caller's filter `And`-composed with the
    // compiled PDP scope, or the scope alone when the caller supplied none —
    // either way one expression, which for a multi-constraint grant is an `Or`.
    // `translate_scope` returns a parenthesized fragment, which is what makes
    // pushing it into a `join(" AND ")` safe.
    if let Some(expr) = query.filter() {
        clauses.push(translate_scope(expr, &mut ctx)?);
    }

    // Metadata side-channel: AND across filters, OR within one filter's
    // values (see [`push_metadata_filter_clauses`]).
    push_metadata_filter_clauses(metadata_filter, &mut ctx, &mut clauses);

    if let Some(keyset) = keyset {
        let order_pairs: Vec<(&str, bool)> = query
            .order
            .0
            .iter()
            .map(|key| (key.field.as_str(), matches!(key.dir, SortDir::Asc)))
            .collect();
        clauses.push(keyset_predicate(
            &order_pairs,
            keyset.values(),
            record_column,
            record_field_kind,
            is_keyset_safe_record_field,
            &mut ctx,
        )?);
    }

    let order_sql = render_order_by(&query.order, record_column)?;

    Ok((
        format!(
            "SELECT {RECORD_COLUMNS} FROM {} WHERE {} ORDER BY {order_sql} LIMIT {}",
            // Called, never spelled: with a literal here the shared constant
            // would be decorative.
            ledger_from_clause(),
            clauses.join(" AND "),
            // The look-ahead: one row past the page, so the page can tell "last
            // page" from "there is another" without a second query. This `+ 1`
            // is what makes `build_list_page`'s `rows.len() > page_size` correct
            // rather than `>=`.
            limit.saturating_add(1),
        ),
        ctx.binds,
    ))
}

/// Turn the look-ahead read into the page the caller gets: drop the
/// look-ahead row and return the last in-page row's keyset for the gateway
/// to mint a continuation from.
///
/// No wire cursor and no `query.filter_hash` is produced here: minting,
/// encoding and fingerprinting belong to the gateway, and this hands back a
/// [`Keyset`] of the last in-page row's sort values for it to mint from.
///
/// The boundary values are read in `query.order` field order, one per key, not
/// in a canonical order this function assumed.
///
/// # Precondition
///
/// `limit >= 1`, and `rows` is the look-ahead read [`build_list_sql`] asked for
/// — at most `limit + 1` rows. The `+ 1` there is what makes the
/// `rows.len() > page_size` here correct rather than `>=`.
/// [`effective_page_size`](super::effective_page_size) floors the limit to 1;
/// this signature does not, so a caller passing `0` truncates the whole page
/// away and reaches the `Err` below.
///
/// The direction is resolved through [`uniform_dir`] rather than read off the
/// leading key — see `keyset.rs` for why every entry point that acts on a
/// direction resolves it that way. This is a pure, directly unit-tested function
/// holding its own contract, so it does not rely on `render_order_by` happening
/// to run first in [`build_list_sql`].
///
/// # Errors
///
/// Returns [`UsageCollectorPluginError::Internal`] when an order field is not a
/// keyset key on the row, when `query.order` mixes sort directions or is empty
/// (both from [`uniform_dir`]), when the keyset is too large
/// ([`usage_collector_sdk::v2::KeysetInvalid::TooLarge`];
/// [`usage_collector_sdk::v2::KeysetInvalid::Empty`] cannot surface, `uniform_dir`'s
/// own empty check running first over the same order), or when a stored row
/// cannot be mapped to the SDK model. Distinctly from those mapped failures,
/// `"non-empty page lost its tail"` means the precondition above was broken —
/// a bug in this crate rather than anything a caller of the SPI can provoke.
// @cpt-algo:cpt-cf-uc-plugin-algo-keyset-seek-page:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-next-page-detection:p1
pub(crate) fn build_list_page(
    mut rows: Vec<UsageRecordRow>,
    query: &ODataQuery,
    limit: u64,
) -> Result<RecordPage, UsageCollectorPluginError> {
    let page_size = usize::try_from(limit).unwrap_or(usize::MAX);

    // Look-ahead row present -> a next page exists; drop it before mapping.
    let has_next = rows.len() > page_size;
    if has_next {
        rows.truncate(page_size);
    }

    let next = if has_next {
        let last = rows
            .last()
            .ok_or_else(|| UsageCollectorPluginError::internal("non-empty page lost its tail"))?;
        let values = query
            .order
            .0
            .iter()
            .map(|key| {
                record_row_key(last, &key.field).ok_or_else(|| {
                    UsageCollectorPluginError::internal(format!(
                        "order field `{}` is not a keyset key on the row",
                        key.field
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        // Through `uniform_dir`, not the leading key, so the keyset's direction
        // cannot describe an order the page was not read in.
        let direction = uniform_dir(query.order.0.iter().map(|key| key.dir))
            .map_err(UsageCollectorPluginError::internal)?;
        Some(
            Keyset::new(values, direction)
                .map_err(|err| UsageCollectorPluginError::internal(err.to_string()))?,
        )
    } else {
        None
    };

    let items = rows
        .into_iter()
        .map(record_row_to_model)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(RecordPage { items, next })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "list_tests.rs"]
mod list_tests;
