//! Handler-level unit tests for `GET /usage-collector/v1/feed`.
//!
//! Scope: pin handler-shaped concerns neither `domain::feed_tests` (the
//! pure cursor core) nor
//! `domain::service_tests::read_usage_feed_tests` (the service's own
//! dispatch guarantees) can reach — how the wire query parameters fold
//! into the typed arguments `Service::read_usage_feed` takes, and how its
//! returned `FeedPage<CursorV1>` projects onto the wire `FeedPageDto`.
//! Specifically: an absent `cursor` compiles to `FeedStart::Oldest`
//! rather than an absent position; a repeated `gts_type_id` builds a
//! multi-type subscription; an over-breadth subscription, a malformed
//! `cursor`, a malformed `until` and a repeated optional parameter are
//! each refused before the plugin is ever dispatched, naming their own
//! parameter; and the DTO's `next_cursor` round-trips, `prev_cursor` is
//! always `null`, and `page_info.limit` carries the resolved default.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Extension, Query};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::Value;
use toolkit_gts::gts_id;
use toolkit_odata::CursorV1;
use usage_collector_sdk::{
    CursorField, FeedPage, FeedPosition, FeedStart, FeedSubscription, IdempotencyKey, MeterTypeId,
    RecordOrigin, ResourceRef, StoredUsageRecord, UsageCollectorPluginError,
    UsageCollectorPluginV1, UsageRecord,
};
use uuid::Uuid;

use super::handle_read_usage_feed;
use crate::domain::Service;
use crate::domain::feed;
use crate::domain::feed::{DEFAULT_FEED_LIMIT, cursors_equal, mint_cursor, position_from_cursor};
use crate::domain::test_support::{
    HappyPathPlugin, RECORDING_PLUGIN_SUFFIX, ServiceFixture, authenticated_ctx, declared_uuid,
    fake_declaration_source_with_fold, qty, recent_window_end, recent_window_start,
    recording_plugin_resolver,
};

const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");
const OTHER_GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.seats_used.v1~");

/// Two types, so a subscription the handler truncated — or rebuilt from
/// somewhere other than the repeated `gts_type_id` parameters — is
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

fn feed_entry(n: u8) -> StoredUsageRecord {
    UsageRecord {
        id: Uuid::from_u128(u128::from(n)),
        gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
        tenant_id: Uuid::from_u128(2),
        resource_ref: ResourceRef::new(format!("rsc-{n}"), "compute.vm")
            .expect("valid resource ref"),
        subject_ref: None,
        metadata: BTreeMap::new(),
        quantity: qty(&n.to_string()),
        idempotency_key: IdempotencyKey::new(format!("idem-{n}")).expect("valid idempotency key"),
        accepted_at: recent_window_end(),
        origin: RecordOrigin::Live,
        invalidation: None,
        window_start: recent_window_start(),
        window_end: recent_window_end(),
    }
    // A plugin's page names its meter by reference, and the gateway
    // re-attaches the identifier from the subscription it resolved — so the
    // reference here has to be the one this suite's declaration double
    // resolves `GTS_ID` to, or every page would be refused as naming a
    // meter the subscription does not.
    .into_stored(declared_uuid(
        &MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
    ))
}

/// A `Service` over a [`HappyPathPlugin`] whose feed slot is programmed
/// with `outcome`, wired against the same permissive PDP fake
/// `domain::service_tests::read_usage_feed_tests` uses. A request that
/// never reaches the plugin still needs a `Service` to hand the handler,
/// which is why every test below builds one even when it asserts the
/// plugin was never dispatched.
fn service_with(
    outcome: Result<FeedPage<FeedPosition, StoredUsageRecord>, UsageCollectorPluginError>,
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
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            RECORDING_PLUGIN_SUFFIX,
        );
    (service, plugin)
}

fn params(extra: &[(&str, &str)]) -> Vec<(String, String)> {
    extra
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// The `field` and `description` of the first `field_violations[]` entry
/// in a canonical `Problem` response body — either or both `None` when
/// the body carries no such entry.
///
/// Read off the wire rather than off a `CanonicalError`, because a
/// handler test's subject is what a caller receives: a `400` that names
/// no parameter leaves the caller diffing their request against the
/// docs. Mirrors `usage_records_tests::first_violation_field`, extended
/// with `description` for the one test below that must discriminate
/// beyond the field name (two different guards can both land on
/// `gts_type_id`).
struct Violation {
    field: Option<String>,
    description: Option<String>,
}

async fn first_violation(response: axum::response::Response) -> Violation {
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: Value = serde_json::from_slice(&body_bytes).expect("Problem is JSON");
    let violation = body
        .get("context")
        .and_then(|c| c.get("field_violations"))
        .and_then(Value::as_array)
        .and_then(|a| a.first());
    Violation {
        field: violation
            .and_then(|v| v.get("field"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        description: violation
            .and_then(|v| v.get("description"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

/// Fails if an absent `cursor` reaches the service as anything but
/// [`FeedStart::Oldest`], if the two repeated `gts_type_id` occurrences
/// do not both reach the plugin subscription, or if the wire
/// `FeedPageDto` drops the served entry, fails to round-trip
/// `next_cursor`, carries a non-null `prev_cursor`, or omits the
/// resolved default `limit`.
///
/// The oracle for the `FeedStart` half: mutating the handler's absent-arm
/// to something other than `FeedStart::Oldest` (an early return, or a
/// silently reinterpreted default) changes `dispatch.start`, which this
/// test reads off the plugin double rather than off the response —
/// nothing about the response shape would change for a wrong `start` the
/// plugin dispatch alone can see.
#[tokio::test]
async fn a_request_naming_no_cursor_and_two_types_reaches_the_service_as_oldest() {
    let served = feed_entry(1);
    let (service, plugin) = service_with(Ok(FeedPage {
        entries: vec![served.clone()],
        next: Some(position(&[9])),
    }));

    let response = handle_read_usage_feed(
        Extension(authenticated_ctx()),
        Extension(service),
        Query(params(&[
            ("gts_type_id", GTS_ID),
            ("gts_type_id", OTHER_GTS_ID),
        ])),
    )
    .await
    .expect("a well-formed request over a permitted subscription succeeds");

    let dto = response.0;
    assert_eq!(dto.entries.len(), 1);
    assert_eq!(dto.entries[0].id, served.id);
    assert!(
        dto.page_info.prev_cursor.is_none(),
        "the feed reads forward only; `prev_cursor` MUST always be null",
    );
    assert_eq!(
        dto.page_info.limit, DEFAULT_FEED_LIMIT,
        "a request naming no `limit` must echo the resolved default onto \
         `page_info.limit`, which `FeedPage` itself carries nowhere",
    );
    let next_token = dto
        .page_info
        .next_cursor
        .expect("a live page always carries its continuation");
    let decoded = CursorV1::decode(&next_token).expect("the minted cursor must be a valid token");
    let recovered = position_from_cursor(&decoded, &subscription(), CursorField::Cursor)
        .expect("the minted continuation must decode against the same subscription");
    assert_eq!(recovered.as_bytes(), position(&[9]).as_bytes());

    let dispatch = plugin
        .last_read_feed_page_input()
        .expect("the feed page must have been dispatched");
    assert_eq!(
        dispatch.start,
        FeedStart::Oldest,
        "an absent `cursor` MUST compile to FeedStart::Oldest, not an absent position",
    );
    let dispatched_ids: Vec<MeterTypeId> = dispatch
        .types
        .iter()
        .map(|meter| meter.id.clone())
        .collect();
    assert_eq!(
        dispatched_ids,
        subscription().types(),
        "both repeated `gts_type_id` occurrences must reach the plugin subscription",
    );
}

/// Fails if a subscription over [`domain::feed::MAX_SUBSCRIPTION_TYPES`]
/// is not refused, is refused on any field but `gts_type_id`, or still
/// reaches the plugin.
#[tokio::test]
async fn over_max_subscription_types_is_refused_naming_gts_type_id_and_never_dispatched() {
    let (service, plugin) = service_with(Ok(FeedPage {
        entries: vec![],
        next: None,
    }));

    // One past the published bound, tied to the constant rather than a
    // bare literal so this fixture cannot silently drift from it.
    let too_many: Vec<(String, String)> = (0..=feed::MAX_SUBSCRIPTION_TYPES)
        .map(|i| ("gts_type_id".to_owned(), format!("t{i}")))
        .collect();

    let response = handle_read_usage_feed(
        Extension(authenticated_ctx()),
        Extension(service),
        Query(too_many),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let violation = first_violation(response).await;
    assert_eq!(
        violation.field.as_deref(),
        Some("gts_type_id"),
        "the 400 MUST blame this test's own subject",
    );
    // Not just the field: this fixture's placeholder ids are ALSO
    // shape-invalid `MeterTypeId`s, and a per-id shape guard failing on
    // the first one would report the same field ("gts_type_id") for a
    // completely different reason. Asserting the description names the
    // count bound is what ties this test to the length guard specifically
    // - deleting only that guard (leaving the per-id shape check in
    // place) would otherwise still pass a field-only assertion, the same
    // vacuous-oracle shape this slice's `CursorRejected` hazard warns
    // about, one layer over.
    let description = violation.description.unwrap_or_default();
    assert!(
        description.contains("at most")
            && description.contains(&feed::MAX_SUBSCRIPTION_TYPES.to_string()),
        "the violation must name the published count bound, not merely any \
         gts_type_id-shaped defect; got: {description:?}",
    );
    assert!(
        plugin.last_read_feed_page_input().is_none(),
        "an over-breadth subscription must be refused before the plugin is ever dispatched",
    );
}

/// Fails if a malformed `cursor` is not refused, is refused on any field
/// but `cursor`, or still reaches the plugin.
///
/// Pins the `cursor` half of ruling E7 at the REST edge: flipping this
/// handler's `feed::decode_cursor_token(t, CursorField::Cursor)` call to
/// `CursorField::Until` would make this test's violation report `until`
/// instead, and only this assertion — not the status alone — catches it.
#[tokio::test]
async fn a_malformed_cursor_is_refused_naming_cursor_and_never_dispatched() {
    let (service, plugin) = service_with(Ok(FeedPage {
        entries: vec![],
        next: None,
    }));

    let response = handle_read_usage_feed(
        Extension(authenticated_ctx()),
        Extension(service),
        Query(params(&[
            ("gts_type_id", GTS_ID),
            ("cursor", "not$valid$base64"),
        ])),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        first_violation(response).await.field.as_deref(),
        Some("cursor"),
        "a cursor defect MUST be reported on `cursor` (ruling E7)",
    );
    assert!(plugin.last_read_feed_page_input().is_none());
}

/// Fails if a malformed `until` is not refused, is refused on any field
/// but `until`, or still reaches the plugin.
///
/// Pins the `until` half of ruling E7 at the REST edge, the counterpart
/// to the `cursor` test above — this task's brief names exactly this
/// case as the minimum bar. Neither handler-layer half is implied by the
/// other: the two `decode_cursor_token` call sites can drift
/// independently, the same shape of gap Task 4's fix round found and
/// closed one layer down, in `Service::read_usage_feed` itself.
#[tokio::test]
async fn a_malformed_until_is_refused_naming_until_and_never_dispatched() {
    let (service, plugin) = service_with(Ok(FeedPage {
        entries: vec![],
        next: None,
    }));

    let response = handle_read_usage_feed(
        Extension(authenticated_ctx()),
        Extension(service),
        Query(params(&[
            ("gts_type_id", GTS_ID),
            ("until", "not$valid$base64"),
        ])),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        first_violation(response).await.field.as_deref(),
        Some("until"),
        "an until defect MUST be reported on `until`, not `cursor` (ruling E7) - \
         flipping the handler's CursorField::Until argument to CursorField::Cursor \
         is what this assertion exists to catch",
    );
    assert!(plugin.last_read_feed_page_input().is_none());
}

/// Fails if a repeated `limit` resolves last-one-wins (silently accepting
/// either `10` or `20`) instead of being refused as a caller error, or if
/// it still reaches the plugin.
///
/// The chosen behaviour, and why: `single_param` refuses a duplicate
/// optional parameter on the same terms `require_single_value` already
/// refuses one for the mandatory parameters — a caller who sent two
/// different values most likely made a mistake worth surfacing, not a
/// preference between them worth guessing at silently.
#[tokio::test]
async fn a_repeated_limit_is_refused_rather_than_resolved_last_one_wins() {
    let (service, plugin) = service_with(Ok(FeedPage {
        entries: vec![],
        next: None,
    }));

    let response = handle_read_usage_feed(
        Extension(authenticated_ctx()),
        Extension(service),
        Query(params(&[
            ("gts_type_id", GTS_ID),
            ("limit", "10"),
            ("limit", "20"),
        ])),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        first_violation(response).await.field.as_deref(),
        Some("limit"),
        "a repeated optional parameter must be refused as a caller error",
    );
    assert!(plugin.last_read_feed_page_input().is_none());
}

// ── DESIGN §3.12 integration coverage: feed pagination and replay ──────────
//
// Ruling E10: DESIGN §3.12's "end-to-end through REST and SDK against an
// in-memory plugin, including feed pagination and replay" is realized here,
// in-crate, driving the REST surface against a `Service` backed by the same
// stub plugin the rest of this file uses — the gear crate has no `tests/`
// directory (every gear test is in-crate `#[cfg(test)]`), and
// `make e2e-usage-collector` is the separate pytest suite under
// `testing/e2e/`, run against a built server binary, not a Rust target this
// suite could extend.
//
// `toolkit_odata::CursorV1` has no `PartialEq`/`Eq`, so every
// cursor comparison below goes through `domain::feed::cursors_equal` rather
// than `assert_eq!`, which would not compile on a bare `CursorV1` pair.

/// Reads page 1 (no cursor), resumes from its cursor for page 2, then
/// replays from page 1's cursor bounded by page 2's cursor and confirms the
/// replay is identical entry for entry to page 2.
///
/// Fails if: page 1's dispatch does not compile an absent cursor to
/// `FeedStart::Oldest`; page 2's dispatch does not resume from page 1's
/// plugin position; the bounded replay's dispatch does not carry page 2's
/// own position as its `until` bound; the replay's entries differ from
/// page 2's, in identity or in order; the replay still carries a next
/// cursor once its bounded read has reached that bound; or the
/// wire-decoded cursor a resume carries is not, by `cursors_equal`, the
/// cursor `domain::feed::mint_cursor` mints fresh over the same plugin
/// position and subscription.
#[tokio::test]
async fn resuming_and_replaying_the_feed_is_identical_entry_for_entry() {
    let sub = subscription();
    let e1 = feed_entry(1);
    let e2 = feed_entry(2);
    let e3 = feed_entry(3);
    let pos_a = position(&[10]);
    let pos_b = position(&[20]);

    let (service, plugin) = service_with(Ok(FeedPage {
        entries: vec![e1.clone(), e2.clone()],
        next: Some(pos_a.clone()),
    }));

    // Page 1: no cursor, begins at the oldest retained position.
    let page1 = handle_read_usage_feed(
        Extension(authenticated_ctx()),
        Extension(Arc::clone(&service)),
        Query(params(&[
            ("gts_type_id", GTS_ID),
            ("gts_type_id", OTHER_GTS_ID),
        ])),
    )
    .await
    .expect("page 1 over a permitted subscription succeeds")
    .0;
    assert_eq!(
        page1.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![e1.id, e2.id],
        "page 1 MUST carry both entries in feed order",
    );
    let dispatch1 = plugin
        .last_read_feed_page_input()
        .expect("page 1 must have been dispatched");
    assert_eq!(
        dispatch1.start,
        FeedStart::Oldest,
        "a request carrying no cursor MUST compile to FeedStart::Oldest, \
         since there is no head-start option on any surface",
    );
    let token_a = page1
        .page_info
        .next_cursor
        .clone()
        .expect("a live page always carries its continuation");
    let cursor_a = CursorV1::decode(&token_a).expect("page 1's cursor decodes");
    assert!(
        cursors_equal(&cursor_a, &mint_cursor(&pos_a, &sub)),
        "the wire-decoded cursor a resume carries MUST be the cursor \
         mint_cursor mints fresh over the same position and subscription",
    );

    // Page 2: resume from page 1's cursor.
    plugin.set_read_feed_page(FeedPage {
        entries: vec![e3.clone()],
        next: Some(pos_b.clone()),
    });
    let page2 = handle_read_usage_feed(
        Extension(authenticated_ctx()),
        Extension(Arc::clone(&service)),
        Query(params(&[
            ("gts_type_id", GTS_ID),
            ("gts_type_id", OTHER_GTS_ID),
            ("cursor", token_a.as_str()),
        ])),
    )
    .await
    .expect("page 2, resumed from page 1's cursor, succeeds")
    .0;
    assert_eq!(
        page2.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![e3.id],
        "page 2 MUST carry only what settled since page 1",
    );
    let dispatch2 = plugin
        .last_read_feed_page_input()
        .expect("page 2 must have been dispatched");
    assert_eq!(
        dispatch2.start,
        FeedStart::After(pos_a.clone()),
        "page 2 MUST resume from page 1's plugin position",
    );
    let token_b = page2
        .page_info
        .next_cursor
        .clone()
        .expect("page 2 is a live page and carries a continuation");
    let cursor_b = CursorV1::decode(&token_b).expect("page 2's cursor decodes");
    assert!(
        cursors_equal(&cursor_b, &mint_cursor(&pos_b, &sub)),
        "page 2's wire-decoded cursor MUST be the cursor mint_cursor mints \
         fresh over the same position and subscription",
    );

    // Bounded replay: start from page 1's cursor again, bounded by page
    // 2's cursor. DESIGN fixes both dispositions on the plugin (never
    // decided by the gateway): this fake is programmed to answer the
    // bounded replay with page 2's own entries and no next cursor, since
    // the read has reached its bound.
    plugin.set_read_feed_page(FeedPage {
        entries: vec![e3.clone()],
        next: None,
    });
    let replay = handle_read_usage_feed(
        Extension(authenticated_ctx()),
        Extension(Arc::clone(&service)),
        Query(params(&[
            ("gts_type_id", GTS_ID),
            ("gts_type_id", OTHER_GTS_ID),
            ("cursor", token_a.as_str()),
            ("until", token_b.as_str()),
        ])),
    )
    .await
    .expect("the bounded replay succeeds")
    .0;

    let dispatch3 = plugin
        .last_read_feed_page_input()
        .expect("the bounded replay must have been dispatched");
    assert_eq!(
        dispatch3.start,
        FeedStart::After(pos_a),
        "the replay MUST start from the same position page 2 resumed from",
    );
    assert_eq!(
        dispatch3.until,
        Some(pos_b),
        "the replay MUST carry page 2's own position as its bound",
    );
    assert_eq!(
        replay.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
        page2.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
        "a replay bounded by page 2's cursor MUST be identical entry for \
         entry to page 2 itself",
    );
    assert!(
        replay.page_info.next_cursor.is_none(),
        "a bounded replay that has reached its bound MUST carry no next cursor",
    );
}
