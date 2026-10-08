//! Where a withdrawal's target comes from, and the one rule the resolved
//! target answers.
//!
//! An invalidation entry is a **faithful copy** of the entry it withdraws:
//! every caller-supplied field equals the target's, and the departures are
//! closed — `entry_type` and `reason_code` (DESIGN §3.1, "Faithful copy";
//! `cpt-cf-usage-collector-adr-append-only-invalidation`). The idempotency key
//! is copied like any other field, not derived: no prefix is reserved, and
//! repeating the target's key is precisely what lets the target be found.
//!
//! This module owns the gateway's invalidation rules that are comparisons —
//! deriving the target's identifier from the submission, and checking that the
//! resolved row was copied faithfully. The rest are elsewhere, and none of
//! them is a comparison:
//!
//! * **Entry type and reason code** are checked against each other inside
//!   the SDK projection, on every submission path at once. `entry_type` is
//!   caller-supplied and required; `reason_code` is required with it and
//!   forbidden without it. Neither half is inferred from the other, so
//!   nothing here re-derives a kind.
//! * **Valid target** is the lookup that produces the `target` argument to
//!   `verify_invalidation_target`, keyed by the identifier
//!   `derive_invalidation_target` returns. (Named, not linked: both are
//!   `pub(crate)` and this module doc is public, so a link from here would be
//!   a `rustdoc::private_intra_doc_links` warning. The private items below
//!   link them freely, which rustdoc does not lint.) This module is pure and
//!   reads nothing, so performing that lookup — and rejecting a target that
//!   resolves to nothing — belongs to its caller.
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
//! This module validates a submission against **another entry**, which is why
//! it is not more of `crate::domain::validation`: that one validates a
//! submission against a *declaration*. Different input, different failure
//! vocabulary.

use usage_collector_sdk::{
    CreateUsageRecord, EntryType, Invalidation, StoredUsageRecord, UsageCollectorError,
    derive_usage_record_id,
};
use uuid::Uuid;

/// The identifier of the entry a withdrawal withdraws, derived from the
/// withdrawal's own fields.
///
/// DESIGN §3.1, "Target resolution": an invalidation names its target through
/// its own identity inputs rather than by reference. The target's `id` is
/// [`derive_usage_record_id`] over this submission's own inputs with
/// [`EntryType::Record`] — the withdrawal's own identity differs in the entry
/// type alone. A faithful copy repeats the rest, so the submission by itself
/// says what it withdraws, and a caller-supplied target — which would let an
/// emitter name a record it never measured — is neither needed nor accepted.
///
/// What it returns is an identifier, not a target: the entry may not exist.
/// The caller looks it up converged-only under the PDP permit scope and
/// rejects a lookup that finds nothing. Every input feeds the derivation, so a
/// typo in any one resolves to an identifier the ledger does not hold and
/// surfaces as a missing target rather than as a field mismatch.
///
/// **It runs before the submission's covered period has been validated, and
/// that is sound rather than a gap in
/// `cpt-cf-usage-collector-adr-record-identity-derivation`.**
/// [`usage_collector_sdk::canonical_period_bound`] truncates below the
/// microsecond, so a finer bound would be truncated here instead of rejected.
/// The projection that follows rejects such a bound before it stamps anything,
/// and the caller discards this value along with the submission: no identifier
/// derived from an unvalidated period reaches a lookup, a response, a metric
/// label, or the store. The ADR requires that no *entry* acquire an identity
/// over an unvalidated period, and none does.
///
/// **One case is not merely discarded, and it is worth knowing before this
/// order is copied elsewhere.** `canonical_period_bound` also carries a
/// `debug_assert!` on the year being in `0..=9999`, and a submission whose
/// period is both out of that range and finer than the microsecond reaches
/// that assert here rather than the projection's precision check — so a debug
/// build panics the request thread. Unreachable over REST (RFC 3339 cannot
/// express such a year) and in a release build; reachable only by an
/// in-process caller constructing the bound directly.
///
/// # Errors
///
/// [`UsageCollectorError::missing_idempotency_key`] when the submission
/// carries no key: the key is one of the identity inputs, so a submission
/// without one names nothing, and both SDK projections refuse it on the same
/// grounds.
///
/// **This is where `cpt-cf-usage-collector-dod-no-invalidation-of-invalidation`
/// is realized**, by the [`EntryType::Record`] argument below rather than by a
/// check: that definition of done requires the property to hold by
/// construction and forbids "a separate check, a candidate-kind filter, or a
/// cascade of any depth". Its second half — a plugin answering such an
/// identifier with an invalidation row is a contract breach and surfaces as an
/// internal failure, never as a caller error — is enforced at the lookup site
/// in `crate::domain::service` (`resolve_invalidation_targets`).
// @cpt-algo:cpt-cf-usage-collector-algo-entry-identity-derivation:p1
// @cpt-dod:cpt-cf-usage-collector-dod-no-invalidation-of-invalidation:p1
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
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
/// Spelled as literals throughout, the covered-period bounds included: this is
/// one closed vocabulary of *request* field names, most of which have no
/// constant to be spelled from, and a list half in literals and half in
/// constants is the shape where one half gets repointed and the other does
/// not. The two that do have constants (`usage_collector_sdk::WINDOW_START_FIELD`
/// and its sibling) are tied to these entries by a test rather than by the
/// spelling, so a wire rename cannot split the vocabulary silently.
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
/// order, which is the order a rejection is reported in. The two sides ignore
/// different sets — the target carries server-assigned fields the submission
/// never supplies and no discriminator field at all — so a destructure-audit
/// expecting symmetry will go looking for a defect that is not there. Every
/// ignored binding is discarded by name, with the reason it is not compared.
///
/// The comparison array is as long as [`COMPARED_FIELDS`], so a comparison
/// added without its name — or removed while its name stays — is a compile
/// error rather than a field that quietly stops being compared.
///
/// **Some of the compared fields cannot differ, and are compared anyway.**
/// `tenant_id`, the meter and both covered-period bounds are inputs to the
/// identifier [`derive_invalidation_target`] resolved the target by, so a row
/// answering that identifier already agrees on them — DESIGN §3.1's "Faithful
/// copy" row says so, and routes a typo in any of them to a missing target
/// instead. They stay in the comparison because the destructure is what makes
/// a field added to [`CreateUsageRecord`] a compile error here.
fn faithful_copy_mismatch(
    entry: &CreateUsageRecord,
    submission_type_uuid: Uuid,
    target: &StoredUsageRecord,
) -> Option<&'static str> {
    let CreateUsageRecord {
        // A permitted departure: the declared kind differs from the target's
        // by definition — a withdrawal is an `invalidation` and its target a
        // `record` — so comparing it would reject every faithful copy.
        entry_type: _,
        // Not compared from here: the target names its meter by reference and
        // this side has only the identifier, so the reference arrives as
        // `submission_type_uuid`. Still bound rather than dropped from the
        // pattern, so a field added to `CreateUsageRecord` keeps failing to
        // compile here until it is classified.
        gts_type_id: _,
        tenant_id,
        resource_ref,
        subject_ref,
        metadata,
        quantity,
        // Not compared, and not because it may differ: it is one of the inputs
        // the target's identifier was derived from, so the row this runs
        // against already carries this key. See `derive_invalidation_target`.
        idempotency_key: _,
        // The other permitted departure: the reason code. On the submission
        // shape it is the reason alone — the reference is server-assigned.
        invalidation: _,
        window_start,
        window_end,
    } = entry;

    let StoredUsageRecord {
        // Not a copied field: the submission has no identity yet. It
        // acquires one of its own on projection, derived over its own
        // idempotency key, so it necessarily differs from the target's.
        id: _,
        gts_type_uuid: target_gts_type_uuid,
        tenant_id: target_tenant_id,
        resource_ref: target_resource_ref,
        subject_ref: target_subject_ref,
        metadata: target_metadata,
        quantity: target_quantity,
        // Not compared, for the reason the submission's binding gives: the
        // withdrawal repeats this key, and the repetition is what located
        // this row.
        idempotency_key: _,
        // Not a copied field: the gateway's own per-request stamp, with
        // nothing on the submission side to compare it against.
        accepted_at: _,
        // Not a copied field: stamped from the route each entry arrived on,
        // and the withdrawal's route is its own. A correction of a period that
        // has since closed travels the backfill route while the entry it
        // retracts came in live, so comparing the two would reject exactly the
        // case the backfill route exists for. `CreateUsageRecord` carries no
        // origin to compare against either.
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
    // absence is a difference like any other — a comparator walking into the
    // `Some` arms alone (the `zip(..).all(..)` shape) would pass that case,
    // which the ADR names specifically. `resource_ref` is likewise compared
    // whole rather than by its identifier.
    //
    // `metadata` is compared for equality, not containment: a withdrawal that
    // drops one of the target's keys breaks the grouped and filtered reads
    // that surfaced the target exactly as one that adds a key does.
    //
    // `quantity` is compared for equality, never for magnitude: an
    // invalidation echoes the quantity it withdraws, and accepting a negated
    // one would re-admit the signed-compensation model this gear rejected.
    // That equality is `UsageQuantity`'s, which is textual, so `42.500` does
    // not copy `42.5`. The rejection names `quantity` and never the target's
    // value, for the oracle reason `verify_invalidation_target` gives.
    //
    // The covered-period comparison is `time::OffsetDateTime`'s, comparing the
    // instant and not the rendering — this runs against a submission that has
    // not been through the projection's UTC normalization while the target
    // has, so the two sides routinely carry one instant under two offsets.
    let departures: [(&'static str, bool); COMPARED_FIELDS.len()] = [
        ("tenant_id", tenant_id != target_tenant_id),
        // The caller-facing field name is the identifier's, because that is
        // what the submitter sent; the comparison is on the reference,
        // because that is what the two sides have in common.
        ("gts_type_id", submission_type_uuid != *target_gts_type_uuid),
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
/// One rule only: the copy must be faithful. Its sibling —
/// no-invalidation-of-an-invalidation — needs nothing here, the target's
/// identifier being derived with `entry_type = record`
/// ([`derive_invalidation_target`]) so a row answering it is a measurement by
/// construction. A check here would report a corrupt store as a caller fault,
/// on a field the caller does not supply.
///
/// `submission_type_uuid` is the registry reference of the meter the
/// submission named, threaded in from the `ResolvedDeclaration` the caller
/// already holds. Resolving it inside this module would put a
/// `types-registry` dependency on the ingestion path, which spec §3.3.3 exists
/// to avoid — and the target names its meter by reference alone, so there is
/// nothing on it to compare an identifier against.
///
/// `invalidation` is the withdrawal the **projected entry** carries, taken as
/// its own argument rather than read off `entry`: the submission carries the
/// reason code alone. Its `target` is what the rejection echoes — the
/// identifier the gateway asked the store for, rather than the `id` the store
/// answered with. The caller has already refused a row whose `id` differs, so
/// echoing the asked-for one keeps the two agreeing independently of that
/// check.
///
/// **A rejection names what differs, never what it differs from.** The field
/// *name* leaks nothing — the caller sent that field — and the identifier
/// beside it is one the caller can derive from its own submission. The
/// target's *value* for the differing field is the part that must not cross.
/// The target is read under the scope compiled from the submitter's own
/// `create` permit, so a row outside that grant answers as absent and never
/// reaches this function; the rule is kept as defence in depth, because a
/// message carrying the target's value would be an oracle the moment a plugin
/// answered outside the scope it was handed. Do not "improve"
/// `UsageCollectorError::invalidation_field_mismatch` by echoing the target's
/// value into it.
///
/// # Errors
///
/// [`UsageCollectorError::InvalidArgument`] with
/// [`usage_collector_sdk::ValidationReason::InvalidationFieldMismatch`],
/// naming the field that differs, when the copy is unfaithful.
///
/// **Traceability.** This function and [`faithful_copy_mismatch`] are the
/// whole of `cpt-cf-usage-collector-algo-faithful-copy-validation` /
/// `cpt-cf-usage-collector-dod-faithful-copy-validation`: the caller-supplied
/// fields DESIGN §3.1's "Faithful copy" row names — `resource_ref`,
/// `subject_ref` (presence against absence included), `quantity` (exact
/// decimal, sign included, no scaling or rounding) and `metadata` (whole key
/// set and every value) — are compared against the resolved target, a
/// difference is one rejection naming one field, and no server-assigned value
/// takes part. `cpt-cf-usage-collector-flow-replace-mismeasured-quantity` is
/// marked here because its error scenarios are this comparison's: an entry
/// carrying the *corrected* quantity and one carrying the *negated* quantity
/// are each rejected naming `quantity`, which is what forces a correction to
/// be a withdrawal plus a fresh emission rather than a signed adjustment.
///
/// **One documented departure from the definition's letter.** It says "The
/// five identifying fields **MUST NOT** be compared"; [`COMPARED_FIELDS`]
/// skips `idempotency_key` and compares the rest. That changes no outcome a
/// conformant store can produce — each is an input to the identifier
/// [`derive_invalidation_target`] resolved the target by, and the caller has
/// already refused a row whose `id` differs — and it is what makes a field
/// added to [`CreateUsageRecord`] a compile error here. DESIGN §3.1 states
/// those fields "cannot mismatch" rather than forbidding the comparison. The
/// one case where it is visible is a plugin that answers an `id` with a row
/// whose own fields do not derive it: that caller sees a `400` naming the
/// field instead of the `500` the sibling store-breach guards in
/// `crate::domain::service` raise.
// @cpt-algo:cpt-cf-usage-collector-algo-faithful-copy-validation:p1
// @cpt-dod:cpt-cf-usage-collector-dod-faithful-copy-validation:p1
// @cpt-flow:cpt-cf-usage-collector-flow-replace-mismeasured-quantity:p1
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub(crate) fn verify_invalidation_target(
    entry: &CreateUsageRecord,
    submission_type_uuid: Uuid,
    invalidation: &Invalidation,
    target: &StoredUsageRecord,
) -> Result<(), UsageCollectorError> {
    if let Some(field) = faithful_copy_mismatch(entry, submission_type_uuid, target) {
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
