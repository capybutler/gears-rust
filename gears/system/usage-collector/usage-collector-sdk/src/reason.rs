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

/// Reserved, and emitted by nothing in this crate.
///
/// It named a counter/gauge value-matrix rule keyed on the sign of a
/// quantity. The wire contract forbids that rule outright — `UsageQuantity`
/// in `usage-collector-v1.yaml` says the sign is never constrained, and a
/// negative quantity is an ordinary measurement recording a real decrease
/// — so the constructors that raised it are gone and nothing replaces
/// them.
///
/// Kept rather than removed: [`ValidationReason`] is `#[non_exhaustive]`,
/// so dropping a variant is silent for a downstream matcher, and a
/// consumer reading an envelope stored while the rule was live still needs
/// this to model itself.
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
/// A REST submission carried a target reference without a reason code, or
/// a reason code without a target reference. Raised at the fold point
/// where the flat wire pair becomes one `Option<Invalidation>`; no
/// in-process caller can reach it, because the domain type makes the
/// half-shape unrepresentable.
pub const INVALIDATION_REFERENCE_INCOMPLETE: &str = "INVALIDATION_REFERENCE_INCOMPLETE";
/// An invalidation's target was itself an invalidation.
pub const INVALIDATION_TARGET_NOT_RECORD: &str = "INVALIDATION_TARGET_NOT_RECORD";
/// An invalidation departed from its target in a field it must copy.
pub const INVALIDATION_FIELD_MISMATCH: &str = "INVALIDATION_FIELD_MISMATCH";
/// The covered period ends further into the future than the live path's
/// configured future tolerance. Raised on **both** ingestion routes: the
/// bound guards against an emitter opening a period that does not yet
/// exist, and the backfill route lifts only the past bound.
pub const FUTURE_WINDOW: &str = "FUTURE_WINDOW";
/// The covered period ends further into the past than the live path's
/// configured past tolerance. Raised on the live path only; the detail
/// names the backfill route, which exists for exactly these periods
/// (`cpt-cf-usage-collector-adr-backfill-isolation`).
pub const PAST_WINDOW: &str = "PAST_WINDOW";

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
    /// See [`INVALIDATION_REFERENCE_INCOMPLETE`].
    InvalidationReferenceIncomplete,
    /// See [`INVALIDATION_TARGET_NOT_RECORD`].
    InvalidationTargetNotRecord,
    /// See [`INVALIDATION_FIELD_MISMATCH`].
    InvalidationFieldMismatch,
    /// See [`FUTURE_WINDOW`].
    FutureWindow,
    /// See [`PAST_WINDOW`].
    PastWindow,
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
            INVALIDATION_REFERENCE_INCOMPLETE => Self::InvalidationReferenceIncomplete,
            INVALIDATION_TARGET_NOT_RECORD => Self::InvalidationTargetNotRecord,
            INVALIDATION_FIELD_MISMATCH => Self::InvalidationFieldMismatch,
            FUTURE_WINDOW => Self::FutureWindow,
            PAST_WINDOW => Self::PastWindow,
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
            Self::InvalidationReferenceIncomplete => INVALIDATION_REFERENCE_INCOMPLETE,
            Self::InvalidationTargetNotRecord => INVALIDATION_TARGET_NOT_RECORD,
            Self::InvalidationFieldMismatch => INVALIDATION_FIELD_MISMATCH,
            Self::FutureWindow => FUTURE_WINDOW,
            Self::PastWindow => PAST_WINDOW,
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

/// Same `idempotency_key`, canonical-field-different payload.
pub const IDEMPOTENCY_CONFLICT: &str = "IDEMPOTENCY_CONFLICT";
/// A second withdrawal of an already-invalidated record. The store detects
/// it, atomically against the entry it admits.
pub const ALREADY_INVALIDATED: &str = "ALREADY_INVALIDATED";

/// Typed view of the `context.reason` codes carried by
/// [`crate::UsageCollectorError::Conflict`] (the AIP-193 `Aborted` /
/// HTTP 409 category).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConflictReason {
    /// See [`IDEMPOTENCY_CONFLICT`].
    IdempotencyConflict,
    /// See [`ALREADY_INVALIDATED`].
    AlreadyInvalidated,
    /// Unmodeled / future reason — preserves the raw wire string.
    Unknown(String),
}

impl ConflictReason {
    /// Project a wire `context.reason` string into the typed discriminator.
    /// Any unmodeled value is preserved in [`Self::Unknown`].
    #[must_use]
    pub fn from_wire(s: &str) -> Self {
        match s {
            IDEMPOTENCY_CONFLICT => Self::IdempotencyConflict,
            ALREADY_INVALIDATED => Self::AlreadyInvalidated,
            other => Self::Unknown(other.to_owned()),
        }
    }

    /// Render the discriminator back to its wire `reason` string. Inverse of
    /// [`Self::from_wire`] for the modeled variants.
    #[must_use]
    pub fn as_wire(&self) -> &str {
        match self {
            Self::IdempotencyConflict => IDEMPOTENCY_CONFLICT,
            Self::AlreadyInvalidated => ALREADY_INVALIDATED,
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
