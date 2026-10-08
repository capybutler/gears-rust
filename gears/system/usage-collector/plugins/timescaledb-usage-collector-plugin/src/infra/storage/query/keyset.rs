//! Order-by rendering, keyset (tuple-comparison) predicates, and cursor
//! decode for keyset pagination.
//!
//! All column identifiers come from a caller-supplied allowlist closure
//! (`record_column` from [`super::translate`]); cursor key values are always
//! bound. The gateway's default order is the all-ascending `(window_end, id)`
//! tuple, so [`keyset_predicate`] emits the row-value tuple form for
//! uniform-direction orders. Every call site in this crate that acts on a sort
//! direction takes it from [`uniform_dir`], so a mixed-direction order — which
//! the gateway guarantees cannot arrive — is refused when the first page is
//! rendered and when the returned keyset is built, not only on the continuation.
//!
//! This module mints no cursor: minting, encoding, fingerprinting and the
//! forward-only check belong to the gateway, which also verifies a returned
//! [`usage_collector_sdk::Keyset`]'s arity and direction against the order it
//! dispatched before minting from it. [`decode_cursor`] survives as disclosed
//! dead code — the SPI says decoding a wire cursor is "the gateway's alone", the
//! gateway strips `query.cursor` before every dispatch, and the only caller in
//! this crate is a test. It is a deletion candidate, not a contract
//! obligation.

use std::str::FromStr;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use toolkit_odata::filter::FieldKind;
use toolkit_odata::{CursorV1, ODataOrderBy, SortDir};

use super::bind::SqlBind;
use super::translate::SqlCtx;

/// The one sort direction an admissible order carries.
///
/// The gateway guarantees `query.order` "uses one sort direction throughout"
/// (`UsageCollectorPluginV1::list_usage_records`), so a mixed-direction order is
/// a breach rather than a shape to serve. Every caller in this crate that acts
/// on a direction resolves it here rather than reading one key's and trusting
/// the rest, because they run at different points of one request and refusing in
/// only one of them is not refusing:
///
/// - [`render_order_by`] runs on the first page, before any cursor exists, and
///   would otherwise render `window_end ASC, id DESC` and serve it.
/// - [`keyset_predicate`] runs on a continuation only, so alone it turns a
///   wrongly-ordered served page into a `500` on page two rather than a refusal
///   on page one.
/// - `build_list_page` (`record_store.rs`) builds the
///   [`usage_collector_sdk::Keyset`] handed back to the gateway, and would
///   otherwise record the leading key's direction as the keyset's own — which
///   `query::verify_returned_keyset` then refuses one request later.
///
/// # Errors
///
/// Returns an error string when `dirs` is empty or carries more than one
/// direction. [`keyset_predicate`] and `build_list_page` check emptiness first;
/// [`render_order_by`] relies on this one. The empty arm is the fail-closed
/// floor for a direct caller of this public function.
pub fn uniform_dir(dirs: impl IntoIterator<Item = SortDir>) -> Result<SortDir, String> {
    let mut dirs = dirs.into_iter();
    let Some(first) = dirs.next() else {
        return Err("order must not be empty".to_owned());
    };
    if dirs.all(|dir| dir == first) {
        Ok(first)
    } else {
        Err(
            "mixed-direction keyset order refused: one sort direction is required throughout"
                .to_owned(),
        )
    }
}

/// Render an `ORDER BY` column list (`"window_end ASC, id ASC"`) from an
/// `ODataOrderBy`, resolving each field through `col`.
///
/// The caller-supplied `$orderby` path: `col` is handed an untyped caller string
/// here, unlike the `$filter` path where a `FilterField` has already bounded the
/// input, so the allowlist is the whole boundary between that string and the
/// rendered SQL. An unresolved field is refused, never interpolated.
///
/// The direction comes from [`uniform_dir`], not from each key independently:
/// this runs on the first page, so mapping directions one by one would *serve* a
/// mixed-direction order and leave [`keyset_predicate`] to refuse it a page
/// later.
///
/// # Errors
///
/// Returns an error string when the order is empty or its directions are mixed
/// (both from [`uniform_dir`], which sees the order before any column is
/// resolved), or when a field is not on the allowlist (never interpolated).
pub fn render_order_by(
    order: &ODataOrderBy,
    col: impl Fn(&str) -> Option<&'static str>,
) -> Result<String, String> {
    let dir = match uniform_dir(order.0.iter().map(|key| key.dir))? {
        SortDir::Asc => "ASC",
        SortDir::Desc => "DESC",
    };
    let parts = order
        .0
        .iter()
        .map(|key| {
            let column = col(&key.field)
                .ok_or_else(|| format!("order field not allowlisted: {}", key.field))?;
            Ok(format!("{column} {dir}"))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(parts.join(", "))
}

/// Build a keyset predicate as a row-value tuple comparison for the supplied
/// `(field_name, is_ascending)` order pairs against `cursor_keys`.
///
/// For an all-ascending order this is `(c1, c2, …) > ($a, $b, …)`; for an
/// all-descending order, `(…) < (…)`. Each cursor key is parsed to a typed bind
/// via [`cursor_key_to_bind`] — keyed by the field's declared [`FieldKind`]
/// (resolved through `kind`), not its column name — and pushed onto `ctx`.
///
/// # Order shape
///
/// The tuple is rendered in the order `order_pairs` arrives in, each bind pushed
/// alongside the key it belongs to; nothing here looks up a canonical name or
/// assumes a canonical slot. That is an obligation:
/// `UsageCollectorPluginV1::list_usage_records` says the canonical names are
/// "guaranteed to be *present*, not to be last: a caller ordering by `id` is
/// handed on as `(id, window_end)`. A plugin MUST read the order it is given
/// rather than assume a position for either key."
///
/// # Uniform directions
///
/// Only a uniform-direction order renders as a row-value tuple; a
/// mixed-direction one is refused by [`uniform_dir`] before any bind is pushed.
/// The gateway guarantees one does not arrive, so this is the same fail-closed
/// backstop the NULL-safety note below describes, against the same route: a
/// continuation's order is decoded from the cursor's signed tokens, and whatever
/// lets a crafted cursor smuggle in a nullable key lets it smuggle in a mixed
/// direction. The lexicographic OR-form that would serve one is deliberately not
/// emitted, so the breach is refused rather than papered over with a comparison
/// that silently returns the wrong rows.
///
/// # NULL safety
///
/// The row-value tuple comparison is only sound when every tuple column is
/// `NOT NULL`: in SQL three-valued logic a tuple whose column is NULL compares
/// as NULL, so a NULL-keyed row is silently dropped from the page (and a page
/// ending on such a row cannot encode a `next_cursor`). `keyset_safe` is the
/// caller's fail-closed predicate for "this field maps to a never-null column";
/// any field it rejects fails the whole predicate closed. The gateway already
/// rejects a caller `$orderby` on a nullable field with a `400`, so this is the
/// defence-in-depth backstop for a crafted cursor or a future in-process
/// caller.
///
/// # Errors
///
/// Returns an error string when `order_pairs` is empty, its length differs from
/// `cursor_keys`, a field is not keyset-safe (nullable), a field is not on the
/// allowlist, a field has no known kind, the directions are mixed, or a cursor
/// key cannot be parsed.
pub fn keyset_predicate(
    order_pairs: &[(&str, bool)],
    cursor_keys: &[String],
    col: impl Fn(&str) -> Option<&'static str>,
    kind: impl Fn(&str) -> Option<FieldKind>,
    keyset_safe: impl Fn(&str) -> bool,
    ctx: &mut SqlCtx,
) -> Result<String, String> {
    if order_pairs.is_empty() {
        return Err("keyset order must not be empty".to_owned());
    }
    if order_pairs.len() != cursor_keys.len() {
        return Err(format!(
            "cursor key count {} does not match order arity {}",
            cursor_keys.len(),
            order_pairs.len()
        ));
    }

    let dirs = order_pairs
        .iter()
        .map(|(_, asc)| if *asc { SortDir::Asc } else { SortDir::Desc });
    let cmp = match uniform_dir(dirs)? {
        SortDir::Asc => ">",
        SortDir::Desc => "<",
    };

    let mut columns = Vec::with_capacity(order_pairs.len());
    let mut placeholders = Vec::with_capacity(order_pairs.len());
    for ((field, _), raw) in order_pairs.iter().zip(cursor_keys.iter()) {
        if !keyset_safe(field) {
            return Err(format!(
                "keyset field is nullable and cannot be a keyset ordering key: {field}"
            ));
        }
        let column = col(field).ok_or_else(|| format!("keyset field not allowlisted: {field}"))?;
        let field_kind =
            kind(field).ok_or_else(|| format!("keyset field has no known kind: {field}"))?;
        let bind = cursor_key_to_bind(field_kind, raw)?;
        let n = ctx.push(bind);
        columns.push(column);
        placeholders.push(format!("${n}"));
    }

    Ok(format!(
        "({}) {cmp} ({})",
        columns.join(", "),
        placeholders.join(", ")
    ))
}

/// Parse a raw cursor key string into a typed bind according to the keyset
/// field's declared [`FieldKind`] — not its column name, so a new keyset column
/// gets the correct bind type for free and an unsupported one fails loudly
/// rather than silently binding as text (a `column op text` runtime error).
///
/// Every keyset-eligible column today is `Uuid`, `DateTimeUtc`, or `String`;
/// any other kind returns an error (fail-closed) until keyset support for it is
/// deliberately added. (`SqlCtx::push` is private, so callers go through
/// [`keyset_predicate`]; this helper is exposed for testing and reuse.)
///
/// # Errors
///
/// Returns an error string when the value cannot be parsed for its kind, or when
/// the kind is not supported as a keyset column.
pub fn cursor_key_to_bind(kind: FieldKind, raw: &str) -> Result<SqlBind, String> {
    match kind {
        FieldKind::DateTimeUtc => OffsetDateTime::parse(raw, &Rfc3339)
            .map(SqlBind::DateTime)
            .map_err(|e| format!("invalid datetime cursor key `{raw}`: {e}")),
        FieldKind::Uuid => Uuid::from_str(raw)
            .map(SqlBind::Uuid)
            .map_err(|e| format!("invalid uuid cursor key `{raw}`: {e}")),
        FieldKind::String => Ok(SqlBind::Str(raw.to_owned())),
        other => Err(format!(
            "cursor key kind `{other}` is not supported as a keyset column"
        )),
    }
}

/// Decode a cursor token. Thin wrapper over [`CursorV1::decode`].
///
/// # Errors
///
/// Returns the `toolkit_odata::Error` surfaced by [`CursorV1::decode`]
/// (malformed base64 / JSON / version / direction / keys).
pub fn decode_cursor(token: &str) -> Result<CursorV1, toolkit_odata::Error> {
    CursorV1::decode(token)
}
