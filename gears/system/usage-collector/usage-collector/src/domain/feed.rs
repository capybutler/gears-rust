//! The Feed Gateway's pure logic: subscription binding, and the wire cursor
//! a plugin's opaque [`FeedPosition`] travels inside.
//!
//! DESIGN §3.2 makes the Feed Gateway a component distinct from the Query
//! Gateway, and `cpt-cf-usage-collector-principle-cursor-gateway-ownership`
//! puts `CursorV1` wholly on this side: "The SPI never sees the wire token."
//!
//! # Age lives nowhere in here
//!
//! ADR-0011's "two zones and one refusal" describes what a consumer may rely
//! on, not a branch. A cursor inside the operational replay horizon is served
//! and that is the published guarantee; one older than it is *also* served
//! whenever the plugin still holds the continuation. Both are the same served
//! path. The one refusal is orthogonal to both and is keyed on a retention
//! mark the plugin raises — DESIGN §3.1: a position's age "is a progress
//! measure, not the retention refusal's input".
//!
//! This module could not compute an age even if a caller asked: age is
//! defined over ledger contents, and the position inside the cursor is opaque
//! bytes here. See `feed_tests.rs`'s age-absence pin.
//!
//! # Where an operator looks after a `CURSOR_BEYOND_RETENTION`
//!
//! The refusal names no GTS type, deliberately: a refused
//! consumer's remedy is unconditional — restart from [`FeedStart::Oldest`] —
//! whether one subscribed type was swept or five. An operator asking *which*
//! type's retention passed the cursor reads the plugin's
//! `usage_feed_retention_marks` table, which records exactly that per type.
//!
//! [`FeedStart::Oldest`]: usage_collector_sdk::FeedStart::Oldest

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use toolkit_odata::{CursorV1, SortDir};
use usage_collector_sdk::error::CursorField;

use crate::domain::fingerprint::fnv1a_64;
use usage_collector_sdk::{
    FeedPosition, FeedSubscription, MeterTypeId, USAGE_RECORD_RESOURCE, UsageCollectorError,
    ValidationReason,
};

/// The `CursorV1::s` value every feed cursor carries.
///
/// `CursorV1::decode` refuses a blank `s`, so a feed cursor must put
/// *something* there — the raw path's signed order tokens have no feed
/// analogue, because the feed admits no caller order (DESIGN §3.1, Order
/// admissibility). A fixed sentinel is therefore forced rather than chosen,
/// and it earns a second keep: a feed cursor and a raw-query cursor are the
/// same type in the same alphabet on the same gear, and this is what tells
/// them apart.
pub(crate) const FEED_CURSOR_SENTINEL: &str = "feed.v1";

/// The published ceiling on a subscription's breadth
/// (`usage-collector-v1.yaml`, the feed path's `gts_type_id` parameter).
///
/// Enforced here because `FeedSubscription::new` enforces only non-emptiness.
/// Reached from both sides of the service boundary, like the page-size bound:
/// [`build_subscription`] applies it to the repeated wire parameter at the
/// REST edge, and [`require_subscription_breadth`] applies it to the built
/// subscription behind the service, where an in-process caller meets it too.
pub(crate) const MAX_SUBSCRIPTION_TYPES: usize = 100;

/// Page size when a caller names none (`usage-collector-v1.yaml`, `Limit`).
pub(crate) const DEFAULT_FEED_LIMIT: u64 = 100;

/// Largest page a caller may request (`usage-collector-v1.yaml`, `Limit`).
pub(crate) const MAX_FEED_LIMIT: u64 = 1000;

/// Longest cursor token accepted (`usage-collector-v1.yaml`, the feed's
/// `Cursor` and `Until`). Checked before base64 decoding, so an oversized
/// token costs a length comparison rather than an allocation.
///
/// **Feed-only, and that is an asymmetry worth stating here.** The raw
/// path's `Cursor` parameter publishes the same `maxLength: 4096`, and
/// nothing enforces it: [`decode_cursor_token`] is this constant's only
/// reader, and the raw path decodes through
/// `query::admit_continuation` instead, which checks structure, direction
/// and fingerprint but not length. The document and the code nevertheless
/// agree on the *promise* — the feed's `Cursor` description says a
/// "malformed or over-length token is rejected `400`", and the raw
/// `Cursor`'s says only "malformed" — so `maxLength` there is a bound on
/// what the gear *mints*, not a rejection it offers. That is what
/// `service_tests`' widest-admissible-request test asserts, which is why
/// a raw-path test reaches across into this feed constant: it is the only
/// spelling of the published number in this crate. Enforcing it on the
/// raw path too would be a new refusal on a surface whose description
/// does not promise one, so it is left as a recorded asymmetry rather
/// than closed silently.
pub(crate) const MAX_CURSOR_TOKEN_CHARS: usize = 4096;

/// Build an `InvalidArgument` naming `field`, with a fixed `Validation`
/// reason and this crate's own resource type.
///
/// Factors the shape every feed-path `InvalidArgument` shares (four call
/// sites, previously each writing out all five struct fields) — mirrors
/// [`reject`]'s role for the `CursorRejected` half of this module. No public
/// SDK constructor fits a caller-chosen `field`/`detail` pair:
/// `UsageCollectorError::invalid_argument_on` does not exist, and the
/// closest shape, `newtype_validation`, is a private `fn` in the SDK crate
/// (`UsageCollectorError::newtype_validation` in
/// `usage-collector-sdk/src/error.rs`) — not reachable from here. So
/// this constructs the variant directly, which is the documented fallback
/// for exactly this situation.
fn invalid_argument(field: &str, detail: impl Into<String>) -> UsageCollectorError {
    UsageCollectorError::InvalidArgument {
        resource_type: USAGE_RECORD_RESOURCE.to_owned(),
        resource_name: None,
        field: field.to_owned(),
        reason: ValidationReason::Validation,
        detail: detail.into(),
    }
}

/// Build a subscription from the repeated wire parameter.
///
/// # Errors
///
/// [`UsageCollectorError::InvalidArgument`] when `raw` names more than
/// [`MAX_SUBSCRIPTION_TYPES`] types, when any one of them is not a valid
/// [`MeterTypeId`], or when `raw` is empty ([`FeedSubscription::new`]'s own
/// guard, reported on the same field).
// @cpt-dod:cpt-cf-usage-collector-dod-feed-subscription-binding:p1
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub(crate) fn build_subscription(raw: &[String]) -> Result<FeedSubscription, UsageCollectorError> {
    if raw.len() > MAX_SUBSCRIPTION_TYPES {
        return Err(invalid_argument(
            "gts_type_id",
            format!(
                "a subscription names at most {MAX_SUBSCRIPTION_TYPES} GTS types; \
                 read more than that over several subscriptions"
            ),
        ));
    }
    let mut types = Vec::with_capacity(raw.len());
    for id in raw {
        // `MeterTypeId::new` already returns `UsageCollectorError` shaped
        // exactly as a `gts_type_id` violation should be (field, and a typed
        // reason more specific than `Validation`), so it propagates as-is
        // rather than being re-wrapped into a second, coarser envelope.
        types.push(MeterTypeId::new(id.as_str())?);
    }
    FeedSubscription::new(types).map_err(|e| invalid_argument("gts_type_id", e.to_string()))
}

/// Apply the published subscription ceiling to an already-built
/// subscription.
///
/// Sited behind the service for the reason [`resolve_limit`] and
/// [`position_from_cursor`] are (rulings E8 and E11): [`build_subscription`]
/// is the REST edge's parameter decoder and its only call site is the
/// handler, so a consumer holding an in-process `UsageCollectorClientV1`
/// builds a [`FeedSubscription`] directly and never meets it.
/// `FeedSubscription::new` enforces only non-emptiness, the Plugin SPI names
/// only an out-of-bound `limit` among the host-contract breaches it answers
/// with `Internal`, and the `TimescaleDB` plugin shapes its feed page
/// statement as one lateral join *per subscribed type* — so an unchecked
/// 5000-type in-process subscription is a 5000-branch SQL statement, not a
/// slightly wider read.
///
/// Not folded into [`build_subscription`]'s own check, which stays on the
/// **raw** parameter list. That one is the published `maxItems: 100` on a
/// query-array parameter, and it is the array the caller sent that the
/// contract bounds; deduplication only shrinks the set, so moving the check
/// after it would silently admit a 150-value array that collapsed to 50.
/// The two therefore measure different things, and both are wanted.
///
/// # Errors
///
/// [`UsageCollectorError::InvalidArgument`] when `sub` names more than
/// [`MAX_SUBSCRIPTION_TYPES`] types, on the same field
/// [`build_subscription`] reports.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub(crate) fn require_subscription_breadth(
    sub: &FeedSubscription,
) -> Result<(), UsageCollectorError> {
    if sub.types().len() > MAX_SUBSCRIPTION_TYPES {
        return Err(invalid_argument(
            "gts_type_id",
            format!(
                "a subscription names at most {MAX_SUBSCRIPTION_TYPES} GTS types; \
                 read more than that over several subscriptions"
            ),
        ));
    }
    Ok(())
}

/// The value bound into `CursorV1::f`.
///
/// DESIGN §3.3: "A feed cursor binds the subscription as a raw cursor binds
/// its filter." [`FeedSubscription`] is already sorted and deduplicated, so
/// this is stable under whatever order a caller listed the types in.
///
/// Built from [`fnv1a_64`] rather than [`toolkit_odata::short_filter_hash`]:
/// the latter fingerprints a parsed `$filter` AST, which a subscription's
/// type list is not. Both run the same algorithm underneath, and the
/// `{:016x}` rendering matches `short_filter_hash`'s, so the two kinds of
/// fingerprint look alike on the wire.
pub(crate) fn subscription_fingerprint(sub: &FeedSubscription) -> String {
    let joined = sub
        .types()
        .iter()
        .map(MeterTypeId::as_str)
        .collect::<Vec<_>>()
        .join("\u{1f}");
    format!("{:016x}", fnv1a_64(joined.as_bytes()))
}

/// Carry a plugin-issued position out to a consumer.
///
/// Total: a position the plugin issued cannot fail to encode.
// @cpt-dod:cpt-cf-usage-collector-dod-feed-cursor-ownership:p1
// @cpt-flow:cpt-cf-usage-collector-principle-cursor-gateway-ownership:p1
pub(crate) fn mint_cursor(position: &FeedPosition, sub: &FeedSubscription) -> CursorV1 {
    CursorV1 {
        k: vec![URL_SAFE_NO_PAD.encode(position.as_bytes())],
        // The feed admits no caller order. `Asc` is the inert value; nothing
        // reads it, and `CursorV1::decode` requires a `SortDir` regardless.
        o: SortDir::Asc,
        s: FEED_CURSOR_SENTINEL.to_owned(),
        f: Some(subscription_fingerprint(sub)),
        // DESIGN §3.2: "The feed reads forward only."
        d: "fwd".to_owned(),
    }
}

/// Reject helper shared by both halves of the decode path.
///
/// `field` names which parameter a violation is reported on, so `until` is
/// refused on the same terms as `cursor` and on its own name.
///
/// Both arms name a **feed** constructor. The shared
/// `inadmissible_cursor_keyset` is the raw read path's and stays there: its
/// detail reports "the cursor's bound order is not a usable keyset", which
/// on this path would tell a consumer whose token was merely too long to go
/// looking for an order the feed does not admit.
fn reject(field: CursorField, defect: String) -> UsageCollectorError {
    match field {
        CursorField::Cursor => UsageCollectorError::inadmissible_feed_cursor(defect),
        CursorField::Until => UsageCollectorError::inadmissible_until_keyset(defect),
    }
}

/// Parse a wire token into a cursor. **Syntactic only.**
///
/// The length bound and `CursorV1::decode`, and nothing else — this half runs
/// at the REST edge, where no subscription-bound judgement belongs. The
/// semantic guards are [`position_from_cursor`]'s, behind the service, because
/// an in-process caller supplies a decoded `CursorV1` and never reaches this
/// function at all.
///
/// # Errors
///
/// [`UsageCollectorError::CursorRejected`] when the token exceeds
/// [`MAX_CURSOR_TOKEN_CHARS`] or fails `CursorV1::decode`.
// @cpt-dod:cpt-cf-usage-collector-dod-feed-cursor-ownership:p1
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub(crate) fn decode_cursor_token(
    token: &str,
    field: CursorField,
) -> Result<CursorV1, UsageCollectorError> {
    if token.chars().count() > MAX_CURSOR_TOKEN_CHARS {
        return Err(reject(
            field,
            format!("the token exceeds the published {MAX_CURSOR_TOKEN_CHARS}-character bound"),
        ));
    }
    CursorV1::decode(token).map_err(|e| reject(field, e.to_string()))
}

/// Recover a plugin position from a decoded cursor. **Semantic.**
///
/// Runs behind the service so that every caller meets it — REST, the
/// in-process client, and a direct `Service` call alike. Siting the
/// subscription check at the edge instead would let an in-process consumer
/// resume a cursor minted over a different subscription and be served a
/// silently wrong page, which on this path is a wrong invoice. The raw read
/// path makes the identical argument for its own fingerprint check.
///
/// # Errors
///
/// [`UsageCollectorError::CursorRejected`] when the cursor's `s` is not
/// [`FEED_CURSOR_SENTINEL`], when its `d` is not `"fwd"`, when it names a
/// different subscription, or when its bound position is not a usable
/// [`FeedPosition`].
// @cpt-algo:cpt-cf-usage-collector-algo-feed-cursor-binding:p1
// @cpt-dod:cpt-cf-usage-collector-dod-feed-subscription-binding:p1
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub(crate) fn position_from_cursor(
    cursor: &CursorV1,
    sub: &FeedSubscription,
    field: CursorField,
) -> Result<FeedPosition, UsageCollectorError> {
    if cursor.s != FEED_CURSOR_SENTINEL {
        return Err(reject(
            field,
            "the token is not a feed cursor; a cursor from another read path cannot \
             continue the feed"
                .to_owned(),
        ));
    }
    // DESIGN §3.2: "The feed reads forward only." [`mint_cursor`] has never
    // minted anything but `"fwd"`, and `CursorV1::decode` validates only that
    // `d` is one of `{"fwd", "bwd"}` — not that it is the one this read path
    // serves. So this is the one place the feed's published forward-only
    // promise is a check rather than a convention, and a `"bwd"` token
    // reaching here was forged or hand-edited rather than issued.
    //
    // Sited here, not in [`decode_cursor_token`], for the reason the whole
    // semantic half is: the edge decoder is the REST path's, and
    // an in-process caller hands the service an already-decoded `CursorV1`.
    // Both of `read_usage_feed`'s cursor-bearing parameters pass through this
    // function, so one check covers `cursor` and `until` alike.
    //
    // Refused through [`reject`] rather than through the raw path's
    // `UsageCollectorError::unsupported_cursor_direction`, which hard-codes
    // `CursorField::Cursor` because upstream has no notion of which of the
    // feed's two parameters is meant — ruling E7 refuses `until` on its own
    // name. The wire code is `InvalidCursor` either way.
    if cursor.d != "fwd" {
        tracing::warn!(
            direction = %cursor.d,
            field = ?field,
            "usage-collector refused a feed cursor whose direction is not forward"
        );
        return Err(reject(
            field,
            format!(
                "it specifies direction `{}`; the feed reads forward only",
                cursor.d
            ),
        ));
    }
    // Both arms name a **feed** constructor, for the reason [`reject`]
    // gives: the shared `cursor_query_mismatch` enumerates `$filter`, the
    // `from` / `to` range and `metadata.<key>` as the inputs a cursor binds,
    // and the feed admits none of the three.
    if cursor.f.as_deref() != Some(subscription_fingerprint(sub).as_str()) {
        return Err(match field {
            CursorField::Cursor => UsageCollectorError::cursor_subscription_mismatch(),
            CursorField::Until => UsageCollectorError::until_query_mismatch(),
        });
    }
    let [encoded] = cursor.k.as_slice() else {
        return Err(reject(
            field,
            "a feed cursor names exactly one position".to_owned(),
        ));
    };
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| reject(field, "the position is not base64url".to_owned()))?;
    FeedPosition::new(bytes).map_err(|e| reject(field, e.to_string()))
}

/// Parse and bound-check a raw `limit` query-string value.
///
/// One producer for the limit error, reached from the REST edge; the service
/// and SDK path reach the same bounds through [`resolve_limit`].
///
/// # Errors
///
/// [`UsageCollectorError::InvalidArgument`] when `raw` does not parse as a
/// `u64`, or fails the bound [`resolve_limit`] applies.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub(crate) fn parse_limit(raw: &str) -> Result<u64, UsageCollectorError> {
    let parsed = raw
        .parse::<u64>()
        .map_err(|_| invalid_argument("limit", "`limit` must be a whole number"))?;
    resolve_limit(Some(parsed))
}

/// Apply the published page-size bounds.
///
/// Refused rather than clamped: the plugin documents a `limit` outside its own
/// bound as a host-contract breach answered with `Internal`, so silently
/// raising a `0` or lowering a `1001` would hide a caller error behind a
/// number the caller never asked for.
///
/// # Errors
///
/// [`UsageCollectorError::InvalidArgument`] when `requested` is `Some(0)` or
/// exceeds [`MAX_FEED_LIMIT`].
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub(crate) fn resolve_limit(requested: Option<u64>) -> Result<u64, UsageCollectorError> {
    let limit = requested.unwrap_or(DEFAULT_FEED_LIMIT);
    if limit == 0 || limit > MAX_FEED_LIMIT {
        return Err(invalid_argument(
            "limit",
            format!("a page holds between 1 and {MAX_FEED_LIMIT} entries"),
        ));
    }
    Ok(limit)
}

/// Compare two cursors field by field.
///
/// `toolkit_odata::CursorV1` implements neither `PartialEq` nor `Eq` and this
/// slice does not add them, so this is the single place two
/// cursors are compared — no test does it ad hoc.
///
/// The destructure deliberately carries **no** `..`: a sixth field on
/// `CursorV1` becomes a compile error here rather than a field this helper
/// silently stops checking. `feed_tests.rs` pins that it sees all five.
#[cfg(test)]
pub(crate) fn cursors_equal(a: &CursorV1, b: &CursorV1) -> bool {
    let CursorV1 {
        k: ak,
        o: ao,
        s: as_,
        f: af,
        d: ad,
    } = a;
    let CursorV1 {
        k: bk,
        o: bo,
        s: bs,
        f: bf,
        d: bd,
    } = b;
    ak == bk && ao == bo && as_ == bs && af == bf && ad == bd
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "feed_tests.rs"]
mod feed_tests;
