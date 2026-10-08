//! Unit tests for the Feed Gateway's pure core: subscription binding, the wire
//! cursor a plugin's opaque [`FeedPosition`] travels inside, and the published
//! page-size / token-length bounds.
//!
//! Fixture meter type ids below are five-token derivations
//! (`vendor.package.namespace.type.vMAJOR`, `~`-terminated) — the shape
//! `MeterTypeId::new` (via the upstream `gts` crate's `GtsId::try_new`)
//! requires.

use toolkit_odata::{CursorV1, SortDir};
use usage_collector_sdk::{FeedPosition, FeedSubscription, MeterTypeId, UsageCollectorError};

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
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
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
    let s = sub(&["gts.cf.core.uc.usage_record.v1~vendor.a._.meter.v1~"]);
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
        "gts.cf.core.uc.usage_record.v1~vendor.a._.meter.v1~",
        "gts.cf.core.uc.usage_record.v1~vendor.b._.meter.v1~",
    ]);
    let narrow = sub(&["gts.cf.core.uc.usage_record.v1~vendor.a._.meter.v1~"]);
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
        "gts.cf.core.uc.usage_record.v1~vendor.b._.meter.v1~",
        "gts.cf.core.uc.usage_record.v1~vendor.a._.meter.v1~",
    ]);
    let b = sub(&[
        "gts.cf.core.uc.usage_record.v1~vendor.a._.meter.v1~",
        "gts.cf.core.uc.usage_record.v1~vendor.b._.meter.v1~",
        "gts.cf.core.uc.usage_record.v1~vendor.a._.meter.v1~",
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
        &sub(&["gts.cf.core.uc.usage_record.v1~vendor.a._.meter.v1~"]),
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
    let s = sub(&["gts.cf.core.uc.usage_record.v1~vendor.a._.meter.v1~"]);
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
/// here would do exactly that — it hard-codes `CursorField::Cursor` — and ruling
/// E7 wants `until` refused on its own name.
#[test]
fn a_backward_until_bound_is_refused_on_its_own_field() {
    let s = sub(&["gts.cf.core.uc.usage_record.v1~vendor.a._.meter.v1~"]);
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
        .map(|i| format!("gts.cf.core.uc.usage_record.v1~vendor.t{i}._.meter.v1~"))
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
    let s = sub(&["gts.cf.core.uc.usage_record.v1~vendor.a._.meter.v1~"]);
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
    let s = sub(&["gts.cf.core.uc.usage_record.v1~vendor.a._.meter.v1~"]);
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
    use usage_collector_sdk::MAX_FEED_POSITION_BYTES;

    // The widest subscription too: `CursorV1::f` carries a
    // `subscription_fingerprint`, and while that is fixed-width today,
    // nothing here should assume the narrowest input.
    let widest = FeedSubscription::new((0..MAX_SUBSCRIPTION_TYPES).map(|i| {
        MeterTypeId::new(format!(
            "gts.cf.core.uc.usage_record.v1~cf.uc_wide._.meter{i}.v1~"
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

// ── Ruling E3: the age-absence pin ──────────────────────────────────────

/// One thing the age-absence pin below inspects: a labelled block of source
/// text. `service.rs` cannot be checked as a whole file (its ingestion and
/// query paths legitimately call `Instant::now`), so its arm — when present
/// at all — carries only `read_usage_feed`'s sliced body, never the file's
/// full text; every other source's `code` is its whole file.
struct FeedSource {
    label: String,
    code: String,
}

/// Recursively collect every `.rs` file under `dir` whose **file name** contains
/// `needle`.
///
/// The file name, not the path. Matching `path.to_string_lossy()` matches the
/// *absolute* path, so a checkout under any directory whose name contains `feed`
/// pulls every `.rs` file in `src/` into the scan. The property under test is
/// about which *modules* are feed modules; where the repository happens to be
/// checked out is not one of its inputs.
fn collect_rs_files_containing(dir: &std::path::Path, needle: &str, out: &mut Vec<FeedSource>) {
    // A read failure here must be loud, naming the path and the `io::Error` —
    // never an omission the floor downstream could absorb. Swallowed silently, a
    // single transient read failure drops one source, the floor still passes at
    // one below the full set, and this pin quietly stops covering the file it
    // dropped while still reporting success.
    let entries = std::fs::read_dir(dir).unwrap_or_else(|err| {
        panic!(
            "feed source scan could not read directory {}: {err}",
            dir.display()
        )
    });
    for entry in entries {
        let entry = entry.unwrap_or_else(|err| {
            panic!(
                "feed source scan could not read a directory entry under {}: {err}",
                dir.display()
            )
        });
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files_containing(&path, needle, out);
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "rs")
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains(needle))
        {
            let code = std::fs::read_to_string(&path).unwrap_or_else(|err| {
                panic!("feed source scan could not read {}: {err}", path.display())
            });
            out.push(FeedSource {
                label: path.display().to_string(),
                code,
            });
        }
    }
}

/// Slice a `pub async fn <fn_name>` method's source out of an `impl` block, from
/// its signature up to whichever comes first: the next `pub async fn` at the same
/// four-space indent, or the `}` that closes the enclosing `impl` block (column
/// 0). Returns `None` when `fn_name` is not found at all.
///
/// The `}`-at-column-0 boundary is what keeps the slice honest when the method is
/// the *last* in `impl Service`: a slice bounded only by "the next `pub async
/// fn`" would run past the `impl` block's closing brace and into whatever
/// follows, picking up unrelated code and its own legitimate `Instant::now`.
fn slice_function_body(text: &str, fn_name: &str) -> Option<String> {
    let marker = format!("pub async fn {fn_name}(");
    let start = text.find(&marker)?;
    let rest = &text[start..];
    // Search from byte 1 so the function's own opening brace / signature
    // text can't re-match the needle at offset 0.
    let tail = &rest[1..];
    let next_method = tail.find("\n    pub async fn ").map(|i| i + 1);
    let end_of_impl = tail.find("\n}\n").map(|i| i + 1);
    let end = match (next_method, end_of_impl) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    };
    Some(match end {
        Some(end) => rest[..end].to_owned(),
        None => rest.to_owned(),
    })
}

/// Derived, not hard-coded: walks `src/` for every feed-named source, plus
/// `read_usage_feed`'s body in `domain/service.rs` when that function exists.
///
/// The scan below asserts that arm is present rather than relying on it: a slice
/// that finds nothing contributes nothing silently, and a silent pass over the
/// file most likely to acquire an age check by accident is worth nothing.
fn collect_feed_sources(src_root: &std::path::Path) -> Vec<FeedSource> {
    let mut sources = Vec::new();
    collect_rs_files_containing(src_root, "feed", &mut sources);

    // A read failure is loud here too: `service.rs` not existing at all would be
    // a real finding, not an omission this function should fold into "the method
    // arm is simply absent" — that absence comes from `slice_function_body`
    // returning `None`, a distinct condition from the file being unreadable.
    let service_path = src_root.join("domain").join("service.rs");
    let text = std::fs::read_to_string(&service_path).unwrap_or_else(|err| {
        panic!(
            "feed source scan could not read {}: {err}",
            service_path.display()
        )
    });
    if let Some(body) = slice_function_body(&text, "read_usage_feed") {
        sources.push(FeedSource {
            label: format!("{}{SERVICE_ARM_LABEL_SUFFIX}", service_path.display()),
            code: body,
        });
    }
    sources
}

/// The tail every [`FeedSource`] built from a sliced `service.rs` method
/// carries, and the only thing that tells that arm apart from a whole-file one.
/// Declared once so the label's shape has a single owner: the scan below both
/// asserts the arm is present and exempts it from the monotonic-latency needles,
/// and a mismatch between those spellings and the one [`collect_feed_sources`]
/// mints would be a silent pass, not an error.
const SERVICE_ARM_LABEL_SUFFIX: &str = "::read_usage_feed";

/// Strip `//`-to-end-of-line and `/* */` comments from `text`.
///
/// Line/character based and does not understand string literals — sufficient
/// here because the subject is this crate's own source, not adversarial
/// input, and the brief asks for exactly this rather than a parser.
fn strip_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
                out.push(c);
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if c == '*' && bytes.get(i + 1) == Some(&b'/') {
                in_block_comment = false;
                i += 2;
                continue;
            }
            if c == '\n' {
                out.push(c);
            }
            i += 1;
            continue;
        }
        if c == '/' && bytes.get(i + 1) == Some(&b'/') {
            in_line_comment = true;
            i += 2;
            continue;
        }
        if c == '/' && bytes.get(i + 1) == Some(&b'*') {
            in_block_comment = true;
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Remove the age-absence scan's own needle-list constant from `code`. Used only
/// against this file's own text before the age-absence scan below runs over it —
/// see the call site for why.
///
/// Deliberately does not spell that constant's declaration out in this docstring:
/// the search below (which also must not name it verbatim, for the same reason)
/// would otherwise match the docstring's own mention before it ever reached the
/// real one, silently redacting nothing. The needle has to name enough of the
/// declaration's exact syntax — including its trailing newline — that only the
/// real, formatted declaration can match it.
fn redact_forbidden_needle_list(code: &str) -> String {
    const DECL_START: &str = "const FORBIDDEN: &[(&str, Reach)] = &[\n";
    let Some(start) = code.find(DECL_START) else {
        return code.to_owned();
    };
    let Some(rel_end) = code[start..].find("];") else {
        return code.to_owned();
    };
    let end = start + rel_end + 2;
    format!("{}{}", &code[..start], &code[end..])
}

/// How widely one forbidden needle applies.
///
/// The distinction exists because one source in the scan is not a module but
/// a *method* — `Service::read_usage_feed` — and a method on `Service` is a
/// metered operation before it is a feed path. See [`Reach::FeedModulesOnly`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// Every scanned source, the `service.rs` method arm included. These are
    /// the needles that can actually express an entry's age: a wall-clock
    /// read, or a named horizon / age quantity. Nothing on the feed path may
    /// contain one, wherever it lives.
    EverySource,
    /// The feed-named modules only (`feed.rs`, `feed_tests.rs`, and any
    /// feed-named file a later task adds) — **not** `read_usage_feed`'s body.
    ///
    /// Reserved for the monotonic-latency needles, `Instant::now` and
    /// `elapsed()`, and narrowed here rather than deleted, exactly as this pin's
    /// own failure message directs. Independent reasons, either sufficient:
    ///
    /// 1. **A monotonic instant cannot express an age.** `std::time::Instant` is
    ///    explicitly unrelated to wall-clock time and comparable only with
    ///    another `Instant` from the same process; no arithmetic takes one and a
    ///    ledger timestamp and yields how old a position is. Every needle that
    ///    *could* do that stays at [`Reach::EverySource`] above, so the property
    ///    this pin exists to hold is untouched.
    /// 2. **DESIGN §3.11.5 requires the read to be timed.**
    ///    `uc_feed_page_duration_seconds` is a published instrument, and the gear
    ///    measures a request's wall-clock the way every other instrumented
    ///    operation in `service.rs` does: an instant taken on entry, an
    ///    `elapsed()` read on the way out. Forbidding that spelling inside
    ///    `read_usage_feed` would forbid instrumenting the feed at all, or push
    ///    the timing into a helper written solely to sit outside this scan.
    ///
    /// The feed modules keep the needles because neither has any business timing
    /// anything: `feed.rs` is pure, total, clock-free cursor logic.
    FeedModulesOnly,
}

/// Fails if any feed-path source grows an age, horizon or clock comparison.
///
/// Ruling E3: ADR-0011's two zones are zones of published guarantee, not
/// branches, and DESIGN §3.1 calls a position's age "a progress measure, not
/// the retention refusal's input". An absence nothing can fail on is how this
/// programme's own "bounded band" survived two slices before ruling D1 caught
/// it, so the absence is pinned rather than asserted in prose.
#[test]
fn no_feed_path_source_computes_an_age() {
    const FORBIDDEN: &[(&str, Reach)] = &[
        ("OffsetDateTime::now", Reach::EverySource),
        ("SystemTime::now", Reach::EverySource),
        ("Instant::now", Reach::FeedModulesOnly),
        ("elapsed()", Reach::FeedModulesOnly),
        // No `replay_horizon` entry: `horizon` above is a strict substring
        // of it, so the longer needle could never match anything the
        // shorter one had not already caught. A list entry that cannot
        // fail is a line a reader takes for coverage it does not have.
        ("horizon", Reach::EverySource),
        ("age_secs", Reach::EverySource),
        ("Duration::from_secs", Reach::EverySource),
    ];

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    // Derived, not hard-coded: a task adding a fourth feed file must not
    // silently fall out of this check's coverage.
    let sources = collect_feed_sources(&root.join("src"));
    assert!(
        sources.len() >= 2,
        "expected at least the feed module and its own tests; found {:?}",
        sources.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
    // The derived floor above cannot see the one arm that is a *method* rather
    // than a file: `collect_feed_sources` contributes the `service.rs` arm only
    // when `slice_function_body` finds the literal `pub async fn
    // read_usage_feed(`, and contributes nothing — silently — when it does not.
    // Renaming the method, changing its visibility or its `async`-ness, or moving
    // it out of `service.rs` would restore that silence, so its presence is
    // asserted rather than counted.
    assert!(
        sources
            .iter()
            .any(|s| s.label.ends_with(SERVICE_ARM_LABEL_SUFFIX)),
        "the Service::read_usage_feed arm must be among the scanned sources; \
         `slice_function_body` found no `pub async fn read_usage_feed(` in \
         domain/service.rs. Found {:?}",
        sources.iter().map(|s| &s.label).collect::<Vec<_>>()
    );

    for source in &sources {
        // This test's own `FORBIDDEN` list is, unavoidably, a source of every
        // substring it forbids — `feed_tests.rs` matches the collection rule like
        // every other test file here, so the naive scan finds this array literal
        // inside itself. Only that one declaration is redacted before matching;
        // everything else in this file stays covered, so a genuine age check
        // added anywhere else here still trips the pin.
        let raw = if source.label.ends_with("feed_tests.rs") {
            redact_forbidden_needle_list(&source.code)
        } else {
            source.code.clone()
        };
        // Strip comments before matching: this module's own docs discuss age
        // at length, and it is the CODE that must not compute one.
        let code = strip_comments(&raw);
        let is_service_arm = source.label.ends_with(SERVICE_ARM_LABEL_SUFFIX);
        for (needle, reach) in FORBIDDEN {
            if is_service_arm && *reach == Reach::FeedModulesOnly {
                continue;
            }
            assert!(
                !code.contains(needle),
                "{} contains `{needle}`; the Feed Gateway computes no age (ruling E3, \
                 spec section 3.5). If this is a false positive, narrow the pattern - do \
                 not delete the check.",
                source.label
            );
        }
    }
}
