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
//! bytes here.
//!
//! # Where an operator looks after a `CURSOR_BEYOND_RETENTION`
//!
//! The refusal names no GTS type, deliberately: a refused
//! consumer's remedy is unconditional — restart from [`FeedStart::Oldest`] —
//! whether one subscribed type was swept or five. An operator asking *which*
//! type's retention passed the cursor reads the plugin's
//! `usage_feed_retention_marks` table, which records exactly that per type.
//!
//! [`FeedStart::Oldest`]: usage_collector_sdk::v2::FeedStart::Oldest

use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use toolkit_odata::{CursorV1, SortDir};
use toolkit_security::SecurityContext;
use usage_collector_sdk::v2::error::CursorField;
use uuid::Uuid;

use crate::v2::domain::fingerprint::fnv1a_64;
use usage_collector_sdk::v2::{
    FeedPage, FeedPosition, FeedStart, FeedSubscription, MeterRef, MeterTypeId, SubscribedMeters,
    USAGE_RECORD_RESOURCE, UsageCollectorError, ValidationReason,
};

use crate::v2::domain::authz;
use crate::v2::domain::error;
use crate::v2::domain::observability::{self, OperationLogEntry, log_operation_completed};
use crate::v2::domain::ports::metrics::{PdpOp, PluginOp};
use crate::v2::domain::service::Service;
use crate::v2::domain::type_resolver::TypeResolver;

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
/// (`usage-collector-v2.yaml`, the feed path's `gts_type_id` parameter).
///
/// Enforced here because `FeedSubscription::new` enforces only non-emptiness.
/// Reached from both sides of the service boundary, like the page-size bound:
/// [`build_subscription`] applies it to the repeated wire parameter at the
/// REST edge, and [`require_subscription_breadth`] applies it to the built
/// subscription behind the service, where an in-process caller meets it too.
pub(crate) const MAX_SUBSCRIPTION_TYPES: usize = 100;

/// Page size when a caller names none (`usage-collector-v2.yaml`, `Limit`).
pub(crate) const DEFAULT_FEED_LIMIT: u64 = 100;

/// Largest page a caller may request (`usage-collector-v2.yaml`, `Limit`).
pub(crate) const MAX_FEED_LIMIT: u64 = 1000;

/// Longest cursor token accepted (`usage-collector-v2.yaml`, the feed's
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
/// Factors the shape every feed-path `InvalidArgument` shares across four
/// call sites — mirrors
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
        resource_type: USAGE_RECORD_RESOURCE,
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
/// [`position_from_cursor`] are: [`build_subscription`]
/// is the REST edge's parameter decoder and its only call site is the
/// handler, so a consumer holding an in-process `UsageCollectorClientV2`
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
    // feed's two parameters is meant — `until` is refused on its own
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

/// Resolve every subscribed meter to the reference the Plugin SPI takes.
///
/// **All or nothing.** A subscription is read as one page across all its
/// meters, so dispatching the subset that resolved would serve a page
/// silently missing a subscribed meter's entries — a consumer could not
/// tell that from the meter having had no traffic. The first failure
/// therefore fails the read, which is also what the three parameter-carrying
/// read paths already do for a single meter.
///
/// Resolution is sequential rather than fanned out: the resolver's cache
/// makes a warm subscription free, and a cold one is bounded by the
/// published subscription-breadth cap (`require_subscription_breadth`,
/// checked before this runs).
async fn resolve_subscription_meters(
    subscription: &FeedSubscription,
    type_resolver: &TypeResolver,
) -> Result<SubscribedMeters, UsageCollectorError> {
    let mut meters = Vec::with_capacity(subscription.types().len());
    for gts_type_id in subscription.types() {
        let declaration = type_resolver.resolve(gts_type_id).await?;
        meters.push(MeterRef::new(declaration.type_uuid, gts_type_id.clone()));
    }
    // Both refusals are host-side invariants rather than caller errors:
    // `FeedSubscription` is itself non-empty, and one reference under two
    // identifiers is the types-registry guarantee broken upstream.
    SubscribedMeters::new(meters).map_err(|err| {
        UsageCollectorError::internal(format!(
            "usage-collector could not resolve the subscription to a usable meter set: {err}"
        ))
    })
}

impl Service {
    /// Replay-safe feed page in feed order (DESIGN §3.1).
    ///
    /// Realises `cpt-cf-usage-collector-component-feed-gateway` and the
    /// `cpt-cf-usage-collector-seq-read-feed` sequence.
    ///
    /// Authorization runs **before** the cursor is examined, so a caller
    /// holding no permit receives `PermissionDenied` rather than a diagnosis
    /// of their token: decoding first would tell an unauthorized caller
    /// whether their token was well-formed, bound to this subscription and
    /// still resumable — three facts about a stream they may not read at all.
    ///
    /// No age is computed anywhere on this path: ADR-0011's two zones are
    /// zones of published guarantee, not branches, and the refusal is the
    /// plugin's, keyed on a retention mark.
    ///
    /// The `until` bound is **passed through, never compared** against the
    /// resume cursor. A [`FeedPosition`] is opaque bytes
    /// here, so the gateway has no ordering to compare them under; an `until`
    /// at or behind the cursor reaches the plugin, which answers an empty
    /// page.
    ///
    /// # Errors
    ///
    /// * [`UsageCollectorError::PermissionDenied`] /
    ///   [`UsageCollectorError::ServiceUnavailable`] when the PDP denies, is
    ///   unavailable, or returns a constraint shape this gear cannot project.
    /// * [`UsageCollectorError::CursorRejected`] when `start`'s cursor or
    ///   `until` is not a feed cursor for this subscription, on the field it
    ///   arrived as.
    /// * [`UsageCollectorError::InvalidArgument`] for a `limit` outside the
    ///   published bound, and — carrying
    ///   [`ValidationReason::CursorBeyondRetention`] — for the plugin's
    ///   replay refusal.
    /// * [`UsageCollectorError::NotFound`] when `subscription` names a meter
    ///   with no resolvable declaration. Resolution runs per subscribed
    ///   type and is all-or-nothing (`resolve_subscription_meters`):
    ///   one unresolvable meter fails the whole read rather than serving a
    ///   page silently missing that meter's entries.
    /// * [`UsageCollectorError::ServiceUnavailable`] when a subscribed
    ///   meter's declaration cannot be resolved because `types-registry` is
    ///   unreachable and nothing is cached.
    /// * Any other [`UsageCollectorError`] variant lifted from a plugin
    ///   transport / persistence failure.
    pub async fn read_usage_feed(
        &self,
        ctx: &SecurityContext,
        subscription: &FeedSubscription,
        start: FeedStart<&CursorV1>,
        until: Option<&CursorV1>,
        limit: Option<u64>,
    ) -> Result<FeedPage<CursorV1>, UsageCollectorError> {
        let started = std::time::Instant::now();
        let result = self
            .dispatch_usage_feed(ctx, subscription, start, until, limit)
            .await;

        let seconds = started.elapsed().as_secs_f64();
        if let Ok(page) = &result {
            self.metrics
                .observe_feed_page_entries(u64::try_from(page.entries.len()).unwrap_or(u64::MAX));
        }
        let (outcome, error_category) = observability::classify_feed_result(&result);
        self.metrics
            .record_feed_request(outcome, error_category, seconds);
        // No `resource` / `referenced_type`: a feed page is read over a
        // subscription naming potentially several `MeterTypeId`s
        // (`cpt-cf-usage-collector-fr-...-feed-subscription`), so there is no
        // single value to carry (`inst-log-identifiers-here` carries those
        // only where the operation has exactly one).
        let log_tenant = ctx.subject_tenant_id().to_string();
        log_operation_completed(
            OperationLogEntry::new(PdpOp::ReadFeed, outcome, error_category)
                .tenant(Some(log_tenant.as_str())),
        );
        result
    }

    /// The request's stages, in the order Spec §6 fixes them. Split from the
    /// completion telemetry above so every exit below — `?` included — is
    /// measured and logged exactly once.
    ///
    /// The plugin is resolved **after** authorization, the limit and breadth
    /// admission, subscription-meter resolution and cursor decoding. That
    /// position is fixed, not incidental: Spec §6 orders an unauthorized
    /// caller's rejection ahead of an unavailable plugin's, so the call must
    /// not be hoisted earlier by a future tidy-up.
    // @cpt-flow:cpt-cf-usage-collector-component-feed-gateway:p1
    // @cpt-flow:cpt-cf-usage-collector-seq-read-feed:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-feed-first-connect:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-feed-resume-after-outage:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-feed-bounded-replay:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-feed-observe-correction:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-feed-recover-refused-cursor:p1
    // @cpt-algo:cpt-cf-usage-collector-algo-feed-request-admission:p1
    // @cpt-algo:cpt-cf-usage-collector-algo-feed-page-assembly:p1
    // @cpt-algo:cpt-cf-usage-collector-algo-feed-retention-refusal:p1
    // @cpt-state:cpt-cf-usage-collector-state-feed-cursor-servability:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-feed-next-cursor-always:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-feed-corrections-as-entries:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-feed-retention-refusal:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-feed-at-least-once:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-feed-unstripped-entries:p1
    async fn dispatch_usage_feed(
        &self,
        ctx: &SecurityContext,
        subscription: &FeedSubscription,
        start: FeedStart<&CursorV1>,
        until: Option<&CursorV1>,
        limit: Option<u64>,
    ) -> Result<FeedPage<CursorV1>, UsageCollectorError> {
        // @cpt-begin:cpt-cf-usage-collector-seq-read-feed:p1:inst-feed-authorize
        let scope = authz::authorize_read_usage_feed(
            &self.enforcer,
            self.metrics.as_ref(),
            PdpOp::ReadFeed,
            ctx,
        )
        .await
        .map_err(UsageCollectorError::from)?;
        // @cpt-end:cpt-cf-usage-collector-seq-read-feed:p1:inst-feed-authorize

        // No [`QueryInflightGuard`] here, and that is a decision rather
        // than an omission: `uc_query_inflight` is labelled by
        // [`QueryKind`], whose published value set is `aggregated` / `raw`
        // — entering the feed under either would mis-attribute it, and
        // DESIGN §3.11.5 declares no feed gauge to widen it for. This path
        // feeds the feed's own instruments instead.
        let limit = resolve_limit(limit)?;
        // Beside `resolve_limit` and for the same reason: the REST edge
        // is not the only caller. See the helper for why this is a
        // second check rather than a moved one.
        require_subscription_breadth(subscription)?;

        // Resolved after the breadth cap and before the plugin is
        // resolved: the cap bounds how many resolves one request can
        // provoke, and a subscription naming an undeclared meter fails
        // closed here rather than reaching the plugin at all.
        let meters = resolve_subscription_meters(subscription, &self.type_resolver).await?;

        let spi_start = match start {
            FeedStart::Oldest => FeedStart::Oldest,
            FeedStart::After(cursor) => FeedStart::After(position_from_cursor(
                cursor,
                subscription,
                CursorField::Cursor,
            )?),
            // `FeedStart` is `#[non_exhaustive]` and foreign, so this arm
            // is required — and required to be a refusal rather than a
            // fallthrough to `Oldest`: a start mode admitted later and
            // silently reinterpreted as "from the oldest retained entry"
            // would re-serve a consumer's whole retained history as if it
            // were new, which on this path is a re-billed invoice.
            ref other => {
                return Err(UsageCollectorError::internal(format!(
                    "unrecognised feed start mode: {other:?}"
                )));
            }
        };

        // `until` is decoded on its own field name, so a consumer who
        // pasted a stale bound is told which parameter is at fault
        // — but it is never compared with `spi_start`.
        let spi_until = until
            .map(|cursor| position_from_cursor(cursor, subscription, CursorField::Until))
            .transpose()?;

        // @cpt-begin:cpt-cf-usage-collector-seq-read-feed:p1:inst-feed-dispatch
        // Resolved here and not earlier: Spec §6 fixes this as the first point
        // an unavailable plugin may be reported, after authorization and every
        // admission check above.
        let plugin = self.resolve_plugin_for(PluginOp::ReadFeedPage).await?;

        let page = observability::instrument_spi(
            self.metrics.as_ref(),
            PluginOp::ReadFeedPage,
            plugin.read_feed_page(&meters, &scope, spi_start, spi_until, limit),
        )
        .await
        .map_err(|e| {
            observability::lift_feed_dispatch_error(
                e,
                &start,
                self.unavailable_retry_after_secs,
                self.target_not_converged_retry_after_secs,
            )
        })?;
        // @cpt-end:cpt-cf-usage-collector-seq-read-feed:p1:inst-feed-dispatch

        // A page longer than the plugin was told to serve is refused, not
        // served and not truncated. The defect is in the page itself
        // rather than in the token that continues it, so unlike an absent
        // continuation it cannot be diagnosed and served: the alternative
        // is handing a charging consumer a body its own schema validator
        // rejects with nothing naming the component at fault. Truncating
        // would drop entries the `next` position already sits past — a
        // gap in an invoice rather than a duplicate.
        //
        // `internal`, not the aggregate path's `InvalidArgument`: there
        // the cap is reachable by a caller widening `group_by`, while here
        // `limit` is the caller's own number and the plugin exceeded it.
        // A host-contract breach, counted under `plugin_error`.
        if page.entries.len() > usize::try_from(limit).unwrap_or(usize::MAX) {
            return Err(UsageCollectorError::internal(format!(
                "usage-collector storage plugin served {} feed entries for a page limit of \
             {limit}; a page longer than the limit it was dispatched under cannot be \
             handed to a consumer",
                page.entries.len(),
            )));
        }

        // Diagnosed, not decided — see the helper for why the feed's
        // half of this has no refusal available to it.
        observability::report_absent_live_continuation(&page, until.is_some());

        // @cpt-begin:cpt-cf-usage-collector-seq-read-feed:p1:inst-feed-map-result
        // Mapped, never decided. DESIGN §3.2 fixes both dispositions on
        // the PLUGIN — `Some` on every page of a live read, short pages
        // included, `None` once a bounded replay has reached its `until`
        // — and a gateway-side branch would be a second, disagreeing
        // authority whose first disagreement hands a live consumer a
        // `None` that reads as "this stream has ended". Hence the breach
        // is reported above rather than left to the `map` below.
        //
        // The identifier is re-attached per entry, from the subscription
        // this read resolved; an entry under a reference the subscription
        // never named has no identifier to re-attach at all — a typed
        // breach, not an index into a missing key.
        //
        // `SubscribedMeters::new` refuses a reference arriving under two
        // identifiers, so the references here are unique and this map cannot
        // lose one to a collision.
        let by_reference: HashMap<Uuid, &MeterTypeId> =
            meters.refs().iter().map(|m| (m.uuid, &m.id)).collect();
        let entries = page
            .entries
            .into_iter()
            .map(|stored| {
                let gts_type_id = by_reference.get(&stored.gts_type_uuid).ok_or_else(|| {
                    error::invariant_breach(format!(
                        "storage plugin answered a feed page with an entry under registry \
                     reference {}, which this subscription does not name",
                        stored.gts_type_uuid,
                    ))
                })?;
                Ok(stored.into_usage_record((*gts_type_id).clone()))
            })
            .collect::<Result<Vec<_>, UsageCollectorError>>()?;

        Ok(FeedPage {
            entries,
            next: page.next.map(|p| mint_cursor(&p, subscription)),
        })
        // @cpt-end:cpt-cf-usage-collector-seq-read-feed:p1:inst-feed-map-result
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "feed_tests.rs"]
pub(crate) mod feed_tests;
