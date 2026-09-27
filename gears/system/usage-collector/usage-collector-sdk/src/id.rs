//! Deterministic derivation of the ledger-entry identity.
//!
//! The entry `id` is not an independent field: it is a deterministic
//! projection of the dedup identity
//! `(tenant_id, gts_type_id, idempotency_key, window_start, window_end, entry_type)`
//! (`cpt-cf-usage-collector-adr-record-identity-derivation`). The
//! Ingestion Gateway derives it at one choke point for every surface, and an
//! emitter reproduces the same value offline — which is what lets a
//! correction name its target before submission, with no round-trip.
//!
//! The identity and the dedup identity read the same six inputs, so the two
//! can never disagree about what one entry is. Entry type is the last of
//! them, and it is the only one that keeps a measurement and its withdrawal
//! apart: an invalidation repeats its target's idempotency key, so without
//! it the two would share a dedup identity and the withdrawal would collide
//! with the entry it withdraws.

use time::{OffsetDateTime, UtcOffset};
use uuid::Uuid;

use crate::models::{EntryType, IdempotencyKey, MeterTypeId};

/// Fixed namespace for the entry-identity derivation (`UUIDv5`).
///
/// NEVER change this value: it admits no rotation, no per-deployment value
/// and no versioned variant, because each of those re-maps every identifier
/// the gear has ever issued.
pub const USAGE_RECORD_ID_NAMESPACE: Uuid =
    Uuid::from_u128(0x5631_3026_863b_4de8_b32b_1f96_b673_06ed);

/// ASCII unit separator between the dedup-identity fields.
///
/// The concatenation stays injective only while no input carries this byte.
/// Four inputs cannot carry it by construction: the tenant is a UUID, both
/// bounds are fixed-width timestamps, and the entry type is a closed
/// enumeration rendering one of two ASCII literals ([`EntryType::as_str`]),
/// so it has no way to spell a control character at all — a guarantee of
/// the type rather than of a validation step, and so stronger than the one
/// the remaining two get. Those two are caller-supplied, and both newtypes
/// reject every ASCII control character ([`MeterTypeId::new`],
/// [`IdempotencyKey::new`]) — which is what keeps two distinct dedup
/// identities from concatenating to one pre-image.
const FIELD_SEPARATOR: u8 = 0x1F;

/// Renders a covered-period bound in the canonical 27-character form
/// `YYYY-MM-DDTHH:MM:SS.ffffffZ`.
///
/// The form is frozen with the namespace constant: a change to the fraction
/// width, the case of `T` / `Z`, or the text encoding re-maps every
/// identifier the gear has issued. Six digits is the microsecond, and it is
/// the precision ceiling of the derivation — a caller that sends
/// `12:00:00Z`, `12:00:00.000Z` or `13:00:00+01:00` reaches one canonical
/// form here.
///
/// This function **truncates** anything below the microsecond, and callers
/// MUST NOT hand it a finer value: the ingestion path rejects one before the
/// derivation runs ([`crate::CreateUsageRecord::try_into_usage_record`]), so
/// inside the gear the two can never disagree. Truncating an unvalidated
/// bound would make a read-back entry derive an identifier different from
/// the one it carries.
///
/// That obligation is stated rather than enforceable: the precision check
/// is private to the projection named above, so an **external** caller of
/// this function has no exported way to honour it and gets silent
/// truncation. Deliberately left that way rather than narrowed, because
/// `cpt-cf-usage-collector-adr-record-identity-derivation` requires an
/// independent implementation to be able to reproduce the pre-image, and
/// that needs this rendering exported. The gap is inherited from
/// `created_at_micros`'s visibility, not introduced with the covered
/// period — and a caller reproducing an identifier is checking a value
/// that already exists, so truncation there yields a mismatch they can
/// see rather than a corrupt row.
///
/// The 27-character width holds for a year in `0..=9999`. Every bound that
/// arrives over REST is RFC 3339-parsed, and no RFC 3339 timestamp can
/// express a year outside that range, so on the wire path the width is
/// guaranteed. An in-process caller is not so constrained: `time` is
/// declared without `large-dates`, so a year down to `-9999` is
/// constructible, and `{:04}` renders `-1` as `-001` — still 27 characters,
/// but not the canonical form. The `debug_assert!` below catches that in
/// every debug and test build; a release build still renders the
/// non-canonical bound, so the assert narrows the window rather than
/// closing it.
#[must_use]
pub fn canonical_period_bound(bound: OffsetDateTime) -> String {
    let utc = bound.to_offset(UtcOffset::UTC);
    // An assert rather than a `Result`: every bound that reaches here over
    // REST was RFC 3339-parsed and so is already in range, which makes the
    // function infallible on the path that carries caller data. Returning a
    // `Result` would push a `?` into the derivation, and from there onto
    // every caller, to carry an error only a same-crate caller can trigger.
    debug_assert!(
        (0..=9999).contains(&utc.year()),
        "canonical_period_bound requires a year in 0..=9999 for the fixed \
         27-character form; got {}",
        utc.year(),
    );
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
        utc.microsecond(),
    )
}

/// Derives the entry identity from the 6-tuple dedup identity:
/// `id = UUIDv5(NS, tenant_id ⟨0x1F⟩ gts_type_id ⟨0x1F⟩ idempotency_key ⟨0x1F⟩ window_start ⟨0x1F⟩ window_end ⟨0x1F⟩ entry_type)`.
///
/// `tenant_id` enters in its lowercase hyphenated 36-character form,
/// `gts_type_id` and `idempotency_key` byte-exact (terminator `~`
/// included), both bounds in the canonical form
/// [`canonical_period_bound`] renders, and `entry_type` as its lowercase
/// ASCII wire literal ([`EntryType::as_str`]), `record` or `invalidation`.
/// `NS` enters as its 16 raw bytes, per RFC 4122.
///
/// The two bounds enter in start-then-end order, so the digest is not
/// order-blind: an order-blind one would collapse two legitimate periods
/// that mirror each other around a shared instant.
///
/// A point event derives over a zero-length period, where the two bounds
/// are equal; the derivation needs no separate case for it.
///
/// Entry type is the last input, and it is what tells a measurement from
/// its withdrawal. An invalidation copies the tenant, type, idempotency key
/// and covered period of its target, so the two differ in this input alone
/// — and the same function over the invalidation's own fields with
/// `entry_type = record` gives the identifier of the entry it withdraws,
/// which is how the gateway resolves the target. Two withdrawals of one
/// target collide on all six inputs, so every invalidation of one entry
/// derives one identifier. `reason_code` is no input here, which leaves it
/// the only field full canonical equality
/// (`cpt-cf-usage-collector-adr-mandatory-idempotency`) can still find
/// differing between two such withdrawals — and so what turns a genuine
/// second one into a loud conflict rather than a silently absorbed
/// duplicate.
#[must_use]
pub fn derive_usage_record_id(
    tenant_id: Uuid,
    gts_type_id: &MeterTypeId,
    idempotency_key: &IdempotencyKey,
    window_start: OffsetDateTime,
    window_end: OffsetDateTime,
    entry_type: EntryType,
) -> Uuid {
    let mut input = Vec::new();
    input.extend_from_slice(tenant_id.to_string().as_bytes());
    input.push(FIELD_SEPARATOR);
    input.extend_from_slice(gts_type_id.as_str().as_bytes());
    input.push(FIELD_SEPARATOR);
    input.extend_from_slice(idempotency_key.as_str().as_bytes());
    input.push(FIELD_SEPARATOR);
    input.extend_from_slice(canonical_period_bound(window_start).as_bytes());
    input.push(FIELD_SEPARATOR);
    input.extend_from_slice(canonical_period_bound(window_end).as_bytes());
    input.push(FIELD_SEPARATOR);
    input.extend_from_slice(entry_type.as_str().as_bytes());
    Uuid::new_v5(&USAGE_RECORD_ID_NAMESPACE, &input)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "id_tests.rs"]
mod id_tests;
