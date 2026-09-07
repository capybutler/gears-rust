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
    UsageCollectorError, WINDOW_END_FIELD, is_keyset_safe_record_field,
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
    // plugin mints that into
    // `next_cursor.f`. The rule here is the same rule stated one layer
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
const RESERVED_FILTER_FIELDS: &[&str] = &["gts_type_id", "window_start", "window_end"];

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

/// Requires the order of a **continuation** to be a sound keyset already,
/// rather than making it one.
///
/// A cursor request's order does not come from the caller: it has been
/// reconstructed from the token's own signed keys, and the token's boundary
/// values (`CursorV1::k`) line up with it one for one. There is nothing
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
/// Returns [`UsageCollectorError::InvalidArgument`] against `cursor`, with
/// `INVALID_CURSOR`, when the order mixes sort directions, names a key
/// that is not a mandatory record attribute, or does not already name every
/// [`CANONICAL_KEYSET_FIELDS`] entry.
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

/// The fingerprint a keyset continuation is bound to: every input that
/// decides which rows the page came from.
///
/// That is the caller's `$filter` plus all three typed parameters —
/// `gts_type_id`, the read range, and `metadata_filter`. None of the three
/// is a `$filter` conjunct, so `toolkit_odata::short_filter_hash` sees none
/// of them and each has to enter here explicitly.
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
/// Computed from the **caller's** query, never the composed one. The PDP
/// scope is AND-merged into `$filter` by [`compose_query_with_scope`] and
/// is server-injected rather than caller-controlled, so hashing the
/// composed filter would embed a value the next request's recomputation can
/// never reproduce — the same reasoning that function documents for why it
/// preserves the caller's `filter_hash` rather than re-hashing it, and just
/// as silent until a PDP scope actually appears.
///
/// The value round-trips across a page boundary on every surface: the read
/// path computes it here from the caller's query, then assigns it onto the
/// composed query it dispatches — replacing the caller's own `filter_hash`
/// that [`compose_query_with_scope`] preserved — a conforming plugin mints
/// it into `next_cursor.f`, and the follow-up request recomputes the same
/// string from its own parameters.
///
/// The range contributes [`TimeRange::canonical_form`] rather than a second
/// hash: the whole fingerprint is opaque to callers, so a second hashing
/// primitive would buy nothing and add one more thing that can disagree
/// with itself.
///
/// `metadata_filter` is **normalized** before it is rendered, because the
/// fingerprint has to be a function of the query's meaning and not of how
/// the caller happened to spell it. A REST caller's repeated
/// `metadata.<key>` parameters reach `MetadataFilter::values` in
/// query-string order, duplicates included (`parse_metadata_filters` groups
/// through a `BTreeMap`, so it sorts keys but pushes values as they
/// arrive), and an in-process caller can build the slice in any order at
/// all. Since the semantics are OR within a key and AND across keys, two
/// spellings that differ only in order or in a repeated value are the same
/// query — so values are sorted and deduplicated, and entries sorted by
/// key. Two entries on the *same* key are deliberately left as two: `k in
/// {a} AND k in {b}` is not `k in {a, b}`.
pub(crate) fn read_fingerprint(
    gts_type_id: &MeterTypeId,
    time_range: TimeRange,
    user_query: &ODataQuery,
    metadata_filter: &[MetadataFilter],
) -> String {
    // `short_filter_hash` returns `None` for an absent filter, and an
    // absent filter is a legitimate complete request — so it folds in as
    // the empty string rather than short-circuiting the rest out of the
    // fingerprint.
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

/// Append one length-prefixed `<len>:<bytes>` field to a fingerprint.
///
/// Length-prefixed rather than separator-joined because none of the fields
/// is separator-free. A GTS type reference carries `~` as its own
/// terminator, [`TimeRange::canonical_form`] already joins its two bounds
/// with `~`, and a metadata key or value is domain-opaque —
/// `MetadataKey::new` rejects only the empty string and NUL, so a key may
/// contain any other byte, and a value is unconstrained. Whatever single
/// character were chosen, one field's content could imitate a field
/// boundary and two different queries would fingerprint identically: a
/// cursor minted under one would then validate against the other, which is
/// the failure the fingerprint exists to prevent. A decimal length makes
/// the concatenation self-delimiting, so injectivity holds for arbitrary
/// content instead of resting on a claim about what callers send.
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
/// one to mint — the read path assigns [`read_fingerprint`] onto every
/// query it dispatches, first page included.
///
/// Separate from [`require_continuation_keyset`] because the two answer
/// different questions about the same token and are actionable differently:
/// that one asks whether the token's order could be a keyset at all
/// (structure, `INVALID_CURSOR`), this one whether the token belongs to
/// this query (relevance, `FILTER_MISMATCH`). Both run on the continuation
/// branch, structure first.
///
/// # Errors
///
/// Returns [`UsageCollectorError::InvalidArgument`] against `cursor`, with
/// `FILTER_MISMATCH`, when the token carries no fingerprint or a different
/// one.
pub(crate) fn require_cursor_fingerprint(
    cursor: &CursorV1,
    fingerprint: &str,
) -> Result<(), UsageCollectorError> {
    if cursor.f.as_deref() == Some(fingerprint) {
        return Ok(());
    }
    Err(UsageCollectorError::cursor_query_mismatch())
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
