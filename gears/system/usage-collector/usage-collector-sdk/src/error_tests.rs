//! Unit tests for the [`UsageCollectorError`] constructors whose message
//! text is itself normative.
//!
//! Most constructors carry prose no document constrains, and pinning that
//! would freeze wording for its own sake. These two are different: DESIGN
//! §3.1's Target resolution row and
//! `cpt-cf-usage-collector-adr-append-only-invalidation`'s Valid target rule
//! both say what an unresolvable target's message has to tell the emitter,
//! because the target is derived rather than sent and a bare "not found"
//! points at no field they can correct.

use super::*;

/// The target identifier is derived, never sent, so the rejection has to
/// name the four inputs it was derived from.
///
/// DESIGN §3.1, Target resolution: nothing found is `NotFound`, "whose
/// message says the target is identified by tenant, GTS type, idempotency
/// key, and covered period, since a typo in any of them surfaces there
/// rather than as a field mismatch". ADR 0010's Valid target rule says the
/// same: "A reference that resolves to nothing is rejected with an
/// actionable error, which names the tenant, type, key, and covered period
/// as what identifies the target."
#[test]
fn the_unresolvable_target_rejection_names_the_four_locating_inputs() {
    let target = Uuid::from_u128(0x0BAD_1DEA);
    let err = UsageCollectorError::invalidation_target_not_found(target);
    let UsageCollectorError::NotFound {
        reason,
        name,
        detail,
        ..
    } = &err
    else {
        panic!("expected NotFound, got {err:?}");
    };
    assert_eq!(*reason, NotFoundReason::InvalidationTargetNotFound);
    assert_eq!(name, &target.to_string());

    // The claim: all four locating inputs are named. Each is asserted
    // separately so a message that drops one fails on that one rather than
    // on an opaque whole-string comparison.
    for input in ["tenant", "GTS type", "idempotency key", "covered period"] {
        assert!(
            detail.contains(input),
            "the rejection must name `{input}` as part of what identifies the target; got {detail}"
        );
    }

    // The derived identifier stays, for an operator correlating the 404
    // against a log line or against `name`.
    assert!(detail.contains(&target.to_string()), "{detail}");
}

/// The message must not send the emitter after `invalidates`.
///
/// `invalidates` is server-assigned (DESIGN §3.1, Field ownership) and
/// [`crate::CreateUsageRecord`] declares no such property, so an emitter
/// told the rejection is about `invalidates` looks for a field they had no
/// way to send. This is the half of the row a `contains` check on the four
/// inputs cannot catch: a message can name all four and still lead with the
/// wrong noun.
#[test]
fn the_unresolvable_target_rejection_names_no_field_the_emitter_never_sent() {
    let err = UsageCollectorError::invalidation_target_not_found(Uuid::from_u128(7));
    let UsageCollectorError::NotFound { detail, .. } = &err else {
        panic!("expected NotFound, got {err:?}");
    };
    assert!(
        !detail.contains("invalidates"),
        "`invalidates` is server-assigned and absent from the ingestion shape, so the \
         rejection must not present it as the caller's field; got {detail}"
    );
}
