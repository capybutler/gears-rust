//! Wire round-trip unit tests for the typed reason enums.

use super::*;

#[test]
fn validation_reason_round_trips_each_constant() {
    for (wire, expected) in [
        (SEMANTICS_VIOLATION, ValidationReason::SemanticsViolation),
        (VALIDATION, ValidationReason::Validation),
        (METADATA_VALIDATION, ValidationReason::MetadataValidation),
        (UNKNOWN_METADATA_KEY, ValidationReason::UnknownMetadataKey),
        (INVALID_BASE_GTS_ID, ValidationReason::InvalidBaseGtsId),
        (
            INVALID_METADATA_FIELDS_EMPTY_STRING,
            ValidationReason::MetadataFieldEmptyString,
        ),
        (
            INVALID_METADATA_FIELDS_INVALID_KEY,
            ValidationReason::MetadataFieldInvalidKey,
        ),
        (
            INVALID_METADATA_FIELDS_DUPLICATE,
            ValidationReason::MetadataFieldDuplicate,
        ),
        (
            AGGREGATION_RESULT_TOO_LARGE,
            ValidationReason::AggregationResultTooLarge,
        ),
        (INVALID_CURSOR, ValidationReason::InvalidCursor),
        (FILTER_MISMATCH, ValidationReason::FilterMismatch),
        (
            INVALIDATION_REFERENCE_INCOMPLETE,
            ValidationReason::InvalidationReferenceIncomplete,
        ),
        (
            INVALIDATION_TARGET_NOT_RECORD,
            ValidationReason::InvalidationTargetNotRecord,
        ),
        (
            INVALIDATION_FIELD_MISMATCH,
            ValidationReason::InvalidationFieldMismatch,
        ),
        (FUTURE_WINDOW, ValidationReason::FutureWindow),
        (PAST_WINDOW, ValidationReason::PastWindow),
    ] {
        assert_eq!(ValidationReason::from_wire(wire), expected);
        assert_eq!(expected.as_wire(), wire);
    }
}

#[test]
fn conflict_reason_round_trips_each_constant() {
    for (wire, expected) in [
        (IDEMPOTENCY_CONFLICT, ConflictReason::IdempotencyConflict),
        (ALREADY_INVALIDATED, ConflictReason::AlreadyInvalidated),
    ] {
        assert_eq!(ConflictReason::from_wire(wire), expected);
        assert_eq!(expected.as_wire(), wire);
    }
}

#[test]
fn the_retired_compensation_reasons_no_longer_model_themselves() {
    // The four compensation codes left the vocabulary with the
    // mutate-in-place correction model
    // (`cpt-cf-usage-collector-adr-append-only-invalidation`). Because
    // `ConflictReason` is `#[non_exhaustive]`, their removal is silent for
    // a downstream matcher: it falls through to `Unknown` rather than
    // failing to build. So pin that the fall-through happens *and* that it
    // preserves the raw string, so a consumer reading an envelope stored
    // under the old model still sees what it said.
    for wire in [
        "ALREADY_INACTIVE",
        "CORRECTS_ID_TARGETS_COMPENSATION",
        "CORRECTS_ID_WRONG_SCOPE",
        "CORRECTS_ID_INACTIVE",
    ] {
        assert_eq!(
            ConflictReason::from_wire(wire),
            ConflictReason::Unknown(wire.to_owned()),
        );
        assert_eq!(ConflictReason::from_wire(wire).as_wire(), wire);
    }
}

#[test]
fn reasons_preserve_unknown_wire_string() {
    assert_eq!(
        ValidationReason::from_wire("FUTURE_CODE"),
        ValidationReason::Unknown("FUTURE_CODE".to_owned())
    );
    assert_eq!(
        ConflictReason::from_wire("FUTURE_CODE"),
        ConflictReason::Unknown("FUTURE_CODE".to_owned())
    );
}
