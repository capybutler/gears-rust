//! Typed `reason` discriminators carried by the compacted
//! [`crate::UsageCollectorError`] category variants.
//!
//! A typed view over
//! the stable `SCREAMING_SNAKE` wire codes, with `from_wire` / `as_wire`
//! round-trips and an [`Unknown`](ValidationReason::Unknown) catch-all that
//! preserves any future code verbatim. The host-side lift in
//! `usage-collector::infra::sdk_error_mapping` projects each onto the
//! RFC-9457 `Problem` (`field_violations[].reason` for [`ValidationReason`],
//! `context.reason` for [`ConflictReason`]).

use core::fmt;

// ─────────────────────────────────────────────────────────────────────
// ValidationReason — 400 InvalidArgument `field_violations[].reason`.
// ─────────────────────────────────────────────────────────────────────

/// Counter / gauge value-matrix violation (counter ordinary `value >= 0`,
/// counter compensation `value < 0`).
pub const SEMANTICS_VIOLATION: &str = "SEMANTICS_VIOLATION";
/// Generic request-shape validation failure (batch size, validating
/// newtypes, closed-kind enum parse). Catch-all when no finer code applies.
pub const VALIDATION: &str = "VALIDATION";
/// Per-record serialized-metadata size cap exceeded.
pub const METADATA_VALIDATION: &str = "METADATA_VALIDATION";
/// Ingestion supplied a metadata key not declared in the resolved meter
/// declaration's closed `metadata_fields` shape.
pub const UNKNOWN_METADATA_KEY: &str = "UNKNOWN_METADATA_KEY";
/// Malformed / wrong-base `gts_id` on a type or record DTO.
pub const INVALID_BASE_GTS_ID: &str = "INVALID_BASE_GTS_ID";
/// `metadata_fields[i]` entry was the empty string.
pub const INVALID_METADATA_FIELDS_EMPTY_STRING: &str = "INVALID_METADATA_FIELDS_EMPTY_STRING";
/// `metadata_fields[i]` entry failed `MetadataKey::new` (e.g. NUL byte).
pub const INVALID_METADATA_FIELDS_INVALID_KEY: &str = "INVALID_METADATA_FIELDS_INVALID_KEY";
/// Duplicate `metadata_fields[i]` entry.
pub const INVALID_METADATA_FIELDS_DUPLICATE: &str = "INVALID_METADATA_FIELDS_DUPLICATE";
/// A continuation token was refused: malformed, or bound to an order that
/// is not a usable keyset. Emitted by `toolkit_odata`'s own cursor decode
/// path and by the read path's keyset floor, and enumerated on the wire as
/// one of the `cursor` field violations in `usage-collector-v1.yaml`.
pub const INVALID_CURSOR: &str = "INVALID_CURSOR";
/// A continuation token was minted over a different query than the request
/// carrying it.
///
/// The bound query is **every input that selects rows**: the caller's
/// `$filter` and all three typed parameters — `gts_type_id`, the `from` /
/// `to` range, and the `metadata.<key>` filters. None of the three is a
/// `$filter` conjunct, so changing any one of them between pages surfaces
/// here exactly as changing `$filter` always did. In particular, keeping
/// `$filter` and the range fixed while changing the meter or a metadata
/// value does **not** let a cursor carry over.
///
/// Enumerated on the wire as one of the `cursor` field violations in
/// `usage-collector-v1.yaml`, and the code `toolkit_odata`'s own
/// filter-hash comparison already emits.
pub const FILTER_MISMATCH: &str = "FILTER_MISMATCH";
/// An aggregated query produced more distinct groups than
/// [`crate::MAX_AGGREGATION_BUCKETS`] — typically a high-cardinality `group_by`
/// (e.g. a per-record metadata key) over a wide range. Narrow the read-path
/// time range or drop the high-cardinality dimension.
pub const AGGREGATION_RESULT_TOO_LARGE: &str = "AGGREGATION_RESULT_TOO_LARGE";

/// Typed view of the `field_violations[].reason` codes carried by
/// [`crate::UsageCollectorError::InvalidArgument`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValidationReason {
    /// See [`SEMANTICS_VIOLATION`].
    SemanticsViolation,
    /// See [`VALIDATION`].
    Validation,
    /// See [`METADATA_VALIDATION`].
    MetadataValidation,
    /// See [`UNKNOWN_METADATA_KEY`].
    UnknownMetadataKey,
    /// See [`INVALID_BASE_GTS_ID`].
    InvalidBaseGtsId,
    /// See [`INVALID_METADATA_FIELDS_EMPTY_STRING`].
    MetadataFieldEmptyString,
    /// See [`INVALID_METADATA_FIELDS_INVALID_KEY`].
    MetadataFieldInvalidKey,
    /// See [`INVALID_METADATA_FIELDS_DUPLICATE`].
    MetadataFieldDuplicate,
    /// See [`AGGREGATION_RESULT_TOO_LARGE`].
    AggregationResultTooLarge,
    /// See [`INVALID_CURSOR`].
    InvalidCursor,
    /// See [`FILTER_MISMATCH`].
    FilterMismatch,
    /// Unmodeled / future reason — preserves the raw wire string.
    Unknown(String),
}

impl ValidationReason {
    /// Project a wire `field_violations[].reason` string into the typed
    /// discriminator. Any unmodeled value is preserved in [`Self::Unknown`].
    #[must_use]
    pub fn from_wire(s: &str) -> Self {
        match s {
            SEMANTICS_VIOLATION => Self::SemanticsViolation,
            VALIDATION => Self::Validation,
            METADATA_VALIDATION => Self::MetadataValidation,
            UNKNOWN_METADATA_KEY => Self::UnknownMetadataKey,
            INVALID_BASE_GTS_ID => Self::InvalidBaseGtsId,
            INVALID_METADATA_FIELDS_EMPTY_STRING => Self::MetadataFieldEmptyString,
            INVALID_METADATA_FIELDS_INVALID_KEY => Self::MetadataFieldInvalidKey,
            INVALID_METADATA_FIELDS_DUPLICATE => Self::MetadataFieldDuplicate,
            AGGREGATION_RESULT_TOO_LARGE => Self::AggregationResultTooLarge,
            INVALID_CURSOR => Self::InvalidCursor,
            FILTER_MISMATCH => Self::FilterMismatch,
            other => Self::Unknown(other.to_owned()),
        }
    }

    /// Render the discriminator back to its wire `reason` string. Inverse of
    /// [`Self::from_wire`] for the modeled variants.
    #[must_use]
    pub fn as_wire(&self) -> &str {
        match self {
            Self::SemanticsViolation => SEMANTICS_VIOLATION,
            Self::Validation => VALIDATION,
            Self::MetadataValidation => METADATA_VALIDATION,
            Self::UnknownMetadataKey => UNKNOWN_METADATA_KEY,
            Self::InvalidBaseGtsId => INVALID_BASE_GTS_ID,
            Self::MetadataFieldEmptyString => INVALID_METADATA_FIELDS_EMPTY_STRING,
            Self::MetadataFieldInvalidKey => INVALID_METADATA_FIELDS_INVALID_KEY,
            Self::MetadataFieldDuplicate => INVALID_METADATA_FIELDS_DUPLICATE,
            Self::AggregationResultTooLarge => AGGREGATION_RESULT_TOO_LARGE,
            Self::InvalidCursor => INVALID_CURSOR,
            Self::FilterMismatch => FILTER_MISMATCH,
            Self::Unknown(s) => s.as_str(),
        }
    }
}

impl fmt::Display for ValidationReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

// ─────────────────────────────────────────────────────────────────────
// ConflictReason — 409 Aborted `context.reason`.
// ─────────────────────────────────────────────────────────────────────

/// Deactivation targeted a record already `inactive` (one-way latch).
pub const ALREADY_INACTIVE: &str = "ALREADY_INACTIVE";
/// Same `idempotency_key`, canonical-field-different payload.
pub const IDEMPOTENCY_CONFLICT: &str = "IDEMPOTENCY_CONFLICT";
/// `corrects_id` referenced another compensation row.
pub const CORRECTS_ID_TARGETS_COMPENSATION: &str = "CORRECTS_ID_TARGETS_COMPENSATION";
/// `corrects_id` referenced a row in a different identity tuple.
pub const CORRECTS_ID_WRONG_SCOPE: &str = "CORRECTS_ID_WRONG_SCOPE";
/// `corrects_id` referenced an `inactive` row.
pub const CORRECTS_ID_INACTIVE: &str = "CORRECTS_ID_INACTIVE";

/// Typed view of the `context.reason` codes carried by
/// [`crate::UsageCollectorError::Conflict`] (the AIP-193 `Aborted` /
/// HTTP 409 category).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConflictReason {
    /// See [`ALREADY_INACTIVE`].
    AlreadyInactive,
    /// See [`IDEMPOTENCY_CONFLICT`].
    IdempotencyConflict,
    /// See [`CORRECTS_ID_TARGETS_COMPENSATION`].
    CorrectsIdTargetsCompensation,
    /// See [`CORRECTS_ID_WRONG_SCOPE`].
    CorrectsIdWrongScope,
    /// See [`CORRECTS_ID_INACTIVE`].
    CorrectsIdInactive,
    /// Unmodeled / future reason — preserves the raw wire string.
    Unknown(String),
}

impl ConflictReason {
    /// Project a wire `context.reason` string into the typed discriminator.
    /// Any unmodeled value is preserved in [`Self::Unknown`].
    #[must_use]
    pub fn from_wire(s: &str) -> Self {
        match s {
            ALREADY_INACTIVE => Self::AlreadyInactive,
            IDEMPOTENCY_CONFLICT => Self::IdempotencyConflict,
            CORRECTS_ID_TARGETS_COMPENSATION => Self::CorrectsIdTargetsCompensation,
            CORRECTS_ID_WRONG_SCOPE => Self::CorrectsIdWrongScope,
            CORRECTS_ID_INACTIVE => Self::CorrectsIdInactive,
            other => Self::Unknown(other.to_owned()),
        }
    }

    /// Render the discriminator back to its wire `reason` string. Inverse of
    /// [`Self::from_wire`] for the modeled variants.
    #[must_use]
    pub fn as_wire(&self) -> &str {
        match self {
            Self::AlreadyInactive => ALREADY_INACTIVE,
            Self::IdempotencyConflict => IDEMPOTENCY_CONFLICT,
            Self::CorrectsIdTargetsCompensation => CORRECTS_ID_TARGETS_COMPENSATION,
            Self::CorrectsIdWrongScope => CORRECTS_ID_WRONG_SCOPE,
            Self::CorrectsIdInactive => CORRECTS_ID_INACTIVE,
            Self::Unknown(s) => s.as_str(),
        }
    }
}

impl fmt::Display for ConflictReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "reason_tests.rs"]
mod reason_tests;
