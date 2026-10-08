//! The raw path's keyset/cursor lifecycle, and its own small helpers.
//!
//! Split out of `domain/query.rs`, which keeps what both read paths share
//! ([`crate::domain::query`]); this is the raw (`list_usage_records`) path's
//! alone, listed in call order. Declared `pub` (not `pub(crate)`) only
//! because the enclosing `domain/mod.rs` module is itself `pub(crate)` and
//! the workspace denies `clippy::redundant_pub_crate`.
//!
//! * `reattach` — re-attaches the queried meter's identifier to a plugin-
//!   returned record, refusing a foreign-meter row as an invariant breach.
//!
//! **Cursor lifecycle** (gateway-owned per
//! `cpt-cf-usage-collector-dod-gateway-owned-cursor`: the plugin gets a
//! structured [`Keyset`], never a wire token):
//!
//! * `establish_keyset_order` / `require_continuation_keyset` — the keyset
//!   floor: a first page's order is *established* (missing canonical fields
//!   appended); a continuation's is *required* to already be one.
//! * `admit_continuation` — binds the token's order, then runs
//!   `require_continuation_keyset`, `require_forward_cursor` and
//!   `require_cursor_fingerprint` in that order.
//! * `require_forward_cursor` — refuses a non-forward token (`CursorV1::decode`
//!   only checks the direction is well-formed, not which ones this path serves).
//! * `read_fingerprint` / `require_cursor_fingerprint` — the digest a cursor is
//!   bound to (`$filter` + `gts_type_id` + range + `metadata_filter`), and the
//!   refusal of a cursor minted over a different one.
//! * `keyset_from_cursor` — extracts the token's boundary values into [`Keyset`].
//! * `verify_returned_keyset` — checks the plugin's returned keyset before a
//!   token is minted from it.
//! * `mint_record_cursor` — mints the next page's wire token.
//!
//! [`Service::list_usage_records`]'s `impl Service` block lives here too, so
//! the read path sits beside the lifecycle it drives.

use std::collections::BTreeSet;

use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, Page as ODataPage, PageInfo, SortDir};
use toolkit_security::SecurityContext;
use usage_collector_sdk::{
    Keyset, MetadataFilter, MeterRef, MeterTypeId, RECORD_ID_FIELD, RecordPage, StoredUsageRecord,
    TimeRange, UsageCollectorError, UsageRecord, WINDOW_END_FIELD, is_keyset_safe_record_field,
};

use crate::domain::authz;
use crate::domain::error;
use crate::domain::error::{
    DomainError, TargetNotConvergedRetryAfterSecs, UnavailableRetryAfterSecs, lift_domain_error,
};
use crate::domain::fingerprint::fnv1a_64;
use crate::domain::observability::{self, OperationLogEntry, log_operation_completed};
use crate::domain::ports::metrics::{PdpOp, PluginOp, QueryKind};
use crate::domain::query;
use crate::domain::query::{
    compose_query_with_scope, reject_off_label_literals, reject_unpublished_filter_fields,
    require_metadata_filter_keys_declared, require_metadata_filter_within_caps,
};
use crate::domain::service::{DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE, Service};

/// Re-attach `meter`'s identifier to a record the plugin returned.
///
/// Checks the reference first: a row answering under another meter's
/// reference would otherwise be silently relabelled and served as fact —
/// the same class of breach as a mismatched `get_usage_record`, refused the
/// same way (typed `Internal`, never a panic).
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
fn reattach(
    stored: StoredUsageRecord,
    meter: &MeterRef,
) -> Result<UsageRecord, UsageCollectorError> {
    if stored.gts_type_uuid != meter.uuid {
        return Err(error::invariant_breach(error::foreign_reference_detail(
            stored.gts_type_uuid,
            meter,
        )));
    }
    Ok(stored.into_usage_record(meter.id.clone()))
}

/// The fields every raw-list order must name for a sound keyset.
///
/// `window_end` is the primary time key
/// (`cpt-cf-usage-collector-adr-window-end-selection`), so one index serves
/// both the read-range selection and the page order. `id` is unique per
/// row, so naming it rules out a page boundary inside a run of rows
/// sharing every other key.
///
/// Membership only, not position: [`establish_keyset_order`] appends
/// whichever is missing (`$orderby=id` becomes `(id, window_end)`).
const CANONICAL_KEYSET_FIELDS: &[&str] = &[WINDOW_END_FIELD, RECORD_ID_FIELD];

/// Why an order cannot serve as a keyset, independent of which surface it
/// arrived on. Both modes of the floor share these rules and render them
/// into the error their own caller can act on.
enum KeysetDefect {
    /// Sorts against the leading key's direction.
    MixedDirection(String),
    /// Not a never-null record attribute
    /// ([`is_keyset_safe_record_field`] is a fail-closed allowlist).
    InadmissibleKey(String),
    /// Named more than once.
    DuplicateKey(String),
}

/// The properties an order must have before it can be a keyset at all —
/// none of which appending a field could repair.
///
/// * **One direction** — the plugin compares continuation boundaries as a
///   row-value tuple, which has no meaning across mixed directions.
/// * **No nullable key** — a NULL leading column drops rows from the page
///   silently, and a page ending on one can't encode a cursor at all.
/// * **No repeated key** — nothing downstream dedups an order, so a key
///   named twice mints a keyset and cursor twice as wide as a sound one
///   (and can alone breach the cursor's `maxLength`). Mirrors the
///   aggregate path's `group_by` uniqueness gap.
///
/// Only one defect is returned, and the check order is load-bearing: each
/// refusal names one key, so a doubly-defective order must report the
/// actionable one. `MixedDirection` is checked first (a whole-tuple
/// property, repairable independently of any single key); `DuplicateKey`
/// last (naming a key as duplicated is the wrong fix if it was never
/// orderable at any multiplicity). Pinned by `query_tests.rs`'s
/// `the_first_defect_reported_for_a_doubly_defective_order_is_the_actionable_one`.
fn keyset_defect(order: &ODataOrderBy) -> Option<KeysetDefect> {
    let keys = &order.0;
    if let Some(first) = keys.first()
        && let Some(deviating) = keys.iter().find(|key| key.dir != first.dir)
    {
        return Some(KeysetDefect::MixedDirection(deviating.field.clone()));
    }
    if let Some(bad) = keys
        .iter()
        .find(|key| !is_keyset_safe_record_field(&key.field))
    {
        return Some(KeysetDefect::InadmissibleKey(bad.field.clone()));
    }
    let mut seen = BTreeSet::new();
    keys.iter()
        .find(|key| !seen.insert(key.field.as_str()))
        .map(|dup| KeysetDefect::DuplicateKey(dup.field.clone()))
}

/// Establishes the keyset the Plugin SPI promises on a **first page**: a
/// non-empty, uniform-direction, never-null order naming every field in
/// [`CANONICAL_KEYSET_FIELDS`], so it sorts on a globally unique tuple.
///
/// Lives here rather than in the REST handler because DESIGN §3.1/§3.2
/// allocate order admissibility to the Query Gateway and keep it uniform
/// across the SDK and REST — an in-process caller reaches
/// [`Service::list_usage_records`](crate::domain::Service::list_usage_records)
/// with its own `ODataQuery`, often an empty order, bypassing REST
/// entirely.
///
/// [`keyset_defect`] must clear first — neither property it checks can be
/// repaired by appending. `ensure_tiebreaker` then appends whichever
/// canonical field is missing, in the order's own direction, and is a
/// no-op if both are already named (so `prepare_list_query`'s own REST-edge
/// call and this one don't double-append) — which is also why the
/// guarantee is about membership, not position.
///
/// A continuation goes to [`require_continuation_keyset`] instead, so a
/// call site states which it wants rather than branching on cursor
/// presence.
///
/// # Errors
///
/// Returns [`UsageCollectorError::InvalidArgument`] against `$orderby`
/// when the order mixes sort directions, names a key that is not a
/// mandatory record attribute, or names a key more than once.
// @cpt-algo:cpt-cf-usage-collector-algo-query-field-validation:p1
// @cpt-dod:cpt-cf-usage-collector-dod-query-field-validation:p1
// @cpt-algo:cpt-cf-usage-collector-algo-query-cursor-lifecycle:p1
// @cpt-dod:cpt-cf-usage-collector-dod-gateway-owned-cursor:p1
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub fn establish_keyset_order(query: &mut ODataQuery) -> Result<(), UsageCollectorError> {
    match keyset_defect(&query.order) {
        Some(KeysetDefect::MixedDirection(field)) => {
            return Err(UsageCollectorError::mixed_direction_order(&field));
        }
        Some(KeysetDefect::InadmissibleKey(field)) => {
            return Err(UsageCollectorError::inadmissible_order_key(&field));
        }
        Some(KeysetDefect::DuplicateKey(field)) => {
            return Err(UsageCollectorError::duplicate_order_key(&field));
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

/// Admits a continuation, or refuses it.
///
/// One call site, in order, because each rule constrains the whole token
/// and binding must happen first:
/// 1. [`bind_continuation_order`] replaces the order with the one the
///    token was minted under.
/// 2. [`require_continuation_keyset`] refuses that order if it is not a
///    sound keyset.
/// 3. [`require_forward_cursor`] refuses a token whose `d` is not `"fwd"`.
/// 4. [`require_cursor_fingerprint`] refuses a token minted over a
///    different query.
///
/// Kept as separate functions because 2/3 say the token is structurally
/// unusable while 4 says it belongs to another query — different error
/// classes, both `toolkit_odata`'s own (`InvalidCursor` vs
/// `FilterMismatch`); the wire field is this gear's own `CursorField`
/// (DESIGN §3.3).
///
/// # Errors
///
/// Returns [`UsageCollectorError::CursorRejected`] from whichever rule
/// refuses first.
// @cpt-algo:cpt-cf-usage-collector-algo-query-cursor-lifecycle:p1
// @cpt-dod:cpt-cf-usage-collector-dod-gateway-owned-cursor:p1
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
fn admit_continuation(
    query: &mut ODataQuery,
    fingerprint: &str,
) -> Result<(), UsageCollectorError> {
    bind_continuation_order(query)?;
    require_continuation_keyset(query)?;
    let cursor = query
        .cursor
        .as_ref()
        .ok_or_else(|| UsageCollectorError::inadmissible_cursor_keyset("it carries no cursor"))?;
    require_forward_cursor(cursor)?;
    require_cursor_fingerprint(cursor, fingerprint)
}

/// Replaces `query.order` with the order the continuation token was minted
/// under, decoded from its own signed tokens.
///
/// The plugin builds both `ORDER BY` and the keyset predicate from
/// `query.order`, and compares it against `CursorV1::k` — one boundary
/// value per key **of the order the token was minted under**. A
/// caller-supplied order on a cursor request is therefore not an input but
/// a contradiction, and is overwritten rather than compared or merged: a
/// merely-agreeing order would still leave the caller's spelling in the
/// slot, and nothing downstream re-derives it.
///
/// Runs in the service rather than only at the REST edge: REST happens to
/// leave `query.order` empty on a cursor request, but an in-process caller
/// can set `order` and `cursor` independently, which would otherwise let a
/// `(id, window_end)` order dispatch against `(window_end, id)` boundary
/// values — both sound keysets, so no structural check catches it, and the
/// result is a misaligned continuation served as a `200`.
///
/// # Errors
///
/// Returns [`UsageCollectorError::CursorRejected`] carrying
/// `toolkit_odata`'s `InvalidCursor` when the token's signed payload does
/// not decode into a non-empty order — the same class as a truncated or
/// forged token, so it's refused in the cursor's own vocabulary rather than
/// as an `$orderby` complaint about a parameter the caller cannot send
/// alongside a cursor.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
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

/// Requires the order of a **continuation** to already be a sound keyset,
/// rather than making it one.
///
/// [`bind_continuation_order`] has already reconstructed the order from the
/// token's own signed keys, so there is nothing left to normalize —
/// appending here would widen the sort tuple past the token's boundary
/// values and silently misalign the page, which nothing downstream would
/// catch. A conforming plugin can't trip this (mint/decode round-trip field
/// names and directions exactly); every refusal here is a forged, truncated
/// or replayed token.
///
/// Takes `&ODataQuery` rather than `&mut` — the signature is what says this
/// path appends nothing.
///
/// # Errors
///
/// Returns [`UsageCollectorError::CursorRejected`] carrying
/// `toolkit_odata`'s `InvalidCursor` (wire field `CursorField::Cursor`) when
/// the order mixes directions, names a non-mandatory or repeated key, or
/// omits a [`CANONICAL_KEYSET_FIELDS`] entry.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
fn require_continuation_keyset(query: &ODataQuery) -> Result<(), UsageCollectorError> {
    let defect = match keyset_defect(&query.order) {
        Some(KeysetDefect::MixedDirection(field)) => {
            format!("its key '{field}' sorts against the leading key")
        }
        Some(KeysetDefect::InadmissibleKey(field)) => {
            format!("its key '{field}' is not a mandatory record attribute")
        }
        Some(KeysetDefect::DuplicateKey(field)) => {
            format!("its key '{field}' is named more than once")
        }
        None => match CANONICAL_KEYSET_FIELDS
            .iter()
            .find(|field| !query.order.0.iter().any(|key| key.field == **field))
        {
            Some(missing) => format!("it does not name '{missing}'"),
            None => return Ok(()),
        },
    };

    // Likeliest cause is a plugin minting `next_cursor` against an order it
    // wasn't handed — a conformance breach, not caller input — hence
    // `warn!` not `error!`: ordinary forged/replayed tokens land here too.
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

/// Refuses a continuation whose `d` is not `"fwd"`.
///
/// `CursorV1::decode` only checks `d` is one of `{"fwd", "bwd"}` — it has no
/// notion of which directions a read path supports — so without this guard
/// a correctly-bound `d: "bwd"` token would serve a forward page with
/// `200`. The raw path mints `"fwd"` exclusively and is forward-only by
/// design (`usage-collector-v1.yaml`'s `Cursor` parameter), so a `"bwd"`
/// token here is forged, replayed, or hand-built.
///
/// # Errors
///
/// Returns [`UsageCollectorError::CursorRejected`] carrying
/// `toolkit_odata`'s `InvalidCursor` when `cursor.d` is not `"fwd"`.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
fn require_forward_cursor(cursor: &CursorV1) -> Result<(), UsageCollectorError> {
    if cursor.d == "fwd" {
        return Ok(());
    }
    tracing::warn!(
        direction = %cursor.d,
        "usage-collector refused a continuation token whose direction is not forward"
    );
    Err(UsageCollectorError::unsupported_cursor_direction(format!(
        "it specifies direction `{}`; the raw path reads forward only",
        cursor.d
    )))
}

/// The fingerprint a keyset continuation is bound to: a 16-character digest
/// of every input that decides which rows the page came from — the
/// caller's `$filter` plus the typed `gts_type_id`, read range and
/// `metadata_filter` (none of which `toolkit_odata::short_filter_hash` sees,
/// since none is a `$filter` conjunct). [`require_cursor_fingerprint`]
/// compares this against `CursorV1::f` on the next request, refusing a
/// caller who changed their query — a different range, meter or metadata
/// filter — between pages.
///
/// A digest rather than the pre-image: the value travels
/// base64url-encoded in a URL query parameter, and the pre-image's length
/// is a function of the caller's own metadata filter — within the declared
/// caps, still enough to make the cursor multi-kilobyte and the page-2 URL
/// `414`-refused by a proxy. Hashing makes the length a constant instead;
/// [`read_fingerprint_pre_image`] rules out the collision risk that comes
/// with it. [`fnv1a_64`] reuses the same primitive as `short_filter_hash`
/// (a fixed public spec), so the digest is stable across builds, platforms
/// and replicas.
///
/// Computed from the **caller's** query, never the composed one — the
/// server-injected PDP scope must stay out
/// ([`crate::domain::query::compose_query_with_scope`]). It never reaches a
/// plugin: held locally, written into the token's `f` by
/// [`mint_record_cursor`], and recomputed from scratch on the follow-up
/// request rather than read back off `composed.filter_hash` (which the SPI
/// forbids a plugin from reading).
pub fn read_fingerprint(
    gts_type_id: &MeterTypeId,
    time_range: TimeRange,
    user_query: &ODataQuery,
    metadata_filter: &[MetadataFilter],
) -> String {
    let pre_image =
        read_fingerprint_pre_image(gts_type_id, time_range, user_query, metadata_filter);
    format!("{:016x}", fnv1a_64(pre_image.as_bytes()))
}

/// Mints the raw path's wire cursor from the keyset a plugin returned.
///
/// The gateway owns minting, decoding and validating — the plugin only
/// returns a [`Keyset`] (`cpt-cf-usage-collector-dod-gateway-owned-cursor`).
/// `s` carries the order's signed tokens, which [`admit_continuation`]
/// reconstructs the order from on the way back, so the two must be the same
/// order — hence `order` here is the one the read was *dispatched* under,
/// not the one the caller sent.
// @cpt-algo:cpt-cf-usage-collector-algo-query-cursor-lifecycle:p1
// @cpt-dod:cpt-cf-usage-collector-dod-gateway-owned-cursor:p1
fn mint_record_cursor(order: &ODataOrderBy, keyset: &Keyset, fingerprint: &str) -> CursorV1 {
    CursorV1 {
        k: keyset.values().to_vec(),
        o: keyset.direction(),
        s: order.to_signed_tokens(),
        f: Some(fingerprint.to_owned()),
        // DESIGN §3.3's `Cursor` parameter: "Forward-only".
        d: "fwd".to_owned(),
    }
}

/// Extracts the structured keyset a continuation token carries, for
/// dispatch.
///
/// Runs after [`admit_continuation`], which has already reconstructed and
/// checked `query.order` — so the only thing left to verify is that
/// `cursor.k`'s arity matches it.
///
/// # Errors
///
/// Returns [`UsageCollectorError::CursorRejected`] when the token's
/// boundary values do not number one per key of the bound order. That is a
/// forged, truncated or replayed token rather than caller input on a
/// parameter they can change, so it is refused in the cursor's own
/// vocabulary.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
fn keyset_from_cursor(
    cursor: &CursorV1,
    order: &ODataOrderBy,
) -> Result<Keyset, UsageCollectorError> {
    if cursor.k.len() != order.0.len() {
        return Err(UsageCollectorError::inadmissible_cursor_keyset(format!(
            "it carries {} boundary values for an order of {} keys",
            cursor.k.len(),
            order.0.len()
        )));
    }
    Keyset::new(cursor.k.clone(), cursor.o)
        .map_err(|err| UsageCollectorError::inadmissible_cursor_keyset(err.to_string()))
}

/// Verifies a plugin's returned page against the dispatched order, before
/// the gateway mints a token from it: one boundary value per key, sorting
/// in the order's direction, and a continuation only on a page with rows.
/// Refused rather than logged because a breach here becomes a token a
/// caller follows.
///
/// `internal` in every arm — unlike the aggregate path's `group_by` cap, no
/// caller request can produce a malformed keyset here; only a
/// non-conforming plugin can. The direction check reads `order`'s leading
/// key directly (already established uniform by [`establish_keyset_order`]
/// or [`require_continuation_keyset`]) rather than through the plugin's own
/// `uniform_dir`, which answers a different question for input the gateway
/// doesn't yet trust.
///
/// # Errors
///
/// [`UsageCollectorError::Internal`] naming the mismatch.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
fn verify_returned_keyset(
    order: &ODataOrderBy,
    page: &RecordPage,
) -> Result<(), UsageCollectorError> {
    let Some(keyset) = page.next.as_ref() else {
        return Ok(());
    };
    if page.items.is_empty() {
        return Err(UsageCollectorError::internal(
            "usage-collector storage plugin returned an empty page carrying a continuation \
             keyset; a continuation names the last row of the page it continues, and a caller \
             following this one would page forever",
        ));
    }
    if keyset.values().len() != order.0.len() {
        return Err(UsageCollectorError::internal(format!(
            "usage-collector storage plugin returned {} keyset boundary values for the {}-key \
             order it was dispatched under; a token minted from this would bind an order its \
             own boundary values cannot line up with",
            keyset.values().len(),
            order.0.len(),
        )));
    }
    // A 0-key `order` can't reach here — `Keyset::new` refuses empty
    // `values`, so the arity check above already caught it. Reaching `None`
    // is this function's own invariant broken, not caller/plugin input.
    let Some(first) = order.0.first() else {
        return Err(UsageCollectorError::internal(
            "usage-collector dispatched an empty order against a non-empty returned keyset; \
             the arity check above should have refused this first, so reaching here is a bug \
             in verify_returned_keyset's own invariants",
        ));
    };
    if keyset.direction() != first.dir {
        return Err(UsageCollectorError::internal(format!(
            "usage-collector storage plugin returned a keyset sorting {:?} against the {:?} \
             order it was dispatched under",
            keyset.direction(),
            first.dir,
        )));
    }
    Ok(())
}

/// The exact bytes [`read_fingerprint`] digests.
///
/// Split out so a rendering change is diagnosable: one golden test pins
/// this string, a second pins the digest over it.
///
/// Every field is length-prefixed (`<len>:<bytes>`, a netstring) rather
/// than separator-joined, because no separator is safe against
/// domain-opaque content (`MetadataKey::new` only rejects empty/NUL) — a
/// collision here is a digest collision two different queries would share
/// a cursor over. The per-entry value count is part of that: without it,
/// `[a → {b}, c → {d}]` and `[a → {b, c, d}]` would render identically.
///
/// `metadata_filter` is normalized (values sorted + deduped, entries sorted
/// by key, identical entries collapsed) before rendering, so the
/// fingerprint is a function of the query's *meaning* rather than of
/// caller spelling order — values are OR'd within a key and AND'd across
/// filters, so reordering or duplicating a value doesn't change the query,
/// but two entries on the same key with *different* value sets stay two
/// (the plugin emits one AND-ed clause per entry: `k in {a} AND k in {b}`
/// is not `k in {a, b}`).
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
/// carrying it — refusing an absent fingerprint as well as a differing one,
/// since `toolkit_odata::validate_cursor_against` skips its own comparison
/// whenever either side is `None` and the cursor is caller-supplied JSON.
/// [`mint_record_cursor`] always sets `f: Some(..)` (only the gear mints,
/// so no implementor can forget it; only the raw path mints at all, since
/// the aggregate path paginates nothing), so `f: None` here names a forged
/// or hand-built token.
///
/// Separate from [`require_continuation_keyset`]: that checks structure
/// (`InvalidCursor`), this checks relevance (`FilterMismatch`) — different
/// error classes, structure checked first.
///
/// # Errors
///
/// Returns [`UsageCollectorError::CursorRejected`] carrying
/// `toolkit_odata`'s `FilterMismatch` (wire field `CursorField::Cursor`)
/// when the token carries no fingerprint or a different one.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
fn require_cursor_fingerprint(
    cursor: &CursorV1,
    fingerprint: &str,
) -> Result<(), UsageCollectorError> {
    match cursor.f.as_deref() {
        Some(bound) if bound == fingerprint => Ok(()),
        // Absent fingerprint still refuses, same as a differing one — the
        // gear always sets `f` on mint, so `None` is forged/hand-built, not
        // a gap a conforming mint could leave. `warn!`, not `error!`: this
        // is ordinary hostile/stale input, not a host-invariant breach.
        bound => {
            tracing::warn!(
                bound_fingerprint = bound.unwrap_or("<none>"),
                expected_fingerprint = %fingerprint,
                likely_cause = if bound.is_none() {
                    "forged, truncated or hand-built token: a token this gear minted always carries f"
                } else {
                    "caller changed the query between pages, or the token was forged"
                },
                "usage-collector refused a continuation token bound to another query"
            );
            Err(UsageCollectorError::cursor_query_mismatch())
        }
    }
}

impl Service {
    /// Keyset-paginated list of `UsageRecord`s from the bound storage
    /// plugin, narrowed by the PDP-returned constraints.
    ///
    /// Seven responsibilities, per
    /// `cpt-cf-usage-collector-flow-query-raw-ledger-page`:
    ///
    /// 1. **Authorize** via [`authz::authorize_list_usage_records`] under
    ///    `require_constraints(true)` — a pre-row PEP request, so a
    ///    degenerate unconstrained permit is denied rather than read as
    ///    "all tenants".
    /// 2. **Resolve** the meter's declaration
    ///    ([`TypeResolver::resolve`](crate::domain::type_resolver::TypeResolver::resolve)),
    ///    fail-closed — an unresolvable type is a pre-dispatch 404.
    /// 3. **Gate** the query surface against that declaration (Spec §3.11's
    ///    four checks that apply without `group_by`; rule and reasoning in
    ///    [`crate::domain::query`]).
    /// 4. **Compose** the PDP constraints into the caller's filter,
    ///    intersection-only, via [`authz::scope_to_odata_filter`]
    ///    (`cpt-cf-usage-collector-algo-read-scope-composition`).
    ///    `gts_type_id`/`time_range` stay typed parameters, never filter
    ///    conjuncts.
    /// 5. **Floor the keyset** — [`establish_keyset_order`] on a first
    ///    page, [`admit_continuation`] on a continuation, run here so every
    ///    surface passes through it.
    /// 6. **Bind the cursor** to its query with [`read_fingerprint`], held
    ///    locally and never carried by the plugin.
    /// 7. **Delegate** to the plugin's `list_usage_records` SPI with the
    ///    composed filter, floored order, bound fingerprint and typed
    ///    `time_range` (`from <= window_end < to`,
    ///    `cpt-cf-usage-collector-adr-window-end-selection`).
    ///
    /// # Errors
    ///
    /// * [`UsageCollectorError::PermissionDenied`] /
    ///   [`UsageCollectorError::ServiceUnavailable`] — PDP denies, is
    ///   unavailable, or returns a constraint shape this gear can't honour.
    /// * [`UsageCollectorError::NotFound`] — `gts_type_id` does not resolve.
    /// * [`UsageCollectorError::InvalidArgument`] — `$filter` names a
    ///   reserved/unpublished field, `metadata_filter` names an undeclared
    ///   key, or the order isn't floorable into a sound keyset. (A
    ///   malformed range can't surface here: [`TimeRange::new`] is its only
    ///   constructor.)
    /// * [`UsageCollectorError::CursorRejected`] — the token's order isn't
    ///   one a conforming plugin could produce, or it was minted over a
    ///   different query.
    /// * Any other [`UsageCollectorError`] lifted from a plugin
    ///   transport/persistence failure.
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    pub async fn list_usage_records(
        &self,
        ctx: &SecurityContext,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorError> {
        let started = std::time::Instant::now();
        let log_referenced_type = gts_type_id.as_str().to_owned();
        let result = self
            .dispatch_usage_records(ctx, gts_type_id, time_range, query, metadata_filter)
            .await;
        let seconds = started.elapsed().as_secs_f64();
        // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-result-rows-observe
        if let Ok(page) = &result {
            self.metrics.observe_query_result_rows(
                QueryKind::Raw,
                u64::try_from(page.items.len()).unwrap_or(u64::MAX),
            );
        }
        // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-result-rows-observe
        // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-telemetry-complete
        let (outcome, error_category) = observability::classify_query_result(&result);
        self.metrics
            .record_query_request(QueryKind::Raw, outcome, error_category, seconds);
        let log_tenant = ctx.subject_tenant_id().to_string();
        log_operation_completed(
            OperationLogEntry::new(PdpOp::QueryRaw, outcome, error_category)
                .tenant(Some(log_tenant.as_str()))
                .referenced_type(Some(log_referenced_type.as_str())),
        );
        // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-telemetry-complete
        result
    }

    /// The request's seven stages, in the order Spec §6 fixes them — split
    /// from the completion telemetry above so every exit, `?` included, is
    /// measured and logged exactly once and the `QueryInflightGuard` drops
    /// before duration is read.
    ///
    /// The plugin is resolved **after** authorization, declaration
    /// resolution and the query-surface gate — Spec §6 orders an
    /// unauthorized caller's rejection ahead of an unavailable plugin's, so
    /// this call must not be hoisted earlier.
    // @cpt-flow:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-query-raw:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-tenant-isolation:p1
    // @cpt-dod:cpt-cf-usage-collector-principle-pdp-centric-authorization:p1
    // @cpt-dod:cpt-cf-usage-collector-principle-fail-closed:p1
    // @cpt-dod:cpt-cf-usage-collector-constraint-no-business-logic:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-authorize-query:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-cross-tenant-read:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-find-withdrawal-of-record:p1
    //
    // Reuses flow-query-raw-ledger-page unchanged for
    // flow-find-withdrawal-of-record: `invalidates` is one of
    // PUBLISHED_FILTER_FIELDS, so a caller narrows on it with no extra code.
    // One dispatch below, no retry, no session/pool affinity, no
    // replication-watermark wait, empty page returned as ordinary success —
    // the raw-list half of `algo-staleness-defensive-read` and of
    // `dod-consistency-no-stronger-read-claim`.
    // @cpt-dod:cpt-cf-usage-collector-dod-consistency-no-stronger-read-claim:p1
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    async fn dispatch_usage_records(
        &self,
        ctx: &SecurityContext,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorError> {
        // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-scope
        // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-attribution
        // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-denied
        let scope = authz::authorize_list_usage_records(
            &self.enforcer,
            self.metrics.as_ref(),
            PdpOp::QueryRaw,
            ctx,
        )
        .await
        .map_err(UsageCollectorError::from)?;
        // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-denied
        // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-attribution
        // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-scope

        // Tracks in-flight for the remainder of this attempt (decremented
        // on every exit below, `?` included).
        let _inflight = query::QueryInflightGuard::enter(self.metrics.as_ref(), QueryKind::Raw);

        // Fail-closed: an unresolvable type is a pre-dispatch 404, never
        // reaching the plugin.
        // @cpt-flow:cpt-cf-usage-collector-flow-resolve-for-read:p1
        let declaration = self.type_resolver.resolve(&gts_type_id).await?;

        // Spec §3.11's query-surface gate (rule and reasoning:
        // [`crate::domain::query`]); `require_dimensions_declared` doesn't
        // apply, this path taking no `group_by`.
        if let Some(filter) = query.filter() {
            reject_unpublished_filter_fields(filter)?;
            reject_off_label_literals(filter)?;
        }
        require_metadata_filter_within_caps(metadata_filter)?;
        require_metadata_filter_keys_declared(
            metadata_filter,
            declaration.metadata_schema.declared_keys(),
            &gts_type_id,
        )?;

        // Computed on the CALLER's query, before composition AND-merges in
        // the server-injected PDP scope — a value the next request could
        // never reproduce.
        let fingerprint = read_fingerprint(&gts_type_id, time_range, query, metadata_filter);

        // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-scope
        let mut composed = compose_query_with_scope(query, &scope)?;
        // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-scope

        // First page: order normalized (idempotent). Continuation: order
        // reconstructed from the token and only checked. Composition above
        // only rewrites `filter`, so the order reaching here is unchanged.
        match composed.cursor {
            Some(_) => admit_continuation(&mut composed, &fingerprint)?,
            None => establish_keyset_order(&mut composed)?,
        }

        let keyset = match composed.cursor.as_ref() {
            Some(cursor) => Some(keyset_from_cursor(cursor, &composed.order)?),
            None => None,
        };

        // The SPI's two MUST-NOTs on this dispatch: `query.cursor` and
        // `query.filter_hash` must not be read by a plugin. Stripped here
        // rather than left for a plugin to ignore, so the guarantee is
        // structural.
        composed.cursor = None;
        composed.filter_hash = None;

        let plugin = self.resolve_plugin_for(PluginOp::ListUsageRecords).await?;

        let meter = MeterRef::new(declaration.type_uuid, gts_type_id.clone());

        // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-dispatch
        // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-plugin-catch
        let page = observability::instrument_spi(
            self.metrics.as_ref(),
            PluginOp::ListUsageRecords,
            plugin.list_usage_records(
                &meter,
                time_range,
                &composed,
                metadata_filter,
                keyset.as_ref(),
            ),
        )
        .await
        // @cpt-begin:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-dispatch-catch
        .map_err(|e| {
            lift_domain_error(
                DomainError::from(e),
                UnavailableRetryAfterSecs(self.unavailable_retry_after_secs),
                TargetNotConvergedRetryAfterSecs(self.target_not_converged_retry_after_secs),
            )
        })?;
        // @cpt-end:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-dispatch-catch
        // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-plugin-catch
        // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-dispatch

        // Last chance to catch a malformed keyset before it becomes a
        // token a caller follows.
        verify_returned_keyset(&composed.order, &page)?;

        let next_cursor = page
            .next
            .as_ref()
            .map(|ks| mint_record_cursor(&composed.order, ks, &fingerprint))
            .map(|cursor| cursor.encode())
            .transpose()
            .map_err(|err| {
                UsageCollectorError::internal(format!(
                    "usage-collector could not encode the raw page's continuation: {err}"
                ))
            })?;

        // The plugin reports no limit; resolved here instead
        // ([`DEFAULT_PAGE_SIZE`] for why the default and clamp are
        // load-bearing).
        let limit = composed
            .limit
            .unwrap_or(DEFAULT_PAGE_SIZE)
            .clamp(1, MAX_PAGE_SIZE);

        let mut items = Vec::with_capacity(page.items.len());
        for stored in page.items {
            items.push(reattach(stored, &meter)?);
        }

        // Flows through untouched otherwise — no fold, no marker, no
        // suppression, no reordering.
        // @cpt-algo:cpt-cf-usage-collector-algo-raw-ledger-projection:p1
        // @cpt-dod:cpt-cf-usage-collector-dod-raw-returns-persisted-pair:p1
        // @cpt-dod:cpt-cf-usage-collector-dod-canonical-page-envelope:p1
        Ok(ODataPage::new(
            items,
            PageInfo {
                next_cursor,
                // The raw path reads forward only
                // (`usage-collector-v1.yaml`, the `Cursor` parameter).
                prev_cursor: None,
                limit,
            },
        ))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "list_query_tests.rs"]
mod list_query_tests;
