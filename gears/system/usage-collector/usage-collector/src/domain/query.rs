//! Query read-path helpers for the usage-collector domain service.
//!
//! Holds the helpers that serve only the `list_usage_records` /
//! `query_aggregated_usage_records` read paths, kept out of `service.rs`
//! so it stays focused on orchestration:
//!
//! * `compose_query_with_scope` — AND-merges the PDP-returned
//!   [`AccessScope`] into the caller's `$filter`.
//! * `reject_reserved_filter_fields` — rejects a `$filter` naming a field
//!   reserved to a typed parameter (`gts_type_id`, the covered-period
//!   bounds), wherever in the AST it appears.
//! * `require_dimensions_declared` — checks a `group_by` list only names
//!   the fixed dimensions or a metadata key the queried meter's resolved
//!   declaration actually declares.
//! * `require_metadata_filter_keys_declared` — the same check for
//!   `metadata_filter`, the dynamic-key side channel that exists precisely
//!   because the `toolkit-odata` grammar cannot express filters over JSON
//!   map keys, so it never flows through `$filter` at all.
//! * `read_fingerprint` / `require_cursor_fingerprint` — the query a
//!   keyset continuation is bound to (the caller's `$filter` plus all
//!   three typed parameters: `gts_type_id`, the read range and
//!   `metadata_filter`), and the refusal of a cursor minted over a
//!   different one.
//! * `establish_keyset_order` / `require_continuation_keyset` — the raw
//!   path's keyset floor, in its two modes. Between them they guarantee
//!   every dispatch carries the non-empty, uniform-direction, never-null
//!   order naming both canonical keyset fields that the Plugin SPI
//!   promises, whichever surface the call came in on. A first page has its
//!   order *established*; a continuation has the token's order *required*
//!   to be one already.
//!
//! Per Spec §3.11, the admissible filter and grouping surface is the fixed
//! fields (via `$filter`, gated by [`reject_reserved_filter_fields`]) plus
//! the queried meter's declared metadata keys (via `group_by` and
//! `metadata_filter`, gated by [`require_dimensions_declared`] and
//! [`require_metadata_filter_keys_declared`] respectively) — recomputed
//! per request from the resolved declaration, never cached independently
//! of it, so a property declared a moment ago is usable on the very next
//! call.

use std::collections::BTreeSet;

use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, SortDir, ast};
use toolkit_security::AccessScope;
use usage_collector_sdk::{
    AggregationDimension, MetadataFilter, MeterTypeId, RECORD_ID_FIELD, TimeRange,
    UsageCollectorError, WINDOW_END_FIELD, WINDOW_START_FIELD, is_keyset_safe_record_field,
};

use crate::domain::authz;

/// AND-merge the PDP-returned [`AccessScope`] into the caller's
/// [`ODataQuery`] filter under intersection-only semantics, returning a
/// fresh query ready for plugin dispatch.
///
/// `composed_filter = user_filter AND scope_filter`. The scope always
/// contributes a narrowing predicate: [`authz::scope_to_odata_filter`]
/// fails closed on an unconstrained / empty-constraint / deny-all scope
/// rather than yielding a pass-through, so there is no "filter unchanged"
/// branch. When the user supplied no filter the scope filter alone becomes
/// the composed filter, which is what makes an empty `$filter` a complete
/// request. The order / limit / cursor / select projections on
/// [`ODataQuery`] flow through verbatim — the composition only touches the
/// `$filter` AST — and `gts_type_id` and the read range are typed
/// parameters that no [`ODataQuery`] carries as a predicate, so composition
/// cannot narrow, widen, or drop either of them. (The read path does fold
/// both, and the metadata filter, into `filter_hash` afterwards via
/// [`read_fingerprint`], but that is an opaque pagination fingerprint and
/// not a row constraint.)
///
/// Per `cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2`:
/// composition is intersection-only (no widening). PDP constraint
/// shapes outside the supported set (tree predicates, unknown
/// properties, value-type mismatches) bubble up as fail-closed
/// [`AuthorizationDenied`](crate::domain::DomainError::AuthorizationDenied) from
/// [`authz::scope_to_odata_filter`].
// @cpt-algo:cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2:p2
pub(crate) fn compose_query_with_scope(
    user_query: &ODataQuery,
    scope: &AccessScope,
) -> Result<ODataQuery, UsageCollectorError> {
    // @cpt-begin:cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2:p2:inst-constraint-composition-parse-pdp
    // @cpt-begin:cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2:p2:inst-constraint-composition-iterate
    // `scope_to_odata_filter` always yields a narrowing predicate or fails
    // closed: an unconstrained / empty-constraint / deny-all scope is denied,
    // never passed through as "no row narrowing".
    let scope_expr = authz::scope_to_odata_filter(scope).map_err(UsageCollectorError::from)?;
    // @cpt-end:cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2:p2:inst-constraint-composition-iterate
    // @cpt-end:cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2:p2:inst-constraint-composition-parse-pdp

    // @cpt-begin:cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2:p2:inst-constraint-composition-intersect
    let composed_filter: ast::Expr = match user_query.filter().cloned() {
        Some(user_expr) => user_expr.and(scope_expr),
        None => scope_expr,
    };
    // @cpt-end:cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2:p2:inst-constraint-composition-intersect

    let mut composed = user_query.clone();
    // Preserve the incoming `filter_hash` — do NOT re-hash the AND-merged
    // filter. The keyset cursor's `f` field exists to detect the *caller*
    // changing their query between paginated requests; the PDP scope
    // AND-merged here is server-injected and not caller-controlled, so it
    // MUST stay out. Re-hashing to `hash(user AND scope)` would embed a
    // value the follow-up request's own recomputation can never reproduce,
    // breaking keyset pagination with a spurious `FILTER_MISMATCH` 400 the
    // moment PDP returns any row scope (latent until LIST began requiring
    // constraints). `composed` keeps `user_query.filter_hash` from the
    // clone.
    //
    // What the read path then dispatches is not that value: it overwrites
    // `filter_hash` with [`read_fingerprint`], which binds the caller's
    // `$filter` and every typed parameter that selects rows, and the
    // plugin mints that into `next_cursor.f`. The rule here is the same
    // rule stated one layer
    // down — compute the bound value from the caller's query, never the
    // composed one — which is why the preservation still has to hold: a
    // re-hash here would be the composed filter leaking into the
    // fingerprint by the back door.
    composed.filter = Some(Box::new(composed_filter));
    // @cpt-begin:cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2:p2:inst-constraint-composition-return
    Ok(composed)
    // @cpt-end:cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2:p2:inst-constraint-composition-return
}

/// Field names a caller may never name in a `$filter`.
///
/// `gts_type_id` travels as a typed parameter and the covered period as a
/// typed [`TimeRange`](usage_collector_sdk::TimeRange), so a predicate over
/// any of the three would express a second, possibly contradictory,
/// constraint on something already fixed.
///
/// Both covered-period bounds *are* filterable-schema fields
/// ([`usage_collector_sdk::UsageRecordFilterField`]) — that schema doubles
/// as the plugin's field-to-column mapping and the `$orderby` vocabulary,
/// and `window_end` has to resolve to a column for the canonical keyset to
/// mean anything. This guard, not their absence from the schema, is the
/// whole reason a `$filter` cannot reach them.
const RESERVED_FILTER_FIELDS: &[&str] = &[
    // `gts_type_id` has no SDK constant: it is not a filterable-schema
    // field at all, so there is nothing to point at.
    "gts_type_id",
    // The bounds come from the SDK constants rather than being respelled.
    // This is the one host-side list where a typo silently un-reserves a
    // field — the guard would simply stop matching — and it is exactly the
    // drift the constants were introduced to prevent.
    WINDOW_START_FIELD,
    WINDOW_END_FIELD,
];

/// `true` when `name` names a [`RESERVED_FILTER_FIELDS`] entry, ignoring
/// ASCII case.
///
/// Case-insensitive rather than a bare `contains` because the reservation
/// has to be un-evadable: its whole purpose is that no `$filter` naming a
/// reserved field reaches a plugin, and a case-varied spelling would
/// otherwise walk straight past it. That reason is local to this check
/// rather than a gear-wide convention — the `$orderby` guards next door
/// ([`usage_collector_sdk::is_keyset_safe_record_field`] and
/// `toolkit_odata::ODataOrderBy::ensure_tiebreaker`) both match exactly.
///
/// The case-insensitivity is load-bearing rather than theoretical:
/// `window_start` / `window_end` are filterable-schema fields, so a
/// case-varied spelling of one resolves as a legitimate field and would
/// reach a plugin as a real predicate if this comparison were exact.
/// (A case-varied `gts_type_id` would additionally dead-end downstream as
/// `toolkit_odata`'s own case-insensitive `UnknownField`, since that name
/// is off the schema entirely — but the bounds have no such second net.)
fn is_reserved_filter_field(name: &str) -> bool {
    RESERVED_FILTER_FIELDS
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

/// Rejects a `$filter` naming a [`RESERVED_FILTER_FIELDS`] identifier,
/// wherever in the AST it appears.
///
/// Walks the **whole** tree rather than only top-level conjuncts: a
/// reserved identifier nested under an `or` (or a `not`, or an `in` list)
/// is just as much a constraint on the reserved field as one at the top
/// level, so a top-level-only check would let it through.
///
/// # Errors
///
/// Returns [`UsageCollectorError::InvalidArgument`] naming the offending
/// field.
pub(crate) fn reject_reserved_filter_fields(filter: &ast::Expr) -> Result<(), UsageCollectorError> {
    match filter {
        ast::Expr::Identifier(name) if is_reserved_filter_field(name) => {
            Err(UsageCollectorError::reserved_filter_field(name))
        }
        ast::Expr::Identifier(_) | ast::Expr::Value(_) => Ok(()),
        ast::Expr::Not(inner) => reject_reserved_filter_fields(inner),
        ast::Expr::And(left, right) | ast::Expr::Or(left, right) => {
            reject_reserved_filter_fields(left)?;
            reject_reserved_filter_fields(right)
        }
        ast::Expr::Compare(left, _op, right) => {
            reject_reserved_filter_fields(left)?;
            reject_reserved_filter_fields(right)
        }
        ast::Expr::In(left, items) => {
            reject_reserved_filter_fields(left)?;
            items.iter().try_for_each(reject_reserved_filter_fields)
        }
        ast::Expr::Function(_name, args) => args.iter().try_for_each(reject_reserved_filter_fields),
    }
}

/// The two fields every raw-list order must name for its sort tuple to be
/// a sound keyset.
///
/// `window_end` is the primary time key — the same column the read range
/// selects on (`cpt-cf-usage-collector-adr-window-end-selection`), so one
/// index serves both the selection and the page order. `id` is unique per
/// row, so an order naming it can never leave a page boundary inside a run
/// of rows sharing every other key.
///
/// These are *membership* requirements, not positions.
/// [`establish_keyset_order`] appends whichever is missing, so an order
/// naming neither ends in `(window_end, id)` — but a caller order that
/// already names `id` keeps it where it put it (`$orderby=id` becomes
/// `(id, window_end)`). What is guaranteed downstream is that both names
/// are present, which is what makes the tuple unique; where they sit is
/// not.
const CANONICAL_KEYSET_FIELDS: &[&str] = &[WINDOW_END_FIELD, RECORD_ID_FIELD];

/// Why an order cannot serve as a keyset, independent of which surface it
/// arrived on.
///
/// Both modes of the floor share these two rules and differ only in what
/// they do about a shortfall, so the rules live here once and each mode
/// renders them into the error its own caller can act on.
enum KeysetDefect {
    /// The named key sorts against the leading key's direction.
    MixedDirection(String),
    /// The named key is not a never-null record attribute — or is not a
    /// record attribute at all, since
    /// [`is_keyset_safe_record_field`] is a fail-closed allowlist.
    InadmissibleKey(String),
}

/// The two properties an order must have before it can be a keyset at all,
/// neither of which appending a field could repair.
///
/// * **One direction.** The plugin's continuation is a row-value tuple
///   comparison (`(c1, c2, …) > ($…)`), which has no meaning across mixed
///   directions.
/// * **No nullable key.** A tuple whose leading column is NULL compares as
///   NULL in SQL's three-valued logic, so every NULL-keyed row silently
///   drops out of the page and a page ending on one cannot encode a cursor
///   at all.
fn keyset_defect(order: &ODataOrderBy) -> Option<KeysetDefect> {
    let keys = &order.0;
    if let Some(first) = keys.first()
        && let Some(deviating) = keys.iter().find(|key| key.dir != first.dir)
    {
        return Some(KeysetDefect::MixedDirection(deviating.field.clone()));
    }
    keys.iter()
        .find(|key| !is_keyset_safe_record_field(&key.field))
        .map(|bad| KeysetDefect::InadmissibleKey(bad.field.clone()))
}

/// Establishes the keyset the Plugin SPI promises on a **first page**: a
/// non-empty, uniform-direction, never-null order that names every field
/// in [`CANONICAL_KEYSET_FIELDS`], and so sorts on a globally unique
/// tuple.
///
/// This is the raw path's keyset floor, and it lives here rather than in
/// the REST handler because DESIGN §3.1 allocates order admissibility to
/// the Query Gateway, which §3.2 exists to keep uniform across the SDK and
/// REST. An in-process caller reaches
/// [`Service::list_usage_records`](crate::domain::Service::list_usage_records)
/// with an [`ODataQuery`] of their own construction — an empty order, most
/// of the time — so a normalization that only ran at the REST edge left
/// the SPI's order slot unpopulated on exactly the surface no REST test
/// can reach.
///
/// [`keyset_defect`] must clear first, because neither of the properties
/// it checks can be repaired by appending. Whichever canonical field the
/// order does not already name is then appended in the order's own
/// direction (`Asc` for an empty order) via
/// [`toolkit_odata::ODataOrderBy::ensure_tiebreaker`], which skips a field
/// the order already names — so this is idempotent, and applying it after
/// REST has validated an order is a no-op rather than a double append. It
/// is also why the guarantee is about membership and not position: a
/// caller order already naming `id` keeps it where it is and gains only
/// `window_end` after it.
///
/// `prepare_list_query` calls this too, on the caller's `$orderby` before
/// any authorization or plugin work happens, so a wire request is refused
/// where its input is parsed and the `400` blames the parameter the caller
/// actually sent. That mirroring is the point of the idempotence: the same
/// rule, stated once, applied at the edge for the message and again in the
/// service for the guarantee.
///
/// A continuation goes to [`require_continuation_keyset`] instead — the
/// two are separate functions precisely so a call site says which one it
/// wants rather than letting the presence of a cursor decide silently.
///
/// # Errors
///
/// Returns [`UsageCollectorError::InvalidArgument`] against `$orderby`
/// when the order mixes sort directions or names a key that is not a
/// mandatory record attribute.
pub(crate) fn establish_keyset_order(query: &mut ODataQuery) -> Result<(), UsageCollectorError> {
    match keyset_defect(&query.order) {
        Some(KeysetDefect::MixedDirection(field)) => {
            return Err(UsageCollectorError::mixed_direction_order(&field));
        }
        Some(KeysetDefect::InadmissibleKey(field)) => {
            return Err(UsageCollectorError::inadmissible_order_key(&field));
        }
        None => {}
    }

    let dir = query.order.0.last().map_or(SortDir::Asc, |key| key.dir);
    let mut order = std::mem::take(&mut query.order);
    for &field in CANONICAL_KEYSET_FIELDS {
        order = order.ensure_tiebreaker(field, dir);
    }
    query.order = order;
    Ok(())
}

/// Admits a continuation, or refuses it: the three rules a cursor request
/// must satisfy before its query reaches a plugin.
///
/// One call site, three rules, because they all constrain the same
/// artifact and a caller cannot satisfy some of them:
///
/// 1. [`bind_continuation_order`] replaces the order with the one the
///    token was minted under, so the sort the plugin performs and the
///    boundary values it compares against come from the same place.
/// 2. [`require_continuation_keyset`] refuses that order if it is not a
///    sound keyset — checking it, never extending it.
/// 3. [`require_cursor_fingerprint`] refuses a token minted over a
///    different query.
///
/// They stay separate functions, each with its own upstream error and its
/// own tests, because they are actionable differently: 2 says the token is
/// structurally unusable, 3 says it belongs to another query. Structure
/// before relevance, and binding before both — a rule about the order
/// cannot be applied to an order that has not been established yet.
///
/// The gear declares neither wire code. Each refusal carries the
/// `toolkit_odata` error that owns it — `InvalidCursor` for structure,
/// `FilterMismatch` for relevance — and the host lift converts that error
/// to obtain the field and the code the caller reads (Spec §3.13).
///
/// # Errors
///
/// Returns [`UsageCollectorError::CursorRejected`] from whichever rule
/// refuses first.
pub(crate) fn admit_continuation(
    query: &mut ODataQuery,
    fingerprint: &str,
) -> Result<(), UsageCollectorError> {
    bind_continuation_order(query)?;
    require_continuation_keyset(query)?;
    let cursor = query
        .cursor
        .as_ref()
        .ok_or_else(|| UsageCollectorError::inadmissible_cursor_keyset("it carries no cursor"))?;
    require_cursor_fingerprint(cursor, fingerprint)
}

/// Replaces `query.order` with the order the continuation token was minted
/// under, decoded from its own signed tokens.
///
/// The plugin reads `query.order` to build **both** the `ORDER BY` and the
/// keyset continuation predicate, and compares that predicate against the
/// boundary values in `CursorV1::k` — which are one per key **of the order
/// the token was minted under**. So the two have to be the same order, and
/// the token is the only one of the two that cannot have been tampered
/// with independently: `k` and `s` travel together.
///
/// A caller-supplied order on a cursor request is therefore not an input,
/// it is a contradiction, and it is overwritten rather than compared.
/// Overwriting is also what makes this safe to state as a guarantee: an
/// order that merely *agreed* would still leave the caller's spelling in
/// the slot, and nothing downstream re-derives it.
///
/// This is the half of [`toolkit_odata::validate_cursor_against`] that
/// concerns the order, and it lives here for the same reason the
/// fingerprint does: the REST extractor leaves `query.order` empty on a
/// cursor request, so the handler's own derivation happens to be
/// equivalent — but an **in-process** caller sets `order` and `cursor`
/// independently, and an edge-only derivation left that caller able to hand
/// the plugin `(id, window_end)` against boundary values ordered
/// `(window_end, id)`. Both are sound keysets naming both canonical
/// fields, so no structural check catches it, and the result is a
/// misaligned continuation served as a `200`.
///
/// # Errors
///
/// Returns [`UsageCollectorError::CursorRejected`] carrying
/// `toolkit_odata`'s `InvalidCursor` when the token's signed-token payload
/// does not decode into a non-empty order. That is a malformed token — the
/// same class as a truncated or forged one — so it is refused in the
/// cursor's own vocabulary, upstream's, rather than surfacing as an
/// `$orderby` complaint about a parameter the caller cannot send alongside
/// a cursor. The wire field and code follow from that upstream error and
/// are not spelled by this gear (Spec §3.13).
fn bind_continuation_order(query: &mut ODataQuery) -> Result<(), UsageCollectorError> {
    let Some(cursor) = query.cursor.as_ref() else {
        return Ok(());
    };
    match ODataOrderBy::from_signed_tokens(&cursor.s) {
        Ok(order) => {
            query.order = order;
            Ok(())
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                signed_tokens = %cursor.s,
                "usage-collector refused a continuation token whose signed keys do not decode"
            );
            Err(UsageCollectorError::inadmissible_cursor_keyset(format!(
                "its signed keys do not decode into an order ({err})"
            )))
        }
    }
}

/// Requires the order of a **continuation** to be a sound keyset already,
/// rather than making it one.
///
/// A cursor request's order does not come from the caller: it has been
/// reconstructed from the token's own signed keys by
/// [`bind_continuation_order`], which runs immediately before this and is
/// what makes that sentence true on every surface rather than only over
/// REST. The token's boundary values (`CursorV1::k`) line up with it one
/// for one. There is nothing
/// left to normalize — the order either already is the keyset the page was
/// minted under, or the token did not come from a conforming plugin.
/// Appending to it would widen the sort tuple past the boundary values the
/// token carries and hand the plugin a misaligned continuation, which is a
/// silently wrong page where refusing is merely a refused one. Nothing
/// downstream would catch it either:
/// [`toolkit_odata::validate_cursor_against`] never checks the token's
/// width against the order's, and this gear hands it no filter hash at all
/// — whether a token belongs to this query is
/// [`require_cursor_fingerprint`]'s question, not the toolkit's.
///
/// Taking `&ODataQuery` rather than `&mut` is the point — the signature is
/// what says this path appends nothing.
///
/// A conforming plugin cannot trip this. It mints `next_cursor` from the
/// order it was handed, which is a floored one, and
/// `to_signed_tokens` / `from_signed_tokens` round-trip field names and
/// directions exactly — so the reconstructed order passes and this is a
/// no-op. Everything refused here is a forged, truncated or replayed token,
/// or a plugin minting against an order it was not given, which is why the
/// refusal also logs.
///
/// # Errors
///
/// Returns [`UsageCollectorError::CursorRejected`] carrying
/// `toolkit_odata`'s `InvalidCursor` — the sole source of the wire field
/// and code (Spec §3.13) — when the order mixes sort directions, names a
/// key that is not a mandatory record attribute, or does not already name
/// every [`CANONICAL_KEYSET_FIELDS`] entry.
pub(crate) fn require_continuation_keyset(query: &ODataQuery) -> Result<(), UsageCollectorError> {
    let defect = match keyset_defect(&query.order) {
        Some(KeysetDefect::MixedDirection(field)) => {
            format!("its key '{field}' sorts against the leading key")
        }
        Some(KeysetDefect::InadmissibleKey(field)) => {
            format!("its key '{field}' is not a mandatory record attribute")
        }
        None => match CANONICAL_KEYSET_FIELDS
            .iter()
            .find(|field| !query.order.0.iter().any(|key| key.field == **field))
        {
            Some(missing) => format!("it does not name '{missing}'"),
            None => return Ok(()),
        },
    };

    // A caller-visible 400, but the likeliest cause is a plugin minting a
    // `next_cursor` against an order it was not handed — a conformance
    // breach the caller can do nothing about and would otherwise leave no
    // trace. `warn!` rather than the `error!` of a host-invariant breach:
    // a forged or replayed token reaches here too, and that is ordinary
    // hostile input. Same shape as `service::invariant_breach` — log the
    // detail, then return the typed error.
    //
    // The defect and the `signed_tokens` beside it describe the same
    // thing, because `bind_continuation_order` derived the order under
    // inspection from exactly those tokens. Before it did, an in-process
    // caller could produce a log line showing a healthy token next to a
    // defect that came from the caller's own contradictory `order`.
    tracing::warn!(
        defect = %defect,
        signed_tokens = %query
            .cursor
            .as_ref()
            .map_or("<none>", |cursor| cursor.s.as_str()),
        "usage-collector refused a continuation token whose bound order is not a keyset"
    );
    Err(UsageCollectorError::inadmissible_cursor_keyset(defect))
}

/// The fingerprint a keyset continuation is bound to: a 16-character digest
/// of every input that decides which rows the page came from.
///
/// Those inputs are the caller's `$filter` plus all three typed parameters
/// — `gts_type_id`, the read range, and `metadata_filter`. None of the
/// three is a `$filter` conjunct, so `toolkit_odata::short_filter_hash`
/// sees none of them and each has to enter here explicitly.
///
/// `CursorV1::f` and [`ODataQuery::filter_hash`] exist so a caller who
/// changes their query between pages is refused rather than served a
/// continuation minted over a different row set. While the mandatory
/// window lived inside `$filter` the hash covered it for free; it does not
/// any more, and it never covered the meter or the metadata filter. Without
/// all four a page-2 request can carry the same cursor against a different
/// range, a different `metadata.<key>` value, or **another meter entirely**
/// and be served a continuation that means nothing over its own row set —
/// as a `200` nothing downstream can notice.
///
/// # Why a digest and not the pre-image
///
/// The value is serialized into `CursorV1::f`, base64url-encoded into the
/// opaque `cursor` token, and sent back as a **URL query parameter**. So
/// its length is a wire constraint, and returning the pre-image made the
/// cursor grow with the caller's own metadata filter: `MAX_METADATA_FILTERS`
/// and `MAX_METADATA_FILTER_VALUES` cap how *many* keys and values a caller
/// may send, not how long they are, so a request well inside both caps —
/// five keys of twenty UUID values, say — yields a multi-kilobyte cursor
/// and a page-2 URL that a proxy refuses with a `414` the caller cannot act
/// on. Page one succeeds, page two does not, and nothing in the gear is
/// involved in the failure. Hashing makes the value's length a constant of
/// this function instead of a function of the caller's input.
///
/// Correctness is unaffected either way, which is exactly why the size
/// argument has to be made on its own: a digest compares equal precisely
/// when the pre-image does, up to collision, and
/// [`read_fingerprint_pre_image`] is what rules the collisions out.
///
/// [`toolkit_odata::fnv1a_64`] rather than a local hash: it is the same
/// algorithm and the same 16-hex rendering `short_filter_hash` already
/// produces for the `$filter` field nested inside the pre-image, so there
/// is one hashing primitive in this value rather than two that could
/// drift. FNV-1a is a fixed public specification, so the digest is stable
/// across builds, platforms and replicas — the property a value compared
/// on a *later* request cannot do without.
///
/// # Ownership
///
/// Computed from the **caller's** query, never the composed one. The PDP
/// scope is AND-merged into `$filter` by [`compose_query_with_scope`] and
/// is server-injected rather than caller-controlled, so hashing the
/// composed filter would embed a value the next request's recomputation can
/// never reproduce — the same reasoning that function documents for why it
/// preserves the caller's `filter_hash` rather than re-hashing it, and just
/// as silent until a PDP scope actually appears.
///
/// The value round-trips across a page boundary: the read path computes it
/// here from the caller's query, then assigns it onto the composed query it
/// dispatches — replacing the caller's own `filter_hash` that
/// [`compose_query_with_scope`] preserved — a conforming plugin mints it
/// into `next_cursor.f`, and the follow-up request recomputes the same
/// string from its own parameters.
pub(crate) fn read_fingerprint(
    gts_type_id: &MeterTypeId,
    time_range: TimeRange,
    user_query: &ODataQuery,
    metadata_filter: &[MetadataFilter],
) -> String {
    let pre_image =
        read_fingerprint_pre_image(gts_type_id, time_range, user_query, metadata_filter);
    format!("{:016x}", toolkit_odata::fnv1a_64(pre_image.as_bytes()))
}

/// The exact bytes [`read_fingerprint`] digests.
///
/// Split out from the digest so a change to the rendering is diagnosable:
/// a golden test pins this string, and a second pins the digest over it, so
/// an edit that moves the fingerprint says *which layer* moved instead of
/// only "the hash changed".
///
/// Every field is length-prefixed as `<len>:<bytes>`, making the
/// concatenation self-delimiting — a netstring. That is load-bearing, and
/// **more** so under hashing rather than less: a collision here is a
/// collision in the digest, and nothing downstream could tell the two
/// queries apart. None of the fields is separator-free, so no single
/// separator would do. A GTS type reference carries `~` as its own
/// terminator, [`TimeRange::canonical_form`] joins its two bounds with `~`,
/// and a metadata key or value is domain-opaque — `MetadataKey::new`
/// rejects only the empty string and NUL, so a key may contain any other
/// byte and a value is unconstrained. Whatever character were chosen, one
/// field's content could imitate a field boundary and two different
/// queries would render alike, so a cursor minted under one would validate
/// against the other. A decimal length makes injectivity hold for
/// arbitrary content instead of resting on a claim about what callers send.
///
/// The two count fields are part of that, not decoration: without the
/// per-entry value count, `[a → {b}, c → {d}]` and `[a → {b, c, d}]`
/// render identically.
///
/// `metadata_filter` is **normalized** before it is rendered, because the
/// fingerprint has to be a function of the query's meaning and not of how
/// the caller happened to spell it. A REST caller's repeated
/// `metadata.<key>` parameters reach `MetadataFilter::values` in
/// query-string order, duplicates included (`parse_metadata_filters` groups
/// through a `BTreeMap`, so it sorts keys but pushes values as they
/// arrive), and an in-process caller can build the slice in any order at
/// all. Since the semantics are OR within a key and AND across every
/// filter, two spellings that differ only in order or in a repeated value
/// are the same query — so values are sorted and deduplicated, entries
/// sorted by key, and wholly identical entries collapsed (`X AND X` is
/// `X`). Two entries on the same key with *different* value sets are
/// deliberately left as two: the storage plugin emits one AND-ed clause per
/// entry, so `k in {a} AND k in {b}` is not `k in {a, b}`.
fn read_fingerprint_pre_image(
    gts_type_id: &MeterTypeId,
    time_range: TimeRange,
    user_query: &ODataQuery,
    metadata_filter: &[MetadataFilter],
) -> String {
    // `short_filter_hash` returns `None` for an absent filter, and an
    // absent filter is a legitimate complete request — so it folds in as
    // the empty string rather than short-circuiting the rest out of the
    // pre-image.
    let filter = toolkit_odata::short_filter_hash(user_query.filter()).unwrap_or_default();

    let mut entries: Vec<(&str, Vec<&str>)> = metadata_filter
        .iter()
        .map(|filter| {
            let mut values: Vec<&str> = filter.values().iter().map(String::as_str).collect();
            values.sort_unstable();
            values.dedup();
            (filter.key().as_str(), values)
        })
        .collect();
    entries.sort_unstable();
    // Only wholly identical entries collapse, which is why this runs after
    // the value normalization above: `X AND X` is `X`, while two entries on
    // one key with different value sets mean something a merge would lose.
    entries.dedup();

    let mut out = String::new();
    push_fingerprint_field(&mut out, gts_type_id.as_str());
    push_fingerprint_field(&mut out, &filter);
    push_fingerprint_field(&mut out, &time_range.canonical_form());
    push_fingerprint_field(&mut out, &entries.len().to_string());
    for (key, values) in entries {
        push_fingerprint_field(&mut out, key);
        push_fingerprint_field(&mut out, &values.len().to_string());
        for value in values {
            push_fingerprint_field(&mut out, value);
        }
    }
    out
}

/// Append one length-prefixed `<len>:<bytes>` field to a fingerprint
/// pre-image. See [`read_fingerprint_pre_image`] for why the length is
/// there.
fn push_fingerprint_field(out: &mut String, field: &str) {
    out.push_str(&field.len().to_string());
    out.push(':');
    out.push_str(field);
}

/// Requires a continuation token to have been minted over the query now
/// carrying it.
///
/// Refuses a token whose `f` is absent as well as one that differs.
/// [`toolkit_odata::validate_cursor_against`] skips its own comparison
/// whenever either side is `None`, which is exactly the hole this exists to
/// close: the cursor is caller-supplied JSON, so "no fingerprint recorded"
/// is not evidence of a matching query, and a conforming plugin always has
/// one to mint — the raw read path assigns [`read_fingerprint`] onto every
/// `list_usage_records` dispatch, first page included. (Only that path:
/// the aggregate path paginates nothing, so it mints no cursor and needs
/// no fingerprint.)
///
/// Separate from [`require_continuation_keyset`] because the two answer
/// different questions about the same token and are actionable differently:
/// that one asks whether the token's order could be a keyset at all
/// (structure, `toolkit_odata`'s `InvalidCursor`), this one whether the
/// token belongs to this query (relevance, its `FilterMismatch`). Both run
/// on the continuation branch, structure first.
///
/// # Errors
///
/// Returns [`UsageCollectorError::CursorRejected`] carrying
/// `toolkit_odata`'s `FilterMismatch` — the sole source of the wire field
/// and code (Spec §3.13) — when the token carries no fingerprint or a
/// different one.
pub(crate) fn require_cursor_fingerprint(
    cursor: &CursorV1,
    fingerprint: &str,
) -> Result<(), UsageCollectorError> {
    match cursor.f.as_deref() {
        Some(bound) if bound == fingerprint => Ok(()),
        // Absent, not merely different. A conforming plugin always has a
        // value to mint — the read path assigns one onto every dispatch —
        // so `None` is the shape a plugin that never learned about the
        // field emits, and calling that "you changed your query" would
        // misdiagnose a conformance breach as caller error. It still
        // refuses: the cursor is caller-supplied JSON, so an absent
        // fingerprint is not evidence of a matching query.
        //
        // Logged for the same reason `require_continuation_keyset` logs.
        // The `next_cursor.f` obligation is the one requirement in this
        // gear's Plugin SPI that gives an implementor no compiler error —
        // a plugin written before it recompiles clean and paginates
        // exactly once — so a mismatch that left no operator-side trace
        // would surface only as a `400` blamed on the caller.
        bound => {
            tracing::warn!(
                bound_fingerprint = bound.unwrap_or("<none>"),
                expected_fingerprint = %fingerprint,
                likely_cause = if bound.is_none() {
                    "plugin did not carry query.filter_hash into next_cursor.f"
                } else {
                    "caller changed the query, or plugin recomputed the fingerprint"
                },
                "usage-collector refused a continuation token bound to another query"
            );
            Err(UsageCollectorError::cursor_query_mismatch())
        }
    }
}

/// Checks every `group_by` dimension is either a fixed field (no
/// declaration needed) or a metadata property `declared_keys` actually
/// declares.
///
/// `declared_keys` is read from the resolved declaration fresh for this
/// request (see [`crate::domain::type_resolver::CompiledMetadataSchema::declared_keys`]),
/// never cached independently of it — so a property declared a moment ago
/// is usable on the very next call, per Spec §3.11. `gts_type_id` is the
/// queried meter, carried onto the error for operator-log parity with
/// [`UsageCollectorError::unknown_metadata_key`].
///
/// # Errors
///
/// Returns [`UsageCollectorError::InvalidArgument`] naming the first
/// undeclared metadata dimension.
pub(crate) fn require_dimensions_declared(
    dimensions: &[AggregationDimension],
    declared_keys: &BTreeSet<String>,
    gts_type_id: &MeterTypeId,
) -> Result<(), UsageCollectorError> {
    for dim in dimensions {
        if let AggregationDimension::Metadata(key) = dim
            && !declared_keys.contains(key.as_str())
        {
            return Err(UsageCollectorError::undeclared_metadata_dimension(
                gts_type_id,
                key.as_str(),
            ));
        }
    }
    Ok(())
}

/// Checks every `metadata_filter` entry names a metadata key
/// `declared_keys` actually declares.
///
/// `metadata_filter` is the dynamic-key side channel that exists precisely
/// because the `toolkit-odata` grammar cannot express a filter over a JSON
/// map key — it never flows through `$filter` at all, so
/// [`reject_reserved_filter_fields`] and this check gate two disjoint
/// surfaces. Without this check an undeclared key would silently narrow
/// the result set to nothing (a plugin equality-filters on a column /
/// property that no row carries) rather than fail with an actionable 400.
///
/// `declared_keys` is read from the resolved declaration fresh for this
/// request, never cached independently of it — so a property declared a
/// moment ago is usable on the very next call, per Spec §3.11.
///
/// # Errors
///
/// Returns [`UsageCollectorError::InvalidArgument`] (via
/// [`UsageCollectorError::unknown_metadata_key`]) naming the first
/// undeclared key.
pub(crate) fn require_metadata_filter_keys_declared(
    metadata_filter: &[MetadataFilter],
    declared_keys: &BTreeSet<String>,
    gts_type_id: &MeterTypeId,
) -> Result<(), UsageCollectorError> {
    for filter in metadata_filter {
        let key = filter.key().as_str();
        if !declared_keys.contains(key) {
            return Err(UsageCollectorError::unknown_metadata_key(gts_type_id, key));
        }
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "query_tests.rs"]
mod query_tests;
