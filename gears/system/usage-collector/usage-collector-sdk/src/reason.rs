//! Typed `reason` discriminators carried by the compacted
//! [`crate::UsageCollectorError`] category variants.
//!
//! Most of them are typed views over the stable `SCREAMING_SNAKE` wire
//! codes, with `from_wire` / `as_wire` round-trips and an
//! [`Unknown`](ValidationReason::Unknown) catch-all that preserves any
//! future code verbatim. The host-side lift in
//! `usage-collector::infra::sdk_error_mapping` projects each onto the
//! RFC-9457 `Problem` (`field_violations[].reason` for [`ValidationReason`],
//! `context.reason` for [`ConflictReason`]).
//!
//! [`NotFoundReason`] is the exception: it is **not projected onto the
//! wire**, because the platform-shared
//! `toolkit_canonical_errors::NotFoundV1` context has no reason slot for it
//! to land in. It therefore carries no wire constants and no round-trip,
//! and serves in-process consumers and the gear's own classification. Read
//! its own documentation before treating anything in this module as
//! uniformly wire-facing.

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
/// The covered period ends further into the future than the ingestion
/// path's configured future tolerance. Raised on **both** ingestion
/// routes — one configured value governs each of them: the bound guards
/// against an emitter opening a period that does not yet exist, and the
/// backfill route lifts only the past bound.
pub const FUTURE_WINDOW: &str = "FUTURE_WINDOW";
/// The covered period ends further into the past than the live path's
/// configured past tolerance. Raised on the live path only; the detail
/// names the backfill route, which exists for exactly these periods
/// (`cpt-cf-usage-collector-adr-backfill-isolation`).
pub const PAST_WINDOW: &str = "PAST_WINDOW";

/// Typed view of the `field_violations[].reason` codes carried by
/// [`crate::UsageCollectorError::InvalidArgument`].
///
/// It does **not** model `INVALID_CURSOR`, `FILTER_MISMATCH` or
/// `ORDER_WITH_CURSOR`. All three belong to `toolkit_odata`, which declares
/// them on its own cursor error enum and maps them to `Problem` field
/// violations. Modeling them here as well would create a second place the
/// same code can be read and disagree (Spec §3.13).
///
/// They do not all reach the wire the same way, and the difference matters
/// to anyone tracing one back to its origin. `INVALID_CURSOR` and
/// `FILTER_MISMATCH` are raised by this gear's own continuation checks and
/// travel through [`crate::UsageCollectorError::CursorRejected`], which
/// carries the upstream error so the lift can read the code off it.
/// `ORDER_WITH_CURSOR` never reaches this gear at all: `toolkit_odata`'s
/// axum extractor refuses `$orderby` alongside a cursor and returns its own
/// canonical error before the handler runs, so no `CursorRejected` is ever
/// built for it and looking for one is a dead end.
///
/// A consumer matching on any of the three reads it out of the wire string
/// via [`Self::Unknown`].
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

// ─────────────────────────────────────────────────────────────────────
// NotFoundReason — 404. In-process only; NOT projected onto the wire.
// ─────────────────────────────────────────────────────────────────────

/// Which lookup failed, on [`crate::UsageCollectorError::NotFound`].
///
/// **Not projected onto the wire, and deliberately not.** An in-process SDK
/// consumer — one resolving [`crate::api::UsageCollectorClientV1`] through
/// `ClientHub` — reads this discriminator directly. A REST client does not,
/// and tells the three cases apart only by `detail` prose. So unlike
/// [`ValidationReason`] and [`ConflictReason`] these have no
/// `SCREAMING_SNAKE` constants, no `from_wire` / `as_wire`, and never
/// appear on a `Problem` body.
///
/// Projecting it is a two-part change and the first part is not this
/// gear's. `toolkit_canonical_errors::NotFoundV1` is an **empty,
/// platform-shared** context struct with no reason path at all, and the
/// canonical builder constructs it with none — so every gear's 404 is in
/// the same position, and a slot has to exist there first. Only then does
/// the gear's half apply: `usage-collector-v1.yaml` already declares the
/// 404 `context` as `additionalProperties: true`, so the schema does not
/// forbid a reason, but its prose enumerates the three categories that
/// carry one and would have to name 404 as a fourth. Reading the YAML
/// alone suggests the work is already done; it is not.
///
/// What it exists for is the gear's own classification. §3.11.5's
/// `invalidation_rule` covers "the copy, reference and at-most-one rules",
/// and the reference rule — an `invalidates` resolving to nothing — is a
/// `NotFound`. Without a discriminator the only thing separating it from an
/// ordinary missing entry is the message string, and classifying a bounded
/// metric label by substring match on caller-facing prose is how a label
/// stops matching silently when the prose is reworded.
///
/// `#[non_exhaustive]` for a different reason than the wire enums, which
/// need it because unmodeled values arrive at runtime — hence their
/// `Unknown(String)`. This has no external producer; the value is that the
/// SDK can add a variant in a minor release without breaking a matcher.
/// The cost is real and points the same way this type was added to fix: a
/// consumer's wildcard arm silently absorbs the new kind, and the gear's
/// own wildcard in `classify_record_error` counts it as
/// `semantics_violation`. A new variant must therefore be reviewed against
/// DESIGN §3.11.5 and given an explicit arm, not left to the wildcard.
// The shared `NotFound` postfix is the point: each variant is named for the
// error it lifts from (`DomainError::DeclarationNotFound`,
// `UsageCollectorPluginError::UsageRecordNotFound`, and the invalidation
// target check), so a reader can follow a reason back to its origin by name.
// Renaming to satisfy `enum_variant_names` would sever that.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NotFoundReason {
    /// A `gts_type_id` that does not resolve to a usable declaration —
    /// the Type Resolver's `DeclarationNotFound`, whether the registry
    /// never declared it or the declaration was rejected as incomplete.
    DeclarationNotFound,
    /// A lookup named a `UsageRecord.id` that did not resolve. Three
    /// producers, and the third is why this variant must stay one variant:
    ///
    /// - the by-id point read, when the store genuinely holds no such row;
    /// - a plugin's own `UsageRecordNotFound` raised from an SPI call
    ///   other than the invalidation-target lookup (that one is converted
    ///   to [`Self::InvalidationTargetNotFound`] at the fan-out);
    /// - a **PDP denial on the by-id point read**, collapsed into this
    ///   exact reason by the host so the surface is not an existence
    ///   oracle. There the row may well exist.
    ///
    /// The collapsed denial and the genuine miss are indistinguishable on
    /// purpose. Splitting them off into a variant of their own — however
    /// it is spelled, and however precise it looks — hands every
    /// in-process consumer the oracle the collapse exists to deny. Do not
    /// do it.
    UsageRecordNotFound,
    /// An invalidation's `invalidates` resolved to nothing. The
    /// valid-reference rule of
    /// `cpt-cf-usage-collector-adr-append-only-invalidation`. A plugin's
    /// `UsageRecordNotFound` from the target lookup is converted to this
    /// at the ingestion fan-out, so on the ingest path it is this reason,
    /// not [`Self::UsageRecordNotFound`], that the target check raises.
    InvalidationTargetNotFound,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "reason_tests.rs"]
mod reason_tests;
