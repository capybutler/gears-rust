//! Where a withdrawal's target comes from, and the one rule the resolved
//! target answers.
//!
//! An invalidation entry is a **faithful copy** of the entry it withdraws:
//! every caller-supplied field equals the target's, and the departures are
//! closed and are exactly two — `entry_type` and `reason_code` (DESIGN §3.1,
//! "Faithful copy"; `cpt-cf-usage-collector-adr-append-only-invalidation`).
//! The idempotency key is copied like any other field, not derived: no
//! prefix is reserved, and repeating the target's key is precisely what lets
//! the target be found.
//!
//! This module owns two of the gateway's invalidation rules — deriving the
//! target's identifier from the submission, and checking that the resolved
//! row was copied faithfully. The rest are elsewhere, and none of them is a
//! comparison:
//!
//! * **Entry type and reason code** are checked against each other inside
//!   the SDK projection, on every submission path at once. `entry_type` is
//!   caller-supplied and required; `reason_code` is required with it and
//!   forbidden without it. Neither half is inferred from the other, so
//!   nothing here re-derives a kind.
//! * **Valid target** is the lookup that produces the `target` argument to
//!   `verify_invalidation_target`, keyed by the identifier
//!   `derive_invalidation_target` returns. (Both are crate-private, so this
//!   module doc names them rather than linking them: an intra-doc link to a
//!   private item resolves nowhere outside the build that defines it.) This
//!   module is pure and reads nothing, so performing that lookup — and
//!   rejecting a target that resolves to nothing — belongs to its caller.
//! * **No invalidation of an invalidation** is no check at all. The target's
//!   identifier is derived with `entry_type = record`, so no identifier a
//!   withdrawal resolves can belong to an invalidation: DESIGN §3.1 states
//!   the property holds by construction and needs no check of its own.
//! * **At-most-one-invalidation** is likewise no check. Every withdrawal of
//!   one record repeats that record's tenant, type, key and period and reads
//!   `entry_type = invalidation`, so all of them reach one dedup identity
//!   and a second one collides; the gateway lifts that conflict as
//!   `AlreadyInvalidated` at dispatch (`error::lift_dispatch_error`).
//!
//! This module validates a submission against **another entry**, which is
//! why it is not more of `crate::domain::validation`: that one validates a
//! submission against a *declaration*. Different input, different failure
//! vocabulary, and the file a reviewer has to read whole to check the copy
//! rule is the copy rule alone.

use usage_collector_sdk::{
    CreateUsageRecord, EntryType, Invalidation, UsageCollectorError, UsageRecord,
    derive_usage_record_id,
};
use uuid::Uuid;

/// The identifier of the entry a withdrawal withdraws, derived from the
/// withdrawal's own fields.
///
/// DESIGN §3.1, "Target resolution": an invalidation names its target
/// through its own identity inputs rather than by reference. The target's
/// `id` is [`derive_usage_record_id`] over this submission's `(tenant_id,
/// gts_type_id, idempotency_key, window_start, window_end)` with
/// [`EntryType::Record`] — the same five inputs the withdrawal's own
/// identity rests on, differing in the sixth alone. A faithful copy repeats
/// all five, so the submission by itself says what it withdraws, and a
/// caller-supplied target — which would let an emitter name a record it
/// never measured — is neither needed nor accepted.
///
/// What it returns is an identifier, not a target: the entry may not exist.
/// The caller looks it up converged-only under the PDP permit scope and
/// rejects a lookup that finds nothing. Because all five inputs feed the
/// derivation, a typo in any one of them resolves to an identifier the
/// ledger does not hold, and so surfaces as a missing target rather than as
/// a field mismatch.
///
/// **It runs before the submission's covered period has been validated, and
/// that is sound rather than a gap in
/// `cpt-cf-usage-collector-adr-record-identity-derivation`.**
/// [`usage_collector_sdk::canonical_period_bound`] truncates below the
/// microsecond, so a bound finer than that would be truncated here instead
/// of rejected. The projection that follows rejects such a bound before it
/// stamps anything, and the caller discards this value along with the
/// submission: no identifier derived from an unvalidated period reaches a
/// lookup, a response, or the store. The ADR's requirement is that no
/// *entry* acquire an identity over an unvalidated period, and no entry
/// does.
///
/// # Errors
///
/// [`UsageCollectorError::missing_idempotency_key`] when the submission
/// carries no key. The key is one of the five inputs, so a submission
/// without one names nothing; both SDK projections refuse the same
/// submission on the same grounds.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is 144 bytes because Conflict carries invalidated_by/reason_code (SPEC-DIFF 2.2); callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub(crate) fn derive_invalidation_target(
    entry: &CreateUsageRecord,
) -> Result<Uuid, UsageCollectorError> {
    let Some(idempotency_key) = entry.idempotency_key.as_ref() else {
        return Err(UsageCollectorError::missing_idempotency_key());
    };
    Ok(derive_usage_record_id(
        entry.tenant_id,
        &entry.gts_type_id,
        idempotency_key,
        entry.window_start,
        entry.window_end,
        EntryType::Record,
    ))
}

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
    "quantity",
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
/// ignores `entry_type`, `idempotency_key` and `invalidation` (three); the
/// target ignores `id`, `idempotency_key`, `accepted_at`, `origin` and
/// `invalidation` (five), because the target carries three server-assigned
/// fields the submission never supplies and no discriminator field at all.
/// A destructure-audit expecting one number on both sides will come out
/// short and go looking for a defect that is not there. Every ignored
/// binding is discarded by name, with the reason it is not compared.
///
/// The comparison array is as long as [`COMPARED_FIELDS`], so a comparison
/// added without its name — or removed while its name stays — is a compile
/// error rather than a field that quietly stops being compared.
///
/// **Four of the eight cannot differ, and are compared anyway.** `tenant_id`,
/// `gts_type_id` and both covered-period bounds are inputs to the identifier
/// [`derive_invalidation_target`] resolved the target by, so a row answering
/// that identifier already agrees on them — DESIGN §3.1's "Faithful copy"
/// row says exactly that, and routes a typo in any of them to a missing
/// target instead. They stay in the comparison because the destructure is
/// what makes a field added to [`CreateUsageRecord`] a compile error here,
/// and dropping four comparisons to save nothing measurable would trade that
/// away.
fn faithful_copy_mismatch(entry: &CreateUsageRecord, target: &UsageRecord) -> Option<&'static str> {
    let CreateUsageRecord {
        // Permitted departure, and the first of exactly two: the declared
        // kind. It differs from the target's by definition — a withdrawal is
        // an `invalidation` and its target is a `record` — so comparing it
        // would reject every faithful copy there is.
        entry_type: _,
        gts_type_id,
        tenant_id,
        resource_ref,
        subject_ref,
        metadata,
        quantity,
        // Not compared, and not because it may differ: it is one of the five
        // inputs the target's identifier was derived from, so the row this
        // runs against already carries this very key. See
        // `derive_invalidation_target`.
        idempotency_key: _,
        // Permitted departure, the second: the reason code. On the
        // submission shape it is the reason alone — the reference is
        // server-assigned and no field here carries it.
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
        quantity: target_quantity,
        // Not compared, for the reason the submission's binding gives: the
        // withdrawal repeats this key, and the repetition is what located
        // this row.
        idempotency_key: _,
        // Not a copied field: it is the gateway's own per-request stamp, not
        // an echo of anything on the target. There is nothing on the
        // submission side to compare it against either —
        // `CreateUsageRecord` carries no `accepted_at`.
        accepted_at: _,
        // Not a copied field: it is stamped from the route each entry
        // arrived on, and the withdrawal's route is its own. A correction
        // of a period that has since closed travels the backfill route
        // while the entry it retracts came in live, so comparing the two
        // would reject exactly the case the backfill route exists for.
        // There is nothing on the submission side to compare it against
        // either — `CreateUsageRecord` carries no origin.
        origin: _,
        // The target carries none, and that is not a rule enforced here or
        // anywhere: its identifier was derived with `entry_type = record`,
        // so a row answering it is a measurement by construction (DESIGN
        // §3.1, "No invalidation of an invalidation").
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
    // `quantity` is compared for equality, never for magnitude: an
    // invalidation echoes the quantity it withdraws. Accepting a negated one
    // would re-admit the signed-compensation model this gear rejected, and a
    // fold that admitted it would double-count the measurement it was meant
    // to remove.
    //
    // That equality is `UsageQuantity`'s, which is textual: the copy repeats
    // the quantity digit for digit (SPEC-DIFF decision S-B7), so `42.500`
    // does not copy `42.5`. The rejection names `quantity` and never the
    // target's value, for the oracle reason `verify_invalidation_target`
    // gives.
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
        ("quantity", quantity != target_quantity),
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
/// One rule, not two: the copy must be faithful. Its sibling —
/// no-invalidation-of-an-invalidation — used to be checked here against
/// `target.entry_type()` and is gone, because there is no longer anything
/// for it to catch. The target's identifier is derived with
/// `entry_type = record` ([`derive_invalidation_target`]), so a row
/// answering that identifier is a measurement by construction; DESIGN §3.1
/// states the property holds that way and "needs no check of its own". Kept,
/// it would have reported a corrupt store as a caller fault, on a field the
/// caller no longer supplies.
///
/// `invalidation` is the withdrawal the **projected entry** carries, taken
/// as its own argument rather than read back off `entry`: the submission
/// carries the reason code alone, and the target half was resolved by the
/// gateway. Its `target` is what the rejection echoes — the identifier the
/// gateway asked the store for, rather than the `id` the store answered
/// with. The caller has already refused a row whose `id` differs, so the two
/// agree here; echoing the asked-for one keeps that true independently of
/// that check.
///
/// **A rejection names what differs, never what it differs from.** The field
/// *name* leaks nothing — the caller sent that field — and the identifier
/// beside it is one the caller can derive from its own submission. The
/// target's *value* for the differing field is the part that must not cross.
/// The target is read under the scope compiled from the submitter's own
/// `create` permit (SPEC-DIFF decision S-A1), so a row outside that grant
/// answers as absent and never reaches this function; the rule is kept as
/// defence in depth, because a message carrying the target's value would be
/// an oracle the moment a plugin answered outside the scope it was handed —
/// submit a faithful copy with one field wrong, read the real value out of
/// the 400, iterate. `UsageCollectorError::invalidation_field_mismatch` is
/// written that way deliberately — do not "improve" the diagnostic by
/// echoing the target's value into it.
///
/// # Errors
///
/// [`UsageCollectorError::InvalidArgument`] with
/// [`usage_collector_sdk::ValidationReason::InvalidationFieldMismatch`],
/// naming the field that differs, when the copy is unfaithful.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is 144 bytes because Conflict carries invalidated_by/reason_code (SPEC-DIFF 2.2); callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub(crate) fn verify_invalidation_target(
    entry: &CreateUsageRecord,
    invalidation: &Invalidation,
    target: &UsageRecord,
) -> Result<(), UsageCollectorError> {
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
