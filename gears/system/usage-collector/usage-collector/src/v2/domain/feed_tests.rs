//! Unit tests for the Feed Gateway's pure core: subscription binding, the wire
//! cursor a plugin's opaque [`FeedPosition`] travels inside, and the published
//! page-size / token-length bounds.
//!
//! Fixture meter type ids below are five-token derivations
//! (`vendor.package.namespace.type.vMAJOR`, `~`-terminated) — the shape
//! `MeterTypeId::new` (via the upstream `gts` crate's `GtsId::try_new`)
//! requires.

use toolkit_odata::{CursorV1, SortDir};
use usage_collector_sdk::v2::{FeedPosition, FeedSubscription, MeterTypeId, UsageCollectorError};

use super::*;

fn sub(ids: &[&str]) -> FeedSubscription {
    FeedSubscription::new(
        ids.iter()
            .map(|s| MeterTypeId::new(*s).expect("valid meter id")),
    )
    .expect("non-empty subscription")
}

fn pos(bytes: &[u8]) -> FeedPosition {
    FeedPosition::new(bytes.to_vec()).expect("valid position")
}

/// Compose the two halves of the decode path the way a REST
/// request does: `decode_cursor_token` at the edge, `position_from_cursor`
/// behind the service. The tests below exercise the pair, because a caller
/// never meets one without the other.
fn decode_cursor(
    token: &str,
    s: &FeedSubscription,
    field: CursorField,
) -> Result<FeedPosition, UsageCollectorError> {
    let cursor = decode_cursor_token(token, field)?;
    position_from_cursor(&cursor, s, field)
}

/// Extract the `detail` string from a `CursorRejected`, panicking on any other
/// variant.
///
/// Several assertions below need to pin the *specific* defect text a guard
/// reports, not merely that some `CursorRejected` arrived: `CursorRejected` is
/// produced by several distinct guards on this path (a length bound, a
/// malformed-base64 decode failure, a sentinel mismatch, a fingerprint
/// mismatch), so a bare `matches!(err, CursorRejected { .. })` can pass even
/// when the specific guard a test names has been deleted.
fn cursor_rejected_detail(err: &UsageCollectorError) -> &str {
    match err {
        UsageCollectorError::CursorRejected { detail, .. } => detail,
        other => panic!("expected CursorRejected, got {other:?}"),
    }
}

/// Fails if minting and decoding disagree on the position's bytes.
#[test]
fn a_minted_cursor_decodes_back_to_the_same_position() {
    let s = sub(&["gts.cf.core.uc.usage_record.v2~vendor.a._.meter.v1~"]);
    let p = pos(&[0xDE, 0xAD, 0xBE, 0xEF]);
    let token = mint_cursor(&p, &s).encode().expect("cursor encodes");
    let back = decode_cursor(&token, &s, CursorField::Cursor).expect("decodes");
    assert_eq!(back.as_bytes(), p.as_bytes());
}

/// Fails if a cursor minted over a wider subscription is served against a
/// narrower one — which would silently deliver a feed missing a type the
/// consumer is still resuming, with no error anywhere.
#[test]
fn a_cursor_minted_over_a_wider_subscription_is_refused() {
    let wide = sub(&[
        "gts.cf.core.uc.usage_record.v2~vendor.a._.meter.v1~",
        "gts.cf.core.uc.usage_record.v2~vendor.b._.meter.v1~",
    ]);
    let narrow = sub(&["gts.cf.core.uc.usage_record.v2~vendor.a._.meter.v1~"]);
    let token = mint_cursor(&pos(&[1]), &wide).encode().expect("encodes");

    let err = decode_cursor(&token, &narrow, CursorField::Cursor)
        .expect_err("a subscription change must refuse the cursor");
    assert!(
        matches!(err, UsageCollectorError::CursorRejected { .. }),
        "got {err:?}"
    );
}

/// Fails if listing the same types in a different order, or twice, refuses a
/// valid cursor. `FeedSubscription::new` sorts and dedups, so these are the
/// SAME subscription and must share a fingerprint.
#[test]
fn subscription_order_and_repeats_do_not_change_the_fingerprint() {
    let a = sub(&[
        "gts.cf.core.uc.usage_record.v2~vendor.b._.meter.v1~",
        "gts.cf.core.uc.usage_record.v2~vendor.a._.meter.v1~",
    ]);
    let b = sub(&[
        "gts.cf.core.uc.usage_record.v2~vendor.a._.meter.v1~",
        "gts.cf.core.uc.usage_record.v2~vendor.b._.meter.v1~",
        "gts.cf.core.uc.usage_record.v2~vendor.a._.meter.v1~",
    ]);
    assert_eq!(subscription_fingerprint(&a), subscription_fingerprint(&b));
}

/// Fails if a raw-query cursor is accepted by the feed. Both surfaces mint
/// `CursorV1` in the same base64url alphabet, so without the sentinel the only
/// thing separating them is a fingerprint comparison that would report a filter
/// mismatch — the wrong diagnosis for the wrong-surface case.
///
/// Pinned on the sentinel-mismatch defect text, not merely on `CursorRejected`:
/// this fixture's `f` is `Some("deadbeef")`, which also fails
/// `position_from_cursor`'s fingerprint check on its own, so dropping the
/// sentinel guard still produces a `CursorRejected` — via
/// `cursor_subscription_mismatch` — and a variant-only assertion would pass
/// under exactly the bug it names.
#[test]
fn a_raw_path_cursor_is_refused_by_the_feed() {
    let raw = CursorV1 {
        k: vec!["2026-01-01T00:00:00Z".to_owned()],
        o: SortDir::Asc,
        s: "+window_end,+id".to_owned(),
        f: Some("deadbeef".to_owned()),
        d: "fwd".to_owned(),
    };
    let token = raw.encode().expect("encodes");
    let err = decode_cursor(
        &token,
        &sub(&["gts.cf.core.uc.usage_record.v2~vendor.a._.meter.v1~"]),
        CursorField::Cursor,
    )
    .expect_err("a raw-path cursor is not a feed cursor");
    let detail = cursor_rejected_detail(&err);
    assert!(
        detail.contains("not a feed cursor"),
        "expected the sentinel-mismatch defect in the detail, got: {detail}"
    );
}

/// Fails if a feed cursor naming `d: "bwd"` is accepted and walked forward.
///
/// The feed is forward-only by contract — the yaml's feed `Cursor` says
/// "Forward-only; callers MUST NOT parse or modify", and [`mint_cursor`] mints
/// nothing but `"fwd"`. Nothing *enforced* it: `position_from_cursor` read `s`,
/// `f` and `k` and never `cursor.d`, and `CursorV1::decode` validates only that
/// `d` is one of `{"fwd", "bwd"}`, not that it is the one this read path serves.
/// A token built exactly like this one therefore decoded clean and was paged
/// forward with `200`, on a path that bills. The raw path's counterpart is
/// `query::require_forward_cursor`.
///
/// The fixture is a **minted** cursor with one field flipped, so every other
/// guard on this path passes by construction and direction is the only thing
/// left to refuse it on. Pinned on the direction defect text rather than merely
/// on `CursorRejected`, for the reason [`cursor_rejected_detail`] gives.
#[test]
fn a_backward_cursor_is_refused() {
    let s = sub(&["gts.cf.core.uc.usage_record.v2~vendor.a._.meter.v1~"]);
    let mut backward = mint_cursor(&pos(&[1]), &s);
    backward.d = "bwd".to_owned();
    let token = backward.encode().expect("encodes");

    let err =
        decode_cursor(&token, &s, CursorField::Cursor).expect_err("the feed reads forward only");
    let detail = cursor_rejected_detail(&err);
    assert!(
        detail.contains("bwd"),
        "expected the direction defect to name the refused direction, got: {detail}"
    );
}

/// Fails if a backward `until` bound is refused on the `cursor` field, or is not
/// refused at all.
///
/// Not implied by [`a_backward_cursor_is_refused`]: `position_from_cursor`'s
/// `field` argument decides nothing except which error is produced, so a
/// direction guard can easily be sited where only the `cursor` arm reaches it.
/// Reusing the raw path's `UsageCollectorError::unsupported_cursor_direction`
/// here would do exactly that — it hard-codes `CursorField::Cursor` — and
/// `until` must be refused on its own name.
#[test]
fn a_backward_until_bound_is_refused_on_its_own_field() {
    let s = sub(&["gts.cf.core.uc.usage_record.v2~vendor.a._.meter.v1~"]);
    let mut backward = mint_cursor(&pos(&[1]), &s);
    backward.d = "bwd".to_owned();
    let token = backward.encode().expect("encodes");

    let err = decode_cursor(&token, &s, CursorField::Until)
        .expect_err("the feed reads forward only on `until` too");
    match err {
        UsageCollectorError::CursorRejected { field, .. } => {
            assert_eq!(field, CursorField::Until);
        }
        other => panic!("expected CursorRejected on `until`, got {other:?}"),
    }
}

/// Fails if an over-wide subscription reaches the plugin. `FeedSubscription::new`
/// rejects only the EMPTY case — it enforces no ceiling — so the yaml's
/// `maxItems: 100` has to be enforced here.
#[test]
fn a_subscription_over_the_published_ceiling_is_refused() {
    let raw: Vec<String> = (0..=MAX_SUBSCRIPTION_TYPES)
        .map(|i| format!("gts.cf.core.uc.usage_record.v2~vendor.t{i}._.meter.v1~"))
        .collect();
    let err = build_subscription(&raw).expect_err("101 types exceeds the published ceiling");
    assert!(
        matches!(err, UsageCollectorError::InvalidArgument { .. }),
        "got {err:?}"
    );
}

/// Fails if naming no type reaches the plugin as an unbounded read.
#[test]
fn an_empty_subscription_is_refused() {
    let err = build_subscription(&[]).expect_err("a subscription reading nothing has no page");
    assert!(
        matches!(err, UsageCollectorError::InvalidArgument { .. }),
        "got {err:?}"
    );
}

/// Fails if a well-formed cursor carrying an unusable position panics or reaches
/// the plugin. `FeedPosition::new` rejects zero bytes; that must become a 400 on
/// the cursor.
#[test]
fn a_cursor_carrying_a_zero_byte_position_is_refused() {
    let s = sub(&["gts.cf.core.uc.usage_record.v2~vendor.a._.meter.v1~"]);
    let empty = CursorV1 {
        k: vec![String::new()],
        o: SortDir::Asc,
        s: FEED_CURSOR_SENTINEL.to_owned(),
        f: Some(subscription_fingerprint(&s)),
        d: "fwd".to_owned(),
    };
    let token = empty.encode().expect("encodes");
    let err = decode_cursor(&token, &s, CursorField::Cursor)
        .expect_err("a position of zero bytes is not a position");
    assert!(
        matches!(err, UsageCollectorError::CursorRejected { .. }),
        "got {err:?}"
    );
}

/// Fails if an over-length token is base64-decoded before its length is checked.
/// The yaml caps both cursor parameters at 4096 characters.
///
/// Pinned on the length-bound defect text, not merely on `CursorRejected`: a
/// `MAX_CURSOR_TOKEN_CHARS + 1`-character token of `A`s is itself invalid base64
/// — `4097 mod 4 == 1`, and no unpadded base64 string has a length congruent to
/// 1 mod 4 — so dropping the length gate still leaves `CursorV1::decode` failing
/// on malformed base64, which is *also* a `CursorRejected`. Lengthening the
/// fixture to a valid base64 shape does not help either: a structurally valid
/// oversized token fails later at `FeedPosition::new` with `TooLarge`, which is
/// also a `CursorRejected`. Only pinning the detail text discriminates
/// regardless of the fixture's length mod 4.
#[test]
fn an_over_length_token_is_refused_before_decoding() {
    let s = sub(&["gts.cf.core.uc.usage_record.v2~vendor.a._.meter.v1~"]);
    let huge = "A".repeat(MAX_CURSOR_TOKEN_CHARS + 1);
    let err = decode_cursor(&huge, &s, CursorField::Cursor)
        .expect_err("a token past the published cap is refused");
    let detail = cursor_rejected_detail(&err);
    assert!(
        detail.contains(&format!("{MAX_CURSOR_TOKEN_CHARS}-character bound")),
        "expected the length-bound defect in the detail, got: {detail}"
    );
}

/// Fails if the limit is clamped rather than refused, which would make the
/// plugin's own out-of-range Internal unreachable in a way no test notices
/// if the two bounds ever diverge.
#[test]
fn a_limit_outside_the_published_range_is_refused_not_clamped() {
    assert_eq!(resolve_limit(None).expect("default"), DEFAULT_FEED_LIMIT);
    assert_eq!(resolve_limit(Some(1)).expect("floor"), 1);
    assert_eq!(
        resolve_limit(Some(MAX_FEED_LIMIT)).expect("ceiling"),
        MAX_FEED_LIMIT
    );
    assert!(
        resolve_limit(Some(0)).is_err(),
        "0 is refused, not raised to 1"
    );
    assert!(
        resolve_limit(Some(MAX_FEED_LIMIT + 1)).is_err(),
        "1001 is refused, not lowered to 1000"
    );
}

/// Fails if the largest position a plugin may issue mints a token the gateway's
/// own decoder would refuse.
///
/// Two constants in two crates, and nothing else holds them together.
/// `MAX_FEED_POSITION_BYTES` (SDK) bounds what a plugin may put in a position;
/// `MAX_CURSOR_TOKEN_CHARS` (this crate) bounds what `decode_cursor_token` will
/// accept back. [`mint_cursor`] is **total** — a position the plugin issued
/// cannot fail to encode, and the signature has nowhere to report it if it could
/// — so if the encoding of a maximum-size position ever exceeded the token
/// bound, the gateway would hand a consumer a continuation its very next request
/// refuses, with no error at the point the token was minted.
///
/// The round trip is asserted, not just the length: a token inside the bound
/// that no longer decodes to the position it was minted from would satisfy a
/// length-only check while breaking exactly the consumer this test is about.
#[test]
fn a_maximum_size_position_mints_a_token_inside_the_published_bound() {
    use usage_collector_sdk::v2::MAX_FEED_POSITION_BYTES;

    // The widest subscription too: `CursorV1::f` carries a
    // `subscription_fingerprint`, and while that is fixed-width today,
    // nothing here should assume the narrowest input.
    let widest = FeedSubscription::new((0..MAX_SUBSCRIPTION_TYPES).map(|i| {
        MeterTypeId::new(format!(
            "gts.cf.core.uc.usage_record.v2~cf.uc_wide._.meter{i}.v1~"
        ))
        .expect("valid meter id")
    }))
    .expect("a hundred-type subscription is non-empty");

    let biggest = FeedPosition::new(vec![0xFF; MAX_FEED_POSITION_BYTES])
        .expect("the maximum position size is admissible");
    let token = mint_cursor(&biggest, &widest)
        .encode()
        .expect("a minted cursor encodes");

    assert!(
        token.chars().count() <= MAX_CURSOR_TOKEN_CHARS,
        "a {MAX_FEED_POSITION_BYTES}-byte position minted a {}-character token, past the \
         {MAX_CURSOR_TOKEN_CHARS}-character bound this gateway's own decoder enforces",
        token.chars().count(),
    );
    let back = decode_cursor(&token, &widest, CursorField::Cursor)
        .expect("the gateway must be able to decode the token it just minted");
    assert_eq!(back.as_bytes(), biggest.as_bytes());
}

/// Fails if the comparison helper ignores any of `CursorV1`'s fields. The helper
/// is the ONLY way this slice compares cursors, so a blind spot in it is a blind
/// spot everywhere at once — which is why it is tested field by field rather
/// than trusted.
#[test]
fn the_cursor_comparison_helper_sees_every_field() {
    let base = CursorV1 {
        k: vec!["aaa".to_owned()],
        o: SortDir::Asc,
        s: FEED_CURSOR_SENTINEL.to_owned(),
        f: Some("ffff".to_owned()),
        d: "fwd".to_owned(),
    };
    assert!(
        cursors_equal(&base, &base.clone()),
        "a cursor equals itself"
    );

    let mut differs_in_k = base.clone();
    differs_in_k.k = vec!["bbb".to_owned()];
    assert!(
        !cursors_equal(&base, &differs_in_k),
        "a differing `k` must not compare equal"
    );

    let mut differs_in_o = base.clone();
    differs_in_o.o = SortDir::Desc;
    assert!(
        !cursors_equal(&base, &differs_in_o),
        "a differing `o` must not compare equal"
    );

    let mut differs_in_s = base.clone();
    differs_in_s.s = "other".to_owned();
    assert!(
        !cursors_equal(&base, &differs_in_s),
        "a differing `s` must not compare equal"
    );

    let mut differs_in_f = base.clone();
    differs_in_f.f = None;
    assert!(
        !cursors_equal(&base, &differs_in_f),
        "a differing `f` must not compare equal"
    );

    let mut differs_in_d = base.clone();
    differs_in_d.d = "bwd".to_owned();
    assert!(
        !cursors_equal(&base, &differs_in_d),
        "a differing `d` must not compare equal"
    );
}

// ── The feed gateway: `Service::read_usage_feed` ───────────────────────────
//
// The pure cursor core (minting, the subscription binding, the page-size
// bounds) is pinned in `feed_tests.rs`. This module pins the *dispatch*: that
// authorization runs ahead of any cursor diagnosis, that each cursor-bearing
// parameter is decoded on its own field name, that the resolved `limit`, the
// subscription and the `until` bound reach the SPI unchanged, and — the rule
// the whole component turns on — that the plugin's continuation is MAPPED
// rather than decided.
mod read_usage_feed_tests {
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_odata::{CursorV1, SortDir, ast};
    use toolkit_security::{
        AccessScope, ScopeConstraint, ScopeFilter, ScopeValue, SecurityContext, pep_properties,
    };
    use usage_collector_sdk::v2::{
        CursorField, FeedPage, FeedPosition, FeedStart, FeedSubscription, IdempotencyKey,
        MeterTypeId, RecordOrigin, ResourceRef, UsageCollectorError, UsageCollectorPluginError,
        UsageCollectorPluginV2, UsageRecord, ValidationReason,
    };
    use uuid::Uuid;

    use crate::v2::domain::Service;
    use crate::v2::domain::authz::{scope_to_odata_filter, usage_record};
    use crate::v2::domain::feed::{
        DEFAULT_FEED_LIMIT, MAX_SUBSCRIPTION_TYPES, mint_cursor, position_from_cursor,
    };
    use crate::v2::domain::test_support::{
        CountingPermitResolver, DenyAllResolver, HappyPathPlugin, RECORDING_PLUGIN_SUFFIX,
        ServiceFixture, authenticated_ctx, fake_declaration_source_with_fold, qty,
        recent_window_end, recent_window_start, recording_plugin_resolver,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");
    const OTHER_GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.seats_used.v1~");

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// More than one type, so a subscription that reached the SPI truncated —
    /// or one the gateway rebuilt from somewhere other than its argument — is
    /// visible rather than coincidentally right.
    fn subscription() -> FeedSubscription {
        FeedSubscription::new([
            MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            MeterTypeId::new(OTHER_GTS_ID).expect("valid gts_type_id"),
        ])
        .expect("a two-type subscription is non-empty")
    }

    fn position(bytes: &[u8]) -> FeedPosition {
        FeedPosition::new(bytes.to_vec()).expect("a non-empty position is valid")
    }

    /// A settled entry, distinguishable from every other `feed_entry(n)`.
    /// Several fields vary: an entry pass-through that dropped, duplicated or
    /// reordered the page leaves a `Vec` of the right length and the right
    /// *kind* of contents, which a fixture of identical rows cannot see.
    fn feed_entry(n: u8) -> usage_collector_sdk::v2::StoredUsageRecord {
        UsageRecord {
            id: Uuid::from_u128(u128::from(n)),
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id: Uuid::from_u128(2),
            resource_ref: ResourceRef::new(format!("rsc-{n}"), "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: std::collections::BTreeMap::new(),
            quantity: qty(&n.to_string()),
            idempotency_key: IdempotencyKey::new(format!("idem-{n}"))
                .expect("valid idempotency key"),
            accepted_at: recent_window_end(),
            origin: RecordOrigin::Live,
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
        // A plugin's page names its meter by reference and the gateway
        // re-attaches the identifier from the subscription it resolved, so the
        // reference must be what this suite's declaration double resolves.
        .into_stored(crate::v2::domain::test_support::declared_uuid(
            &MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
        ))
    }

    /// [`feed_entry`] under a caller-chosen meter — the only departure, and
    /// the whole of what the per-entry mapping test reads: this is the case
    /// that can tell a per-entry lookup from a per-page one.
    fn feed_entry_on(meter_id: &str, n: u8) -> usage_collector_sdk::v2::StoredUsageRecord {
        let meter = MeterTypeId::new(meter_id).expect("valid gts_type_id");
        UsageRecord {
            gts_type_id: meter.clone(),
            ..feed_entry(n).into_usage_record(meter.clone())
        }
        .into_stored(crate::v2::domain::test_support::declared_uuid(&meter))
    }

    /// `served` as the caller sees it: the gateway re-attaches the
    /// subscription's identifier to every entry a plugin answers with, so a
    /// test comparing a served page against what it programmed has to
    /// compare against the re-attached shape rather than the stored one.
    fn reattached(served: &[usage_collector_sdk::v2::StoredUsageRecord]) -> Vec<UsageRecord> {
        let meter = MeterTypeId::new(GTS_ID).expect("valid gts_type_id");
        served
            .iter()
            .map(|entry| entry.clone().into_usage_record(meter.clone()))
            .collect()
    }

    /// A feed cursor bound to [`subscription`], carrying `bytes`.
    fn cursor_at(bytes: &[u8]) -> CursorV1 {
        mint_cursor(&position(bytes), &subscription())
    }

    /// A well-formed `CursorV1` that is not a feed cursor: the raw read
    /// path's shape, which `position_from_cursor` refuses on its sentinel. It
    /// decodes, so a gateway that examined it would have something to say.
    fn foreign_cursor() -> CursorV1 {
        CursorV1 {
            k: vec!["2026-01-01T00:00:00Z".to_owned()],
            o: SortDir::Asc,
            s: "+window_end,+id".to_owned(),
            f: Some("deadbeef".to_owned()),
            d: "fwd".to_owned(),
        }
    }

    /// A `Service` over a [`HappyPathPlugin`] whose feed slot is programmed
    /// with `outcome`, wired against the read-path PDP fake.
    ///
    /// `with_source` gives [`subscription`]'s meters a resolvable
    /// declaration: the feed read path resolves every subscribed meter to its
    /// reference before dispatch, so a fixture without a working Type
    /// Resolver backend would fail closed before reaching `plugin`.
    fn service_with(
        outcome: Result<
            FeedPage<FeedPosition, usage_collector_sdk::v2::StoredUsageRecord>,
            UsageCollectorPluginError,
        >,
    ) -> (Arc<Service>, Arc<HappyPathPlugin>) {
        let plugin = HappyPathPlugin::new();
        match outcome {
            Ok(page) => plugin.set_read_feed_page(page),
            Err(err) => plugin.set_read_feed_page_err(err),
        }
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
                RECORDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    /// The same, with a PDP that denies every evaluation. The feed slot is
    /// left unprogrammed on purpose: reaching it at all is a failure.
    fn denied_service() -> (Arc<Service>, Arc<HappyPathPlugin>) {
        let plugin = HappyPathPlugin::new();
        let service = ServiceFixture::default()
            .with_resolver(Arc::new(DenyAllResolver))
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
                RECORDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    /// Each entry is re-attached with the identifier of **its own** meter,
    /// not with one meter's for the whole page.
    ///
    /// A page spans the whole subscription, so it carries entries of every
    /// subscribed meter, and the gateway looks each entry's reference up in
    /// the map it built. Every other feed test programs entries under one
    /// meter, so an implementation that checked membership and then stapled
    /// `meters[0].id` onto every entry passes all of them; this is the test
    /// that fails against it.
    ///
    /// The entries are under *different* meters and asserted per entry:
    /// whichever `FeedSubscription`'s sorted order puts first, one entry is
    /// not under `meters[0]`, so the defect moves its identifier whatever the
    /// sort does.
    #[tokio::test]
    async fn each_feed_entry_is_named_by_the_meter_its_own_reference_maps_to() {
        let first = feed_entry_on(GTS_ID, 1);
        let second = feed_entry_on(OTHER_GTS_ID, 2);
        assert_ne!(
            first.gts_type_uuid, second.gts_type_uuid,
            "test setup: the two entries must carry different references, or \
             the mapping has nothing to tell apart",
        );
        let (svc, _plugin) = service_with(Ok(FeedPage {
            entries: vec![first.clone(), second.clone()],
            next: Some(position(&[1])),
        }));

        let page = svc
            .read_usage_feed(&ctx(), &subscription(), FeedStart::Oldest, None, Some(50))
            .await
            .expect("a page over a two-meter subscription is served");

        let named: Vec<&str> = page
            .entries
            .iter()
            .map(|entry| entry.gts_type_id.as_str())
            .collect();
        assert_eq!(
            named,
            vec![GTS_ID, OTHER_GTS_ID],
            "each entry must be named by the meter its own reference maps to, \
             in the order the plugin settled them; a gateway that stapled one \
             meter's identifier onto the whole page names both entries the same",
        );
    }

    /// Fails if the gateway ever synthesises a `None` continuation: on a live
    /// read that tells a consumer a stream it is still following has ended,
    /// and nothing downstream would notice. A next cursor comes back with
    /// every page of a live read, short pages included.
    ///
    /// Also pins what the gateway handed the SPI: the whole subscription,
    /// `Oldest` as `Oldest`, no `until`, the resolved default page size, and
    /// the compiled PDP scope the page is filtered by. None of them is
    /// visible in the page that comes back, and each is something a gateway
    /// can drop while still answering `200`.
    #[tokio::test]
    async fn an_empty_live_page_keeps_its_continuation() {
        let (svc, plugin) = service_with(Ok(FeedPage {
            entries: vec![],
            next: Some(position(&[7])),
        }));

        let page = svc
            .read_usage_feed(&ctx(), &subscription(), FeedStart::Oldest, None, None)
            .await
            .expect("an empty live page is served");

        assert!(page.entries.is_empty());
        let next = page.next.expect(
            "an empty live page still carries its cursor; returning None here would report a \
             live stream as ended",
        );
        // Not merely `is_some()`: a cursor minted over another subscription,
        // or without the binding, is a token the next request refuses.
        // Recovering the plugin's own position through the subscription-bound
        // decode says the gateway minted the continuation the plugin issued.
        let recovered = position_from_cursor(&next, &subscription(), CursorField::Cursor)
            .expect("the minted continuation must decode against the same subscription");
        assert_eq!(recovered.as_bytes(), position(&[7]).as_bytes());

        let dispatch = plugin
            .last_read_feed_page_input()
            .expect("the feed page must have been dispatched");
        let dispatched_ids: Vec<MeterTypeId> = dispatch
            .types
            .iter()
            .map(|meter| meter.id.clone())
            .collect();
        assert_eq!(
            dispatched_ids,
            subscription().types(),
            "the whole subscription must reach the SPI; a truncated one silently drops a type \
             the consumer is still reading",
        );
        assert_eq!(dispatch.start, FeedStart::Oldest);
        assert!(dispatch.until.is_none());
        assert_eq!(
            dispatch.limit, DEFAULT_FEED_LIMIT,
            "a caller naming no limit must reach the SPI with the resolved default, never a \
             bound the plugin would refuse",
        );
        // The authorization decision itself, and the one dispatch argument no
        // other test here reads. `HappyPathPlugin` replays its programmed page
        // whatever scope it is handed, so passing a constant-true expression
        // leaves every other assertion green while serving a cross-tenant
        // page. Compared against `scope_to_odata_filter` of the fixture
        // resolver's own permit rather than a hand-written `Expr`: restating
        // the projection's output would pin the shape, not the decision.
        //
        // Compared through `Debug` because `ast::Expr` derives no `PartialEq`.
        // That is sound here: both sides come out of the same projection
        // function, so a rendering change moves them together.
        assert_eq!(
            format!("{:?}", dispatch.scope),
            format!("{:?}", expected_feed_scope()),
            "the page must be filtered by the scope this caller's permit compiled to",
        );
    }

    /// The `ast::Expr` `recording_plugin_resolver`'s permit projects to. That
    /// resolver answers one `OWNER_TENANT_ID` constraint whatever the request,
    /// and `scope_to_odata_filter` is the same projection
    /// `authorize_read_usage_feed` runs.
    fn expected_feed_scope() -> ast::Expr {
        scope_to_odata_filter(&AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(
                pep_properties::OWNER_TENANT_ID,
                ScopeValue::String(Uuid::from_u128(2).to_string()),
            ),
        ])))
        .expect("the fixture's tenant-pinned permit projects")
    }

    /// Fails if a live page the plugin served with no continuation is refused,
    /// or repaired, rather than served as it stands.
    ///
    /// The obligation to carry a next cursor is the plugin's, and this gateway
    /// diagnoses a violation rather than deciding `next` for it: the entries
    /// on such a page are correct and final, so refusing would deny a charging
    /// consumer rows it is entitled to over the plugin's bookkeeping, and
    /// repairing is worse still since the gateway holds no position to mint
    /// one from. The `tracing::error!` emitted alongside is not asserted —
    /// nothing here captures it — but the disposition is.
    #[tokio::test]
    async fn a_live_page_without_a_continuation_is_served_not_refused() {
        let served = vec![feed_entry(3)];
        let (svc, _plugin) = service_with(Ok(FeedPage {
            entries: served.clone(),
            next: None,
        }));

        let page = svc
            .read_usage_feed(&ctx(), &subscription(), FeedStart::Oldest, None, None)
            .await
            .expect("a non-conforming live page is diagnosed, not refused");

        assert_eq!(
            page.entries,
            reattached(&served),
            "the entries must reach the caller unchanged, bar the identifier the gateway \
             staples back on; the plugin's defect is in the continuation, not in the rows",
        );
        assert!(
            page.next.is_none(),
            "the gateway must not invent a continuation it holds no position for",
        );
    }

    /// Fails if a page longer than the limit it was dispatched under is
    /// handed to a caller.
    ///
    /// `FeedPage.entries` carries a published `maxItems` and the resolved
    /// `limit` is at most that, so a page over the limit is over the schema
    /// too — the feed's half of the aggregate path's over-cap refusal.
    ///
    /// Discriminating on the variant *and* the detail: `Internal` is also what
    /// an unrecognised `FeedStart` raises, so `is_err()` alone would pass
    /// under a gateway that tripped on something else.
    #[tokio::test]
    async fn a_page_longer_than_its_limit_is_refused_not_served() {
        let over_limit = vec![feed_entry(1), feed_entry(2), feed_entry(3)];
        let (svc, _plugin) = service_with(Ok(FeedPage {
            entries: over_limit,
            next: Some(position(&[5])),
        }));

        let err = svc
            .read_usage_feed(&ctx(), &subscription(), FeedStart::Oldest, None, Some(2))
            .await
            .expect_err("a page longer than its limit is not served");

        match err {
            UsageCollectorError::Internal { detail } => {
                assert!(
                    detail.contains("3 feed entries") && detail.contains("limit of 2"),
                    "the refusal must name both figures so an operator can attribute it, \
                     got: {detail}"
                );
            }
            other => panic!("expected Internal naming the over-long page, got {other:?}"),
        }
    }

    /// Fails if an in-process caller can hand the plugin a subscription
    /// wider than the published ceiling.
    ///
    /// `feed::build_subscription` enforces that ceiling on the repeated query
    /// parameter and its only call site is the REST handler;
    /// `FeedSubscription::new` enforces only non-emptiness. So this is
    /// reachable through the in-process client, and it matters here because
    /// the storage plugin shapes its feed page statement as one lateral join
    /// per subscribed type.
    ///
    /// Asserted on `field`, not merely the variant: this path's other
    /// `InvalidArgument`s name `limit` and `cursor`.
    #[tokio::test]
    async fn an_in_process_subscription_over_the_ceiling_is_refused_before_dispatch() {
        let (svc, plugin) = service_with(Ok(FeedPage {
            entries: vec![],
            next: None,
        }));
        let wide = FeedSubscription::new((0..=MAX_SUBSCRIPTION_TYPES).map(|i| {
            MeterTypeId::new(format!(
                "gts.cf.core.uc.usage_record.v2~cf.uc_wide._.meter{i}.v1~"
            ))
            .expect("valid gts_type_id")
        }))
        .expect("a wide subscription is still non-empty");

        let err = svc
            .read_usage_feed(&ctx(), &wide, FeedStart::Oldest, None, None)
            .await
            .expect_err("a subscription past the published ceiling is refused");

        match err {
            UsageCollectorError::InvalidArgument { field, .. } => {
                assert_eq!(field, "gts_type_id");
            }
            other => panic!("expected InvalidArgument on `gts_type_id`, got {other:?}"),
        }
        assert!(
            plugin.last_read_feed_page_input().is_none(),
            "an over-wide subscription must never reach the SPI",
        );
    }

    /// Fails if the feed serves a read under a PDP permit whose constraint
    /// this gear cannot project.
    ///
    /// The fail-closed posture [`scope_to_odata_filter`] gives
    /// `authorize_read_usage_feed`, driven on the one permit shape that
    /// reaches that gate: a permit carrying **no** constraints fails closed
    /// upstream in `PolicyEnforcer` and says nothing about this gear's
    /// projection, while a non-empty constraint that does not pin
    /// `owner_tenant_id` compiles fine and is then denied there.
    ///
    /// Deleting the `scope_to_odata_filter` call from
    /// `authorize_read_usage_feed` reds this.
    #[tokio::test]
    async fn a_permit_without_tenant_narrowing_denies_the_feed_before_dispatch() {
        let plugin = HappyPathPlugin::new();
        let service = ServiceFixture::default()
            .with_resolver(CountingPermitResolver::new(
                usage_record::PROP_RESOURCE_TYPE,
                "compute.vm".to_owned(),
            ))
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
                RECORDING_PLUGIN_SUFFIX,
            );

        let err = service
            .read_usage_feed(&ctx(), &subscription(), FeedStart::Oldest, None, None)
            .await
            .expect_err("a permit this gear cannot project must not serve a page");

        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "expected PermissionDenied from the projection gate; got {err:?}"
        );
        assert!(
            plugin.last_read_feed_page_input().is_none(),
            "a read that failed closed must never reach the SPI",
        );
    }

    /// Fails if a **short** live page loses its continuation, and if the
    /// entries the plugin served do not reach the caller intact.
    ///
    /// The neighbouring tests use empty pages, so a gateway that inferred
    /// "fewer entries than the limit ⇒ caught up ⇒ no continuation" would pass
    /// both. A next cursor comes back with every page of a live read, short
    /// pages included; a `None` here tells a consumer a stream it is still
    /// following has ended.
    ///
    /// It carries the entry-payload assertion too, because nothing else here
    /// reads `page.entries`: every other test programs an empty page, and the
    /// metrics suite observes the page *size* rather than its contents.
    #[tokio::test]
    async fn a_short_live_page_keeps_its_continuation_and_its_entries() {
        let served = vec![feed_entry(1), feed_entry(2)];
        let (svc, plugin) = service_with(Ok(FeedPage {
            entries: served.clone(),
            next: Some(position(&[9])),
        }));

        let page = svc
            .read_usage_feed(&ctx(), &subscription(), FeedStart::Oldest, None, Some(50))
            .await
            .expect("a short live page is served");

        assert_eq!(
            page.entries,
            reattached(&served),
            "the entries the plugin settled must reach the caller unchanged and in order, bar \
             the identifier the gateway staples back on",
        );
        assert!(
            page.next.is_some(),
            "a short live page still carries its cursor; returning None here would report a \
             live stream as ended on the very page a backlog drains to",
        );

        // The premise asserted rather than assumed: this page is short of the
        // limit it was read under. Without it "short" would be a property of
        // the fixture's prose rather than of the run.
        let dispatch = plugin
            .last_read_feed_page_input()
            .expect("the feed page must have been dispatched");
        assert_eq!(dispatch.limit, 50);
        assert!(
            page.entries.len() < usize::try_from(dispatch.limit).expect("a limit fits usize"),
            "the fixture must serve fewer entries than the limit, or this is not a short page",
        );
    }

    /// Fails if a `start` cursor defect is reported on any field but `cursor`;
    /// [`an_unusable_until_is_refused_on_its_own_field`] covers the other
    /// half, and neither implies the other. `position_from_cursor`'s `field`
    /// argument decides nothing except which error is produced, so the call
    /// sites can drift independently.
    ///
    /// Against a **permitted** service: a denying one short-circuits at the
    /// PDP and never reaches the decode, which is what
    /// [`a_denied_caller_is_denied_before_the_cursor_is_examined`] pins.
    #[tokio::test]
    async fn an_unusable_start_cursor_is_refused_on_the_cursor_field() {
        let foreign = foreign_cursor();
        let (svc, plugin) = service_with(Ok(FeedPage {
            entries: vec![],
            next: None,
        }));

        let err = svc
            .read_usage_feed(
                &ctx(),
                &subscription(),
                FeedStart::After(&foreign),
                None,
                None,
            )
            .await
            .expect_err("a `start` cursor that is not a feed cursor is refused");

        match err {
            UsageCollectorError::CursorRejected { field, .. } => {
                assert_eq!(field, CursorField::Cursor);
            }
            other => panic!("expected CursorRejected on `cursor`, got {other:?}"),
        }
        assert!(
            plugin.last_read_feed_page_input().is_none(),
            "an unusable resume cursor must be refused before dispatch",
        );
    }

    /// Fails if a backward feed cursor reaches the SPI.
    ///
    /// The feed's counterpart to the raw path's
    /// `a_backward_cursor_never_reaches_the_plugin`, asserted at the service
    /// rather than only at `position_from_cursor`: the guard is worth nothing
    /// if the read path around it dispatches anyway.
    ///
    /// The fixture is [`cursor_at`]'s minted cursor with `d` flipped, so
    /// sentinel, fingerprint and position are what this gear itself mints and
    /// direction is the only thing wrong with the token.
    #[tokio::test]
    async fn a_backward_feed_cursor_never_reaches_the_plugin() {
        let mut backward = cursor_at(&[1]);
        // The one thing wrong with this token.
        backward.d = "bwd".to_owned();
        let (svc, plugin) = service_with(Ok(FeedPage {
            entries: vec![],
            next: None,
        }));

        let err = svc
            .read_usage_feed(
                &ctx(),
                &subscription(),
                FeedStart::After(&backward),
                None,
                None,
            )
            .await
            .expect_err("a backward cursor must be refused: the feed reads forward only");

        match err {
            UsageCollectorError::CursorRejected { source, field, .. } => {
                assert!(
                    matches!(source, toolkit_odata::Error::InvalidCursor),
                    "expected upstream's InvalidCursor, got {source:?}",
                );
                assert_eq!(field, CursorField::Cursor);
            }
            other => panic!("expected a cursor rejection, got {other:?}"),
        }
        assert!(
            plugin.last_read_feed_page_input().is_none(),
            "a refused continuation MUST NOT reach the plugin",
        );
    }

    /// Fails if a backward `until` bound reaches the SPI.
    ///
    /// The `until` half of the pair above. `read_usage_feed` decodes each
    /// cursor-bearing parameter at its own call site, so a guard reached from
    /// one is not reached from the other, and the field the refusal names is
    /// what tells a consumer which token to drop.
    #[tokio::test]
    async fn a_backward_until_bound_never_reaches_the_plugin() {
        let mut backward = cursor_at(&[9]);
        backward.d = "bwd".to_owned();
        let (svc, plugin) = service_with(Ok(FeedPage {
            entries: vec![],
            next: None,
        }));

        let err = svc
            .read_usage_feed(
                &ctx(),
                &subscription(),
                FeedStart::Oldest,
                Some(&backward),
                None,
            )
            .await
            .expect_err("a backward `until` bound must be refused");

        match err {
            UsageCollectorError::CursorRejected { field, .. } => {
                assert_eq!(field, CursorField::Until);
            }
            other => panic!("expected CursorRejected on `until`, got {other:?}"),
        }
        assert!(
            plugin.last_read_feed_page_input().is_none(),
            "a refused bound MUST NOT reach the plugin",
        );
    }

    /// Fails if the gateway synthesises a `Some` when the plugin said the
    /// bounded replay is done.
    ///
    /// The mirror of the test above, and neither stands alone: a gateway
    /// hard-coding `None` passes this one and fails that one, and a gateway
    /// hard-coding `Some` fails this one. Only the pair pins the `map`.
    #[tokio::test]
    async fn a_bounded_replay_at_its_until_returns_no_continuation() {
        let until = cursor_at(&[9]);
        let (svc, _plugin) = service_with(Ok(FeedPage {
            entries: vec![],
            next: None,
        }));

        let page = svc
            .read_usage_feed(
                &ctx(),
                &subscription(),
                FeedStart::Oldest,
                Some(&until),
                None,
            )
            .await
            .expect("a completed bounded replay is served");

        assert!(
            page.next.is_none(),
            "a completed bounded replay carries no continuation",
        );
    }

    /// Fails if a malformed cursor is diagnosed before the caller is
    /// authorized, which would tell an unauthorized caller that their token
    /// was well-formed — and, for a token that *is* well-formed, whether it
    /// names a subscription they may not read.
    ///
    /// Discriminating on the variant: a gateway that decoded first would
    /// answer `CursorRejected` for this input, because the cursor supplied
    /// is not a feed cursor. Both orderings produce an `Err`; only one
    /// produces this variant.
    #[tokio::test]
    async fn a_denied_caller_is_denied_before_the_cursor_is_examined() {
        let (svc, plugin) = denied_service();
        let foreign = foreign_cursor();

        let err = svc
            .read_usage_feed(
                &ctx(),
                &subscription(),
                FeedStart::After(&foreign),
                None,
                None,
            )
            .await
            .expect_err("a denied caller is denied");

        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "expected PermissionDenied before any cursor diagnosis; got {err:?}"
        );
        assert!(
            plugin.last_read_feed_page_input().is_none(),
            "a denied read must never reach the SPI",
        );
    }

    /// Fails if the gateway invents an ordering comparison between `until`
    /// and the resume cursor. Positions are opaque bytes here, so the
    /// gateway has no basis for one; the plugin answers an empty page.
    ///
    /// The `Ok` alone would not catch a gateway that silently *dropped*
    /// `until` instead of comparing it — an unbounded read answering `200` —
    /// so the recorded dispatch is asserted: both bounds must arrive at the
    /// SPI as the caller gave them.
    #[tokio::test]
    async fn an_until_behind_the_cursor_is_passed_through_not_compared() {
        let later = cursor_at(&[9]);
        let earlier = cursor_at(&[1]);
        let (svc, plugin) = service_with(Ok(FeedPage {
            entries: vec![],
            next: None,
        }));

        let page = svc
            .read_usage_feed(
                &ctx(),
                &subscription(),
                FeedStart::After(&later),
                Some(&earlier),
                None,
            )
            .await
            .expect("the gateway does not refuse this; the plugin answers it");

        assert!(page.entries.is_empty());
        let dispatch = plugin
            .last_read_feed_page_input()
            .expect("the read must have reached the SPI rather than being refused here");
        assert_eq!(dispatch.start, FeedStart::After(position(&[9])));
        assert_eq!(
            dispatch.until.map(|p| p.as_bytes().to_vec()),
            Some(vec![1]),
            "the `until` bound must reach the plugin exactly as given, behind the cursor or not",
        );
    }

    /// Fails if the plugin's retention refusal does not reach the caller as
    /// the documented 400. The lift lives in `domain/error.rs`; this pins
    /// that the feed path reaches it.
    ///
    /// Discriminating on the typed reason, not the variant: the feed path
    /// raises `InvalidArgument` for an out-of-bound `limit` too.
    #[tokio::test]
    async fn a_plugin_retention_refusal_surfaces_as_cursor_beyond_retention() {
        let resume = cursor_at(&[4]);
        let (svc, _plugin) = service_with(Err(UsageCollectorPluginError::CursorBeyondRetention));

        let err = svc
            .read_usage_feed(
                &ctx(),
                &subscription(),
                FeedStart::After(&resume),
                None,
                None,
            )
            .await
            .expect_err("a refused cursor is an error");

        match err {
            UsageCollectorError::InvalidArgument { field, reason, .. } => {
                assert_eq!(field, "cursor");
                assert_eq!(reason, ValidationReason::CursorBeyondRetention);
            }
            other => panic!("expected InvalidArgument/CursorBeyondRetention, got {other:?}"),
        }
    }

    /// Fails if the `until` bound is refused under the `cursor` parameter's
    /// name: a consumer fixing a stale token needs to be told which one.
    ///
    /// Discriminating on `field`, since every guard on the decode path raises
    /// the same `CursorRejected` variant — a variant-only assertion would pass
    /// with the `until` half decoded under `CursorField::Cursor`.
    #[tokio::test]
    async fn an_unusable_until_is_refused_on_its_own_field() {
        let foreign = foreign_cursor();
        let (svc, plugin) = service_with(Ok(FeedPage {
            entries: vec![],
            next: None,
        }));

        let err = svc
            .read_usage_feed(
                &ctx(),
                &subscription(),
                FeedStart::Oldest,
                Some(&foreign),
                None,
            )
            .await
            .expect_err("an `until` that is not a feed cursor is refused");

        match err {
            UsageCollectorError::CursorRejected { field, .. } => {
                assert_eq!(field, CursorField::Until);
            }
            other => panic!("expected CursorRejected on `until`, got {other:?}"),
        }
        assert!(
            plugin.last_read_feed_page_input().is_none(),
            "an unusable bound must be refused before dispatch",
        );
    }

    /// Fails if a page size outside the published bound reaches the plugin,
    /// which documents exactly that as a host-contract breach it answers
    /// with `Internal` — turning a caller's 400 into a 500.
    ///
    /// Discriminating on `field`: this path's other `InvalidArgument` is the
    /// retention refusal, which names `cursor`.
    #[tokio::test]
    async fn a_limit_outside_the_published_bound_is_refused_before_dispatch() {
        let (svc, plugin) = service_with(Ok(FeedPage {
            entries: vec![],
            next: None,
        }));

        let err = svc
            .read_usage_feed(&ctx(), &subscription(), FeedStart::Oldest, None, Some(0))
            .await
            .expect_err("a zero-entry page is refused");

        match err {
            UsageCollectorError::InvalidArgument { field, .. } => assert_eq!(field, "limit"),
            other => panic!("expected InvalidArgument on `limit`, got {other:?}"),
        }
        assert!(
            plugin.last_read_feed_page_input().is_none(),
            "an out-of-bound limit must never reach the SPI",
        );
    }
}
