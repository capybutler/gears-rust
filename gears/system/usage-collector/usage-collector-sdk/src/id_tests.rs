use time::{Date, Month, OffsetDateTime, Time, UtcOffset};
use toolkit_gts::gts_id;
use uuid::Uuid;

use crate::id::{USAGE_RECORD_ID_NAMESPACE, canonical_period_bound, derive_usage_record_id};
use crate::models::{IdempotencyKey, MeterTypeId};

fn tenant() -> Uuid {
    Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap()
}
fn gts() -> MeterTypeId {
    // Must be a valid derived GTS type id: `~`-terminated, adding exactly one
    // segment to the reserved usage-record base.
    MeterTypeId::new(gts_id!(
        "cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~"
    ))
    .unwrap()
}
fn key(s: &str) -> IdempotencyKey {
    IdempotencyKey::new(s).unwrap()
}
/// `2023-11-14T22:13:20.000000Z`
fn ws() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}
/// `2023-11-14T23:13:20.000000Z`
fn we() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_003_600).unwrap()
}
fn expect(raw: &str) -> Uuid {
    Uuid::parse_str(raw).unwrap()
}

#[test]
fn derive_is_deterministic() {
    assert_eq!(
        derive_usage_record_id(tenant(), &gts(), &key("idem-1"), ws(), we()),
        derive_usage_record_id(tenant(), &gts(), &key("idem-1"), ws(), we()),
    );
}

#[test]
fn derive_matches_golden_vector() {
    // UUIDv5(NS, "11111111-1111-1111-1111-111111111111" 0x1F
    //            "gts.cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~" 0x1F
    //            "idem-1" 0x1F
    //            "2023-11-14T22:13:20.000000Z" 0x1F
    //            "2023-11-14T23:13:20.000000Z")
    //
    // Computed independently of this crate (RFC 4122 UUIDv5 over the
    // pre-image `cpt-cf-usage-collector-adr-record-identity-derivation`
    // fixes). DO NOT hand-edit: regenerate both sides and reconcile.
    assert_eq!(
        derive_usage_record_id(tenant(), &gts(), &key("idem-1"), ws(), we()),
        expect("5b075acb-e2e8-55a8-aedf-4c7d01b60284"),
    );
}

#[test]
fn derive_produces_a_v5_uuid() {
    assert_eq!(
        derive_usage_record_id(tenant(), &gts(), &key("idem-1"), ws(), we()).get_version_num(),
        5,
    );
}

#[test]
fn namespace_is_pinned() {
    // Fixed forever: a change re-maps every identifier the gear has issued.
    assert_eq!(
        USAGE_RECORD_ID_NAMESPACE,
        expect("56313026-863b-4de8-b32b-1f96b67306ed"),
    );
}

#[test]
fn distinct_keys_yield_distinct_ids() {
    assert_eq!(
        derive_usage_record_id(tenant(), &gts(), &key("idem-2"), ws(), we()),
        expect("f7295cc9-530f-5c93-8020-9f1cd7c78498"),
    );
}

#[test]
fn distinct_tenants_yield_distinct_ids() {
    let other = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
    assert_eq!(
        derive_usage_record_id(other, &gts(), &key("idem-1"), ws(), we()),
        expect("560fbb23-4eb6-508c-ab8d-4d2187acb69b"),
    );
}

#[test]
fn distinct_gts_ids_yield_distinct_ids() {
    let other = MeterTypeId::new(gts_id!(
        "cf.core.uc.usage_record.v1~cf.mini_chat._.messages_sent.v1~"
    ))
    .unwrap();
    assert_eq!(
        derive_usage_record_id(tenant(), &other, &key("idem-1"), ws(), we()),
        expect("86f9359f-2d26-5fb3-b88a-d256910c6462"),
    );
}

#[test]
fn the_tenant_enters_in_its_lowercase_hyphenated_form() {
    // The base golden vector's tenant is all decimal digits, so it cannot
    // tell a lowercase rendering apart from an uppercase one. This vector
    // carries hex letters, and it is the only thing standing between the
    // pre-image and a `to_string().to_uppercase()` slip. The second
    // assertion is the caller-facing half of the same rule: `Uuid` parses
    // either spelling, and both must reach one pre-image.
    let lower = Uuid::parse_str("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee").unwrap();
    let upper = Uuid::parse_str("AAAAAAAA-BBBB-4CCC-8DDD-EEEEEEEEEEEE").unwrap();
    assert_eq!(
        derive_usage_record_id(lower, &gts(), &key("idem-1"), ws(), we()),
        expect("138f9b89-bffa-5550-ab27-98c5b518801a"),
    );
    assert_eq!(
        derive_usage_record_id(upper, &gts(), &key("idem-1"), ws(), we()),
        expect("138f9b89-bffa-5550-ab27-98c5b518801a"),
    );
}

#[test]
fn a_different_covered_period_yields_a_different_id() {
    // The point of the 5-tuple: one stable per-meter idempotency key covers
    // many periods, and two periods are two entries rather than a collision.
    let one_micro_later = ws() + time::Duration::microseconds(1);
    assert_eq!(
        derive_usage_record_id(tenant(), &gts(), &key("idem-1"), one_micro_later, we()),
        expect("bf2e9ea3-746e-5521-8145-a99c37b54924"),
    );
    assert_ne!(
        derive_usage_record_id(tenant(), &gts(), &key("idem-1"), ws(), we()),
        derive_usage_record_id(
            tenant(),
            &gts(),
            &key("idem-1"),
            ws(),
            we() + time::Duration::microseconds(1)
        ),
        "window_end is an input too",
    );
}

#[test]
fn the_two_bounds_enter_in_a_fixed_order() {
    // A concatenation that treated the bounds symmetrically would derive one
    // id for [22:13, 23:13) and [23:13, 22:13). The second is rejected
    // upstream, but the derivation must not be order-blind: an order-blind
    // digest would also collapse two legitimate periods that happen to
    // mirror each other around a shared instant.
    assert_eq!(
        derive_usage_record_id(tenant(), &gts(), &key("idem-1"), we(), ws()),
        expect("6a75ef43-2619-58a0-b75c-53b08bd96a78"),
    );
}

#[test]
fn a_point_event_derives_over_equal_bounds() {
    // A confirmation of
    // `cpt-cf-usage-collector-adr-record-identity-derivation`: a point event
    // is a zero-length period, and the derivation needs no separate case for
    // it.
    assert_eq!(
        derive_usage_record_id(tenant(), &gts(), &key("idem-1"), ws(), ws()),
        expect("64902762-6487-570c-83c3-8975a2e1adb4"),
    );
}

#[test]
fn equivalent_spellings_of_one_instant_derive_one_id() {
    // A confirmation of
    // `cpt-cf-usage-collector-adr-record-identity-derivation`: the canonical
    // form collapses a non-UTC offset and a fraction of zero / three / six
    // digits onto one pre-image. The
    // bounds are parsed from the wire spellings an emitter would actually
    // send, rather than built from components, because that is the path the
    // equivalence has to hold across. Every pairing must derive the base
    // vector. (UUID case is pinned separately by
    // `the_tenant_enters_in_its_lowercase_hyphenated_form`, which needs a
    // letter-bearing tenant the base vector does not have.)
    let base = expect("5b075acb-e2e8-55a8-aedf-4c7d01b60284");
    let parse = |raw: &str| {
        OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339).unwrap()
    };
    let derive_over = |start: &str, end: &str| {
        derive_usage_record_id(tenant(), &gts(), &key("idem-1"), parse(start), parse(end))
    };

    assert_eq!(
        derive_over("2023-11-14T22:13:20Z", "2023-11-14T23:13:20Z"),
        base,
        "an absent fraction pads to six digits",
    );
    assert_eq!(
        derive_over("2023-11-14T22:13:20.000Z", "2023-11-14T23:13:20.000Z"),
        base,
        "a three-digit fraction pads to six digits",
    );
    assert_eq!(
        derive_over("2023-11-14T22:13:20.000000Z", "2023-11-14T23:13:20.000000Z"),
        base,
        "a six-digit fraction is already canonical",
    );
    assert_eq!(
        derive_over("2023-11-15T03:43:20+05:30", "2023-11-15T04:43:20+05:30"),
        base,
        "a non-UTC offset normalizes before the bound is formatted",
    );
    assert_eq!(
        derive_over("2023-11-14T17:13:20-05:00", "2023-11-14T18:13:20-05:00"),
        base,
        "a negative offset normalizes the same way",
    );
}

// ── canonical_period_bound: the 27-character form ───────────────────────────

#[test]
fn canonical_period_bound_is_twenty_seven_characters() {
    let rendered = canonical_period_bound(ws());
    assert_eq!(rendered, "2023-11-14T22:13:20.000000Z");
    assert_eq!(rendered.len(), 27);
}

#[test]
fn canonical_period_bound_always_carries_six_fraction_digits() {
    let with_micros = ws() + time::Duration::microseconds(1);
    assert_eq!(
        canonical_period_bound(with_micros),
        "2023-11-14T22:13:20.000001Z"
    );
    assert_eq!(
        canonical_period_bound(ws() + time::Duration::milliseconds(500)),
        "2023-11-14T22:13:20.500000Z"
    );
}

#[test]
fn canonical_period_bound_zero_pads_every_component() {
    // The base vector's instant has no single-digit component and a
    // four-digit year, so it cannot tell `{:02}` from `{}` on the month,
    // day, hour, minute or second, nor `{:04}` from `{}` on the year. This
    // instant has a single-digit value in every one of those positions —
    // dropping any pad shortens the form below 27 characters and re-maps
    // every identifier derived over it.
    let padded = OffsetDateTime::new_utc(
        Date::from_calendar_date(999, Month::January, 2).unwrap(),
        Time::from_hms_micro(3, 4, 5, 6).unwrap(),
    );
    let rendered = canonical_period_bound(padded);
    assert_eq!(rendered, "0999-01-02T03:04:05.000006Z");
    assert_eq!(rendered.len(), 27);
}

#[test]
fn canonical_period_bound_normalizes_a_non_utc_offset() {
    let plus_one = UtcOffset::from_hms(1, 0, 0).unwrap();
    assert_eq!(
        canonical_period_bound(ws().to_offset(plus_one)),
        canonical_period_bound(ws()),
    );
}

#[test]
fn canonical_period_bound_truncates_below_the_microsecond() {
    // The documented contract of the helper, and the reason its callers
    // must validate first: it drops sub-microsecond nanos rather than
    // rejecting them, so two instants a nanosecond apart render one bound.
    // `CreateUsageRecord::try_into_usage_record` is what makes that
    // unreachable on the ingestion path.
    let with_nanos = ws().replace_nanosecond(1_500).unwrap();
    assert_eq!(
        canonical_period_bound(with_nanos),
        "2023-11-14T22:13:20.000001Z"
    );
}
