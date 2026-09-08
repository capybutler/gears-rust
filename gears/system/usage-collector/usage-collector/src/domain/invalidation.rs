//! The invalidation rules the gateway can decide from the target alone.
//!
//! An invalidation entry is a **faithful copy** of the entry it withdraws:
//! every caller-supplied field equals the target's, and the departures are
//! closed and are exactly three — the entry's own idempotency key, the
//! `invalidates` reference, and the `reason_code`
//! (`cpt-cf-usage-collector-adr-append-only-invalidation`).
//!
//! Of the six invalidation rules, this module decides the two a resolved
//! target answers — the target is itself a record, and the copy is faithful.
//! The other four are elsewhere, and none of them is a comparison:
//!
//! * **Explicit reference** (both-or-neither) is a rule at three boundaries
//!   on a submission path, and only one of them is a typed rejection. (A
//!   fourth applies it off those paths, when a persisted entry is
//!   rehydrated from a wire body into `usage_collector_sdk::UsageRecord`.)
//!   In-process it is structural:
//!   `usage_collector_sdk::Invalidation` groups the reference with its
//!   reason, so no in-process caller can build a half-shape. A JSON body
//!   decoded straight into [`usage_collector_sdk::CreateUsageRecord`] is
//!   refused by that type's own deserialization shadow, inside a
//!   `Deserialize` that erases everything but a message. A REST body is the
//!   third and is the only typed rejection: the served schema declares the
//!   two as flat sibling properties, so the request DTO carries them apart
//!   and the conversion into the domain type is where the pair is rejoined —
//!   see [`UsageCollectorError::invalidation_reference_incomplete`], whose
//!   own documentation states where it is raised from.
//! * **Reason code** is that same grouping: the reason is mandatory inside
//!   `usage_collector_sdk::Invalidation`, and the newtype validates itself.
//! * **Valid reference** is the lookup that produces the `target` argument.
//!   This module is pure and reads nothing, so resolving the reference — and
//!   rejecting one that resolves to nothing — belongs to its caller.
//! * **At-most-one-invalidation** belongs to the store, the only place it can
//!   be made atomic with the entry it admits. A gateway-side pre-read cannot
//!   exclude a concurrent second submission, so the gateway attempts none and
//!   lifts the plugin's `AlreadyInvalidated` instead.
//!
//! This module validates a submission against **another entry**, which is
//! why it is not more of `crate::domain::validation`: that one validates a
//! submission against a *declaration*. Different input, different failure
//! vocabulary, and the file a reviewer has to read whole to check the copy
//! rule is the copy rule alone.

use usage_collector_sdk::{
    CreateUsageRecord, EntryType, Invalidation, UsageCollectorError, UsageRecord,
};

/// The caller-supplied fields an invalidation copies, in comparison order.
///
/// The order is the diagnostic order: a submission differing in several
/// fields is rejected naming the first of these it differs in, so the
/// message is deterministic across runs rather than dependent on a hash
/// iteration order.
///
/// Spelled as literals throughout, the two covered-period bounds included:
/// this is one closed vocabulary of *request* field names, six of the eight
/// have no constant to be spelled from, and a list half in literals and half
/// in constants is the shape where one half gets repointed and the other
/// does not. The two that do have one —
/// `usage_collector_sdk::WINDOW_START_FIELD` and its sibling — are tied to
/// these entries by a test rather than by the spelling, so a wire rename
/// cannot split the vocabulary silently.
pub(crate) const COMPARED_FIELDS: [&str; 8] = [
    "tenant_id",
    "gts_type_id",
    "resource_ref",
    "subject_ref",
    "window_start",
    "window_end",
    "value",
    "metadata",
];

/// Names the first field in which `entry` departs from `target`, or `None`
/// when the copy is faithful.
///
/// Both sides are **destructured** rather than field-accessed, so adding a
/// field to either shape fails to compile here instead of silently joining
/// the uncompared set. Destructuring the *submission* rather than a second
/// projected entry is what makes that hold in both directions: a field added
/// to [`CreateUsageRecord`] alone — one a projection's struct literal would
/// carry across without comment — is a compile error here, which it would
/// not be if this compared two [`UsageRecord`]s.
///
/// The bindings follow each struct's own declaration order so a reviewer can
/// diff them against it; the comparisons below follow [`COMPARED_FIELDS`]
/// order, which is the order a rejection is reported in.
///
/// **The two ignore-counts differ and that is not a bug**: the submission
/// ignores `idempotency_key` and `invalidation` (two); the target ignores
/// `id`, `idempotency_key` and `invalidation` (three), because the target
/// has an identity the submission has not acquired yet. A destructure-audit
/// expecting one number on both sides will come out short and go looking for
/// a defect that is not there. Every ignored binding is discarded by name,
/// with the reason it is not compared.
///
/// The comparison array is as long as [`COMPARED_FIELDS`], so a comparison
/// added without its name — or removed while its name stays — is a compile
/// error rather than a field that quietly stops being compared.
fn faithful_copy_mismatch(entry: &CreateUsageRecord, target: &UsageRecord) -> Option<&'static str> {
    let CreateUsageRecord {
        gts_type_id,
        tenant_id,
        resource_ref,
        subject_ref,
        metadata,
        value,
        // Permitted departure: the entry carries its own key, distinct from
        // the target's.
        idempotency_key: _,
        // Permitted departure: the reference and its reason. One binding,
        // not two — that is the ADR's "exactly three departures" made
        // structural.
        invalidation: _,
        window_start,
        window_end,
    } = entry;

    let UsageRecord {
        // Not a copied field: the submission has no identity yet. It
        // acquires one of its own on projection, derived over its own
        // idempotency key, so it necessarily differs from the target's.
        id: _,
        gts_type_id: target_gts_type_id,
        tenant_id: target_tenant_id,
        resource_ref: target_resource_ref,
        subject_ref: target_subject_ref,
        metadata: target_metadata,
        value: target_value,
        // Permitted departure, as on the submission side.
        idempotency_key: _,
        // The target carries none — that is the
        // no-invalidation-of-an-invalidation rule, checked separately by
        // `verify_invalidation_target` and never inferred from a comparison.
        invalidation: _,
        window_start: target_window_start,
        window_end: target_window_end,
    } = target;

    // Each name sits beside the comparison that raises it, so the two cannot
    // drift into naming the wrong field.
    //
    // `subject_ref` is compared as a whole `Option`, so presence against
    // absence is a difference like any other. A comparator that walked into
    // the `Some` arms alone — the `zip(..).all(..)` shape — would pass that
    // case, because zipping a `Some` with a `None` yields nothing to
    // disagree about, and the ADR names it specifically. Both components are
    // compared, as they are for `resource_ref`: the composite is compared
    // whole rather than by its identifier.
    //
    // `metadata` is compared for equality and not for containment. A
    // withdrawal that drops one of the target's keys breaks the grouped and
    // filtered reads that surfaced the target exactly as one that adds a key
    // does, so both directions are a mismatch.
    //
    // `value` is compared for equality, never for magnitude: an invalidation
    // echoes the quantity it withdraws. Accepting a negated one would
    // re-admit the signed-compensation model this gear rejected, and a fold
    // that admitted it would double-count the measurement it was meant to
    // remove.
    //
    // That equality is `rust_decimal`'s, which is numeric rather than
    // textual: `42.500` equals `42.5` and such a withdrawal is admitted.
    // Deliberate, on three grounds. What a fold excludes is a number rather
    // than a rendering. Trailing zeros are a property of whatever produced
    // the payload, which an emitter often does not control, so rejecting
    // them would fail submissions carrying the right measurement. And the
    // rejection could not say what to change — naming the target's value is
    // exactly the oracle this comparator refuses to be, so the emitter would
    // get a `value` mismatch against a value it already echoed correctly.
    // The residue is that a withdrawal can serialize a different string than
    // the entry it withdraws; no read path compares those strings.
    //
    // The covered-period comparison is `time::OffsetDateTime`'s, which
    // likewise compares the instant and not the rendering. This runs against
    // a submission that has not been through the projection's UTC
    // normalization while the target has, so the two sides routinely carry
    // one instant under two offsets.
    let departures: [(&'static str, bool); COMPARED_FIELDS.len()] = [
        ("tenant_id", tenant_id != target_tenant_id),
        ("gts_type_id", gts_type_id != target_gts_type_id),
        ("resource_ref", resource_ref != target_resource_ref),
        ("subject_ref", subject_ref != target_subject_ref),
        ("window_start", window_start != target_window_start),
        ("window_end", window_end != target_window_end),
        ("value", value != target_value),
        ("metadata", metadata != target_metadata),
    ];

    // The names above and [`COMPARED_FIELDS`] are one vocabulary in one
    // order. A skew announces itself here instead of surfacing as a
    // rejection that names the wrong field.
    debug_assert!(
        departures
            .iter()
            .map(|&(field, _)| field)
            .eq(COMPARED_FIELDS),
        "the comparison order must match COMPARED_FIELDS",
    );

    departures
        .into_iter()
        .find_map(|(field, departed)| departed.then_some(field))
}

/// Verifies a submitted invalidation against the target the SPI returned.
///
/// `invalidation` is the withdrawal `entry` carries, taken as its own
/// argument rather than read back off the entry. That encodes the
/// precondition at the type level — this cannot be invoked for a submission
/// carrying no withdrawal at all — and it is what both rejections echo. They
/// name **`invalidation.target`, the reference the caller supplied, never
/// `target.id`**: the lookup that produced `target` is unscoped, so a
/// mis-paired row would otherwise hand the caller an identifier it never
/// sent and has no scope for. Pairing the two is the caller's obligation;
/// this function's obligation is to say nothing the caller did not already
/// know if the pairing is wrong.
///
/// Runs the two rules a resolved target answers, in the order a caller can
/// act on: the target must itself be a record, then the copy must be
/// faithful. **The order is about actionability, not about outcome for the
/// common case.** A submission that copies an invalidation faithfully has no
/// field mismatch at all — `invalidation` is discarded by both destructures,
/// so the reason code the target carries is never compared — and it is the
/// kind check alone that rejects it whichever order the two run in. The
/// order decides what a submission that breaks *both* is told: that its
/// target is not a record, which is the fault it can act on, rather than a
/// field it would be chasing while the target itself is wrong. Anyone
/// testing this ordering must use a submission that departs in a compared
/// field as well, or the test cannot fail when the two are swapped.
///
/// **A rejection names what differs, never what it differs from, and that is
/// a security rule rather than a style one.** The target is read under an
/// unrestricted filter — a same-request shape check run after the
/// *submitter's* own PDP authorization, explicitly not a caller-scoped read
/// — so the target row was never authorized to this caller. A message
/// carrying the target's value would be an oracle: submit a faithful copy
/// with one field deliberately wrong, read the real value out of the 400,
/// iterate per field, and a record the caller has no scope for is
/// reconstructed. The field *name* leaks nothing (the caller sent that
/// field, and named the target itself); the value is the only new
/// information in the message and it is exactly the part that must not
/// cross. `UsageCollectorError::invalidation_field_mismatch` is written that
/// way deliberately — do not "improve" the diagnostic by echoing the
/// target's value into it.
///
/// # Errors
///
/// * [`UsageCollectorError::InvalidArgument`] with
///   [`usage_collector_sdk::ValidationReason::InvalidationTargetNotRecord`]
///   when the target is itself an invalidation.
/// * [`UsageCollectorError::InvalidArgument`] with
///   [`usage_collector_sdk::ValidationReason::InvalidationFieldMismatch`],
///   naming the field that differs, when the copy is unfaithful.
pub(crate) fn verify_invalidation_target(
    entry: &CreateUsageRecord,
    invalidation: &Invalidation,
    target: &UsageRecord,
) -> Result<(), UsageCollectorError> {
    if target.entry_type() == EntryType::Invalidation {
        return Err(UsageCollectorError::invalidation_target_not_record(
            invalidation.target,
        ));
    }

    if let Some(field) = faithful_copy_mismatch(entry, target) {
        return Err(UsageCollectorError::invalidation_field_mismatch(
            field,
            invalidation.target,
        ));
    }

    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "invalidation_tests.rs"]
mod invalidation_tests;
