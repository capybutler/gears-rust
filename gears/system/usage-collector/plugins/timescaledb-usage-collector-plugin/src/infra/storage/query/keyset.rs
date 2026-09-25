//! Order-by rendering, keyset (tuple-comparison) predicates, and cursor
//! encode/decode for keyset pagination.
//!
//! All column identifiers come from a caller-supplied allowlist closure
//! (`record_column` from [`super::translate`]); cursor key values are always
//! bound. The v1 gateway default order is the all-ascending `(window_end, id)`
//! tuple, so [`keyset_predicate`] emits the row-value tuple form for
//! uniform-direction orders. Every entry point here that acts on a sort
//! direction takes it from [`uniform_dir`], so a mixed-direction order — which
//! the gateway guarantees cannot arrive — is refused when the first page is
//! rendered and when a cursor is minted, not only on the continuation.
//!
//! # Verified `toolkit-odata` cursor / order API (Task E1)
//!
//! - `ODataOrderBy(pub Vec<OrderKey>)`; `OrderKey { field: String, dir:
//!   SortDir }`; `SortDir::{Asc, Desc}` with `reverse()`. `ODataOrderBy` has
//!   `to_signed_tokens() -> String` (`"+window_end,+id"`) and
//!   `from_signed_tokens(&str) -> Result<Self, toolkit_odata::Error>`.
//! - `CursorV1 { k: Vec<String>, o: SortDir, s: String, f: Option<String>, d:
//!   String }`; `encode(&self) -> serde_json::Result<String>` (base64url);
//!   `decode(token: &str) -> Result<CursorV1, toolkit_odata::Error>`. `d` is
//!   `"fwd"` / `"bwd"`.
//! - `Page::new(items: Vec<T>, page_info: PageInfo)`; `PageInfo { next_cursor:
//!   Option<String>, prev_cursor: Option<String>, limit: u64 }`.

use std::str::FromStr;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use toolkit_odata::filter::FieldKind;
use toolkit_odata::{CursorV1, ODataOrderBy, SortDir};

use super::bind::SqlBind;
use super::translate::SqlCtx;

/// Reject any cursor whose direction is not forward (`"fwd"`).
///
/// v1 mints and supports only forward cursors. [`keyset_predicate`] derives the
/// `>`/`<` comparison operator from the sort direction, **not** from the
/// cursor's `d` field, so a `"bwd"` cursor would be silently walked forward and
/// return the wrong page. Reject it fail-closed until backward paging is
/// actually implemented.
///
/// # Errors
///
/// Returns an error string when `cursor.d` is anything other than `"fwd"`.
pub fn ensure_forward_cursor(cursor: &CursorV1) -> Result<(), String> {
    if cursor.d == "fwd" {
        Ok(())
    } else {
        Err(format!(
            "unsupported cursor direction `{}`: only forward paging is supported",
            cursor.d
        ))
    }
}

/// The one sort direction an admissible order carries.
///
/// The gateway guarantees `query.order` "uses one sort direction throughout"
/// (`UsageCollectorPluginV1::list_usage_records`), so a mixed-direction order
/// is a breach rather than a shape to serve. Every entry point in this module
/// that acts on a direction resolves it here rather than reading one key's and
/// trusting the rest — the three run at different points of one request, and
/// refusing in only one of them is not refusing:
///
/// - [`render_order_by`] runs on the first page, before any cursor exists, and
///   would otherwise render `window_end ASC, id DESC` and serve it.
/// - [`encode_next_cursor`] runs at mint and would otherwise record the leading
///   key's direction as `o` for the whole order — a token that describes an
///   order its own page was not read in.
/// - [`keyset_predicate`] runs on a continuation only, so on its own it turns a
///   wrongly-ordered served page into a `500` on page two rather than a refusal
///   on page one.
///
/// # Errors
///
/// Returns an error string when `dirs` is empty, or when it carries more than
/// one direction. Two of the three entry points check emptiness first, with
/// their own message — [`keyset_predicate`] and [`encode_next_cursor`], whose
/// messages name what was empty; [`render_order_by`] relies on this one, which
/// emits the string its own guard used to. The empty arm is also the
/// fail-closed floor for a direct caller of this public function.
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
/// This is the caller-supplied `$orderby` path: `col` is handed an untyped
/// caller string here, unlike the `$filter` path where a `FilterField` has
/// already bounded the input, so the allowlist is the whole boundary between
/// that string and the rendered SQL. An unresolved field is refused, never
/// interpolated.
///
/// The direction comes from [`uniform_dir`], not from each key independently:
/// this runs on the first page, so mapping directions one by one would *serve*
/// a mixed-direction order and leave [`keyset_predicate`] to refuse it a page
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
/// The tuple is rendered in the order `order_pairs` arrives in, each bind
/// pushed alongside the key it belongs to; nothing here looks up a canonical
/// name or assumes a canonical slot. That is an obligation, not an incidental
/// property. `UsageCollectorPluginV1::list_usage_records` says of the two
/// canonical names that they are "guaranteed to be *present*, not to be last:
/// a caller ordering by `id` is handed on as `(id, window_end)`. A plugin MUST
/// read the order it is given rather than assume a position for either key."
///
/// # Uniform directions
///
/// Only a uniform-direction order renders as a row-value tuple; a
/// mixed-direction one is refused by [`uniform_dir`] before any bind is
/// pushed. The gateway guarantees one does not arrive — the same SPI doc says
/// `query.order` "uses one sort direction throughout" — so this is the same
/// fail-closed backstop the NULL-safety note below describes, against the same
/// route: a continuation's order is decoded by the gateway from the cursor's
/// signed tokens, and whatever lets a crafted cursor smuggle in a nullable key
/// lets it smuggle in a mixed direction. The lexicographic OR-form that would
/// serve one is deliberately not emitted, so the breach is refused rather than
/// papered over with a tuple comparison that would silently return the wrong
/// rows and report nothing.
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

/// Build and encode the forward (`"fwd"`) cursor for the next page from the
/// last in-page row's key values, in `order` field order.
///
/// `s` carries the signed sort tokens (`"+window_end,+id"`); `o` is the order's
/// one sort direction, resolved through [`uniform_dir`] rather than read off
/// the leading key, so `o` cannot describe an order the page was not read in.
///
/// `f` is **not** an optional extra, and the parameter is `&str` for that
/// reason. The SPI requires that "a `next_cursor` MUST carry that value through
/// verbatim as its `f`", and guarantees `query.filter_hash` is populated on
/// every `list_usage_records` dispatch, first page included — so an absent
/// value is a gateway breach, not an absent option. This signature is the
/// compiler error the gear says does not exist: `require_cursor_fingerprint`
/// calls the obligation "the one requirement in this gear's Plugin SPI that
/// gives an implementor no compiler error — a plugin written before it
/// recompiles clean and paginates exactly once", the gateway refusing the
/// fingerprint-less token on page two. A caller holding an
/// `Option<String>` must now decide what its `None` means before it can call
/// this at all; it may not resolve one to `None` or to `""` without saying so
/// in its own code.
///
/// # Errors
///
/// Returns an error string when the order is empty, its directions are mixed,
/// its arity differs from `last_row_keys`, or serialization fails.
pub fn encode_next_cursor(
    order: &ODataOrderBy,
    last_row_keys: &[String],
    filter_hash: &str,
) -> Result<String, String> {
    if order.is_empty() {
        return Err("cursor order must not be empty".to_owned());
    }
    if order.0.len() != last_row_keys.len() {
        return Err(format!(
            "row key count {} does not match order arity {}",
            last_row_keys.len(),
            order.0.len()
        ));
    }
    let dir = uniform_dir(order.0.iter().map(|key| key.dir))?;
    let cursor = CursorV1 {
        k: last_row_keys.to_vec(),
        o: dir,
        s: order.to_signed_tokens(),
        f: Some(filter_hash.to_owned()),
        d: "fwd".to_owned(),
    };
    cursor
        .encode()
        .map_err(|e| format!("cursor encode failed: {e}"))
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
