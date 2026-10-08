//! Usage Collector SDK error types.
//!
//! Two `thiserror::Error` enums make up the SDK's error vocabulary:
//!
//! - [`UsageCollectorError`] — public envelope returned by every
//!   [`crate::api::UsageCollectorClientV1`] method. A flat, AIP-193-shaped
//!   set of **nine category variants**: the discriminator
//!   inside a category is normally a typed [`crate::reason`] sub-enum
//!   ([`ValidationReason`], [`ConflictReason`], [`NotFoundReason`]) rather
//!   than a dedicated variant per failure. The exception is
//!   [`UsageCollectorError::CursorRejected`], the one 400 whose wire code
//!   belongs to `toolkit_odata` rather than to this gear (Spec §3.13): it
//!   is a dedicated variant, its discriminator is the upstream
//!   `toolkit_odata::Error` it carries, and it carries no `resource_type`
//!   of its own.
//! - [`UsageCollectorPluginError`] — plugin-side vocabulary returned by
//!   every [`crate::plugin_api::UsageCollectorPluginV1`] method.
//!
//! This crate does NOT depend on `toolkit-canonical-errors`; the host crate
//! owns the lift to RFC-9457 `Problem` at the REST boundary. The category +
//! typed reason + `resource_type` carried here are what the lift projects
//! onto the canonical envelope, so callers dispatch on the variant (and,
//! within a category, the typed reason) rather than parsing strings. One
//! reason is deliberately held back from the wire: [`NotFoundReason`] reaches
//! in-process consumers only, the platform-shared
//! `toolkit_canonical_errors::NotFoundV1` context having no reason slot.
//! `CursorRejected` is again the exception on both counts: the lift reads its
//! wire `reason` off the upstream error and supplies the
//! `USAGE_RECORD_RESOURCE` scope itself, while the wire `field` is named by
//! the variant's own [`CursorField`], upstream knowing only `cursor` and the
//! feed carrying two cursor-bearing parameters.

use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::gts::USAGE_RECORD_RESOURCE;
use crate::models::{AggregationDimension, BACKFILL_ROUTE_PATH, MeterTypeId, WINDOW_END_FIELD};
use crate::reason::{ConflictReason, NotFoundReason, ValidationReason};

/// Renders an instant the way a caller-facing `detail` must echo it: RFC
/// 3339, matching the wire `Timestamp` contract.
///
/// [`OffsetDateTime`]'s own `Display` is space-separated, unpadded, and
/// offset-suffixed rather than `Z` (`2023-11-15 1:43:20.0 +00:00:00`), so it
/// echoes back neither what the caller sent nor anything they could
/// resubmit. Every timestamp-bearing constructor below routes through here
/// so no single one can drift back onto `Display`.
///
/// Formatting can fail, and on the *value* rather than the descriptor:
/// [`Rfc3339`] rejects a year outside `0..10_000` and an offset carrying
/// non-zero seconds, both constructible in-process though no RFC 3339 wire
/// payload can express either. The fallback is therefore a genuine last
/// resort, trading a panic for a self-consistent `Display` rendering.
fn rfc3339(instant: OffsetDateTime) -> String {
    instant
        .format(&Rfc3339)
        .unwrap_or_else(|_| instant.to_string())
}

/// Renders an [`AggregationDimension`] the way a caller spells it in a
/// `group_by` request, so a `detail` naming the offending dimension echoes
/// something the caller actually wrote rather than a Rust-side debug form.
///
/// The five fixed variants serialize as a bare `snake_case` string
/// (`"tenant_id"`), while `Metadata(MetadataKey)` is a newtype variant and
/// serializes as a tagged object (`{"metadata":"region"}`). The two shapes are
/// rendered differently here because a fixed dimension's wire form has no
/// surrounding quotes in a request, so echoing `serde_json`'s quoted
/// `Value::String` verbatim would show the caller something they did not type.
fn dimension_wire_name(dimension: &AggregationDimension) -> String {
    match serde_json::to_value(dimension) {
        Ok(serde_json::Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(e) => unreachable!(
            "AggregationDimension's Serialize is derived over five unit variants and a \
             MetadataKey newtype (itself #[serde(transparent)] over String); \
             serde_json::to_value only fails on an erroring Serialize impl, a non-string map \
             key, or a non-finite float, none of which this type can produce: {e}"
        ),
    }
}

/// Which cursor-bearing parameter a [`UsageCollectorError::CursorRejected`]
/// is about.
///
/// The feed carries two: `cursor`, the resume point, and `until`, the replay
/// bound. `usage-collector-v1.yaml` requires a defect in either to be
/// reported on its own parameter, and DESIGN §3.3 says the same — "rejected
/// with an `until` field violation". The upstream `toolkit_odata::Error` that
/// supplies this variant's wire *reason* knows nothing of `until`, so the
/// field is the gear's to name and the reason is not.
///
/// A closed enum rather than a string: the set of cursor-bearing surfaces is
/// small, known, and worth a compile error when a third one appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorField {
    /// The `cursor` parameter — a resume point, on the raw path and the feed.
    Cursor,
    /// The `until` parameter — the feed's replay bound.
    Until,
}

impl CursorField {
    /// The wire parameter name this violation is reported on.
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Cursor => "cursor",
            Self::Until => "until",
        }
    }
}

/// The reason a [`UsageCollectorError::Conflict`] was raised, and the data
/// specific to that reason.
///
/// A single field in place of three independently-`Option` ones
/// (`invalidated_by`, `reason_code`, `retry_after_seconds`): each payload
/// lives on the one variant it belongs to, so a value carrying
/// `invalidated_by` while the reason is [`ConflictReason::IdempotencyConflict`]
/// does not typecheck. [`Self::reason`] recovers the bare [`ConflictReason`]
/// label the wire `context.reason` and every existing classifier matches on.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConflictOutcome {
    /// See [`ConflictReason::IdempotencyConflict`].
    IdempotencyConflict,
    /// See [`ConflictReason::AlreadyInvalidated`].
    AlreadyInvalidated {
        /// The stored invalidation that already withdrew the target.
        invalidated_by: Uuid,
        /// The reason code the stored invalidation carries.
        reason_code: crate::models::ReasonCode,
    },
    /// See [`ConflictReason::TargetNotConverged`].
    TargetNotConverged {
        /// The configured `target_not_converged_retry_after_secs` delay
        /// (DESIGN §3.8) where the host supplied one, `None` otherwise. This
        /// crate carries no config of its own, so the value is never
        /// synthesized. Read by [`UsageCollectorError::retry_after`].
        retry_after_seconds: Option<u64>,
    },
}

impl ConflictOutcome {
    /// The bare wire-facing discriminator this payload is for — what the
    /// host lift projects onto `context.reason`.
    #[must_use]
    pub fn reason(&self) -> ConflictReason {
        match self {
            Self::IdempotencyConflict => ConflictReason::IdempotencyConflict,
            Self::AlreadyInvalidated { .. } => ConflictReason::AlreadyInvalidated,
            Self::TargetNotConverged { .. } => ConflictReason::TargetNotConverged,
        }
    }
}

/// Public error envelope for the Usage Collector SDK and REST surfaces.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum UsageCollectorError {
    /// PDP denial on the requested operation (HTTP 403). `detail` is the
    /// PDP-supplied reason, kept for operator logs; the host lift drops it
    /// from the public wire body (the SDK never paraphrases PDP detail) and
    /// emits `context.reason="AUTHZ"`.
    #[error("authorization denied: {detail}")]
    PermissionDenied {
        /// PDP-supplied reason (operator-log facing; not echoed to the wire).
        detail: String,
    },

    /// Request-shape / semantics validation failure (HTTP 400). `field` is
    /// the attributed request field, `reason` the typed
    /// [`ValidationReason`] discriminator, and `detail` the wire
    /// `field_violations[0].description`. `resource_type` identifies the GTS
    /// resource the violation is about (a `gts_type_id`-shaped field
    /// violation attributes to the referenced meter even on the ingestion
    /// surface); `resource_name`, when present, is the offending identifier.
    #[error("invalid argument [{field}/{reason}]: {detail}")]
    InvalidArgument {
        /// GTS resource type — [`USAGE_RECORD_RESOURCE`].
        resource_type: String,
        /// The offending identifier, when the violation is about a
        /// specific resource; `None` otherwise. Which identifier depends on
        /// what the violation is about: a `gts_id` for a meter-shaped
        /// violation, the target's `UsageRecord.id` for one about an
        /// invalidation's target.
        resource_name: Option<String>,
        /// Attributed request field (`quantity`, `records`, `metadata`, …).
        field: String,
        /// Typed `field_violations[0].reason` discriminator.
        reason: ValidationReason,
        /// Wire `field_violations[0].description`.
        detail: String,
    },

    /// A continuation token refused by the gear, carrying the wire code
    /// `toolkit_odata` owns.
    ///
    /// Spec §3.13 gives `INVALID_CURSOR`, `FILTER_MISMATCH` and
    /// `ORDER_WITH_CURSOR` to `toolkit_odata`: the gear originates none of
    /// them, a second declaration being a second place the same code can be
    /// read and disagree. `source` is therefore the sole source of the wire
    /// `reason`; the wire `field` is not upstream's to give (DESIGN §3.3).
    ///
    /// `detail` is the gear's own, and is why this variant carries two things
    /// rather than one: upstream's descriptions name the condition but not
    /// the recovery (`FilterMismatch` renders as "Filter mismatch between
    /// cursor and query"). The code comes from upstream, the prose stays here.
    #[error("cursor rejected [{source}]: {detail}")]
    CursorRejected {
        /// The upstream cursor error — sole source of the wire **reason**
        /// for a malformed token or a changed filter, and not the source of
        /// the wire *field* (see `field` below). An order supplied alongside
        /// a cursor is intercepted upstream and never reaches `source` (see
        /// [`crate::reason::ValidationReason`]); a retention refusal is the
        /// plugin's and lifts to `InvalidArgument` carrying
        /// `ValidationReason::CursorBeyondRetention`.
        source: toolkit_odata::Error,
        /// Gear-authored caller guidance, rendered as the violation
        /// description.
        detail: String,
        /// Which cursor-bearing parameter the violation is reported on.
        field: CursorField,
    },

    /// Referenced resource not found (HTTP 404). `resource_type` is the GTS
    /// type, `name` the raw identifier (a `gts_type_id` or a record UUID),
    /// and `reason` says which lookup failed.
    #[error("not found [{resource_type}]: {detail}")]
    NotFound {
        /// GTS resource type — [`USAGE_RECORD_RESOURCE`].
        resource_type: String,
        /// Raw identifier whose row was not present.
        name: String,
        /// Which lookup failed. **Not projected onto the wire** — the
        /// platform-shared `toolkit_canonical_errors::NotFoundV1` context has
        /// no reason slot, so a REST client tells the cases apart only by
        /// `detail`. It exists so the gear can classify its own metric labels
        /// without matching a substring of caller-facing prose.
        reason: NotFoundReason,
        /// Wire `detail` message.
        detail: String,
    },

    /// Duplicate-on-create conflict (HTTP 409). Identical-payload
    /// resubmission is idempotent and returns the stored row on `Ok`.
    #[error("already exists [{resource_type}]: {detail}")]
    AlreadyExists {
        /// GTS resource type — [`USAGE_RECORD_RESOURCE`].
        resource_type: String,
        /// Raw identifier (`gts_id`) that collided.
        name: String,
        /// Wire `detail` message.
        detail: String,
    },

    /// State / concurrency / referential-integrity conflict (HTTP 409,
    /// AIP-193 `Aborted`). `outcome` carries the typed [`ConflictReason`]
    /// discriminator (via [`ConflictOutcome::reason`], projected onto the
    /// wire `context.reason`) together with the data specific to that
    /// reason; `resource_type` / `name` identify the row involved.
    #[error("conflict [{}]: {detail}", .outcome.reason())]
    Conflict {
        /// GTS resource type — [`USAGE_RECORD_RESOURCE`].
        resource_type: String,
        /// Raw identifier of the row involved (`gts_id` / record UUID).
        name: String,
        /// The reason this conflict was raised, and the data that reason
        /// alone carries.
        outcome: ConflictOutcome,
        /// Wire `detail` message.
        detail: String,
    },

    /// Transient infrastructure unavailability (HTTP 503). Covers
    /// host-structural readiness (plugin / types-registry), plugin-reported
    /// transience, and PDP-transport outages — operator triage reads the
    /// curated `detail` string. Carries an optional `retry_after_seconds`
    /// hint. The only classification [`Self::is_retryable`] recognizes —
    /// that predicate is narrower than the full retry decision, which is
    /// [`Self::retry_after`] (DESIGN §3.3 Error Contract: `is_retryable` is
    /// true for `ServiceUnavailable` alone and "is not the complete retry
    /// decision"; three outcomes carry a delay).
    #[error("service unavailable: {detail}")]
    ServiceUnavailable {
        /// Optional retry hint forwarded onto the `Retry-After` slot.
        retry_after_seconds: Option<u64>,
        /// Operator-facing detail (DSN-free, pre-redacted at construction).
        detail: String,
    },

    /// Per-subject ingestion allowance exhausted (HTTP 429). The submission is
    /// rejected whole before any entry is validated, as the structural entry
    /// cap already is. `retry_after_seconds` is the delay after which the
    /// bucket holds enough tokens for a submission of this size — computable
    /// because the entry cap runs first and bounds the cost at
    /// `max_batch_records` (DESIGN §3.2). `detail` names the allowance and the
    /// submitted count, so an operator can tell a burst from a sustained
    /// overrun.
    #[error("ingestion quota exceeded: {detail}")]
    ResourceExhausted {
        /// Seconds until a submission of this size could be admitted.
        /// Forwarded onto the `Retry-After` slot, like
        /// [`Self::ServiceUnavailable`]'s hint.
        retry_after_seconds: u64,
        /// Operator-facing detail naming the allowance and the submitted count.
        detail: String,
    },

    /// Unclassified failure (HTTP 500). `detail` MUST be DSN-free and
    /// pre-redacted at the construction site.
    #[error("internal error: {detail}")]
    Internal {
        /// Pre-redacted operator-facing detail.
        detail: String,
    },
}

/// Named arguments for [`UsageCollectorError::already_invalidated`].
///
/// `target` and `invalidated_by` are both bare `Uuid` in positional order;
/// naming them stops a transposed call from compiling silently
/// (RUST-API-001).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlreadyInvalidatedArgs {
    /// The usage record the invalidation targets.
    pub target: Uuid,
    /// The stored invalidation that already withdrew the target.
    pub invalidated_by: Uuid,
    /// The reason code the stored invalidation carries.
    pub reason_code: crate::models::ReasonCode,
}

/// Named arguments for [`UsageCollectorError::ingestion_quota_exceeded`].
///
/// `allowance` and `retry_after_seconds` are both bare `u64`; naming them
/// stops a transposed call from compiling silently (RUST-API-001).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestionQuotaExceededArgs {
    /// The configured per-subject allowance the submission was rejected
    /// against.
    pub allowance: u64,
    /// The number of entries the rejected submission carried.
    pub submitted: usize,
    /// Seconds until a submission of this size could be admitted.
    pub retry_after_seconds: u64,
}

impl UsageCollectorError {
    // ── PermissionDenied (403) ──────────────────────────────────────────

    /// PDP denial. `detail` is the PDP-supplied reason (operator-log facing).
    #[must_use]
    pub fn permission_denied(detail: impl Into<String>) -> Self {
        Self::PermissionDenied {
            detail: detail.into(),
        }
    }

    // ── InvalidArgument (400) ───────────────────────────────────────────

    /// Batch submission size out of bounds (empty or over the per-call cap).
    #[must_use]
    pub fn invalid_batch_size(actual: usize, min: usize, max: usize) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "records".to_owned(),
            reason: ValidationReason::Validation,
            detail: format!("batch size {actual} out of bounds (expected [{min}, {max}])"),
        }
    }

    /// A quantity outside the published `UsageQuantity` range or precision.
    /// `field` is `quantity`.
    #[must_use]
    pub fn quantity_out_of_range(detail: impl Into<String>) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "quantity".to_owned(),
            reason: ValidationReason::QuantityOutOfRange,
            detail: detail.into(),
        }
    }

    /// Serialized metadata exceeded the per-record size cap.
    #[must_use]
    pub fn metadata_size_exceeded(size: usize, cap: usize) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "metadata".to_owned(),
            reason: ValidationReason::MetadataValidation,
            detail: format!("metadata size {size} bytes exceeds cap {cap} bytes"),
        }
    }

    /// A covered-period bound carried finer than microsecond precision.
    ///
    /// The `detail` names the offending bound, echoes it back in a form the
    /// caller can resubmit, and states the remedy, because the caller's
    /// only route forward is to round the value themselves — the gear
    /// deliberately will not do it for them.
    #[must_use]
    pub fn sub_microsecond_period_bound(field: &str, bound: OffsetDateTime) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: field.to_owned(),
            reason: ValidationReason::Validation,
            detail: format!(
                "covered-period bound `{field}` requires at most microsecond \
                 precision (got {}, carrying {} ns); round the bound to the \
                 microsecond before submitting — the entry identity \
                 derivation reads a fixed-width microsecond form, so a finer \
                 value is rejected rather than truncated",
                rfc3339(bound),
                bound.nanosecond(),
            ),
        }
    }

    /// The covered period was inverted (`window_end < window_start`).
    ///
    /// Attributed to `window_end` rather than to the period as a whole: the
    /// period is not a wire field, and the end is the bound a caller
    /// computes from the start.
    #[must_use]
    pub fn inverted_covered_period(
        window_start: OffsetDateTime,
        window_end: OffsetDateTime,
    ) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: WINDOW_END_FIELD.to_owned(),
            reason: ValidationReason::Validation,
            detail: format!(
                "covered period requires window_start <= window_end (got \
                 window_start={}, window_end={}); equal bounds are a point \
                 event and are valid",
                rfc3339(window_start),
                rfc3339(window_end),
            ),
        }
    }

    /// The covered period ends beyond the ingestion path's future tolerance.
    ///
    /// Raised on **both** routes: the backfill route lifts the past bound
    /// and nothing else, so the detail deliberately does not mention it.
    /// Pointing a clock-skewed emitter at the backfill route would send a
    /// defect somewhere it is just as invalid.
    #[must_use]
    pub fn covered_period_beyond_future_tolerance(
        window_end: OffsetDateTime,
        now: OffsetDateTime,
        tolerance: Duration,
    ) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: WINDOW_END_FIELD.to_owned(),
            reason: ValidationReason::FutureWindow,
            detail: format!(
                "covered period ends at {}, more than {tolerance} after now \
                 ({}); every ingestion path admits only a period ending \
                 within that tolerance of the present",
                rfc3339(window_end),
                rfc3339(now),
            ),
        }
    }

    /// The covered period ends beyond the live path's past tolerance.
    ///
    /// The detail names the backfill route on both surfaces, per
    /// `cpt-cf-usage-collector-adr-backfill-isolation`: "The rejection names
    /// the route and both surfaces carry it" — a REST caller needs the path,
    /// an in-process caller the method. It says "entry" rather than "record"
    /// because the same rejection meets an invalidation withdrawing a closed
    /// period, the ordinary case for a correction.
    #[must_use]
    pub fn covered_period_before_past_tolerance(
        window_end: OffsetDateTime,
        now: OffsetDateTime,
        tolerance: Duration,
    ) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: WINDOW_END_FIELD.to_owned(),
            reason: ValidationReason::PastWindow,
            detail: format!(
                "covered period ends at {}, more than {tolerance} before now \
                 ({}); the live path admits only a period ending within that \
                 tolerance. Submit this entry on the backfill route instead \
                 — `POST {BACKFILL_ROUTE_PATH}`, or `backfill_usage_records` \
                 on the SDK trait",
                rfc3339(window_end),
                rfc3339(now),
            ),
        }
    }

    /// A read-path time range was empty or inverted (`to <= from`).
    ///
    /// [`crate::TimeRange`] is mandatory on both range-taking read paths
    /// (the point lookup selects by `id` and takes none) and selects an
    /// entry when `from <= window_end < to`
    /// (`cpt-cf-usage-collector-adr-window-end-selection`), so a range that
    /// is not strictly ordered selects nothing whatever is stored.
    /// Rejecting it here names the caller's mistake instead of reporting an
    /// empty result that would read as "no usage".
    #[must_use]
    pub fn invalid_time_range(from: OffsetDateTime, to: OffsetDateTime) -> Self {
        let from = rfc3339(from);
        let to = rfc3339(to);
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            // `field` names the range as a whole: both bounds parsed, and the
            // ordering between them is what failed. Shared by both read paths,
            // so the detail names both carriers.
            field: "time_range".to_owned(),
            reason: ValidationReason::Validation,
            detail: format!(
                "time range requires from < to (got from={from}, to={to}); supply a \
                 lower bound strictly before the upper bound. The range arrives as \
                 the `from` / `to` query parameters on the raw path and as \
                 `time_range` in the aggregate request body"
            ),
        }
    }

    /// An aggregated query produced more than `cap`
    /// ([`crate::MAX_AGGREGATION_BUCKETS`]) buckets — a high-cardinality
    /// `group_by` (e.g. a per-record metadata key) over a wide range. The
    /// plugin bounds its own scan to `cap + 1` rows (memory guard); the gateway
    /// rejects the over-cap result as this client-fixable `400`.
    #[must_use]
    pub fn aggregation_result_too_large(cap: usize) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "group_by".to_owned(),
            reason: ValidationReason::AggregationResultTooLarge,
            detail: format!(
                "aggregated query produced more than {cap} groups; narrow the \
                 time range or drop a high-cardinality group_by dimension"
            ),
        }
    }

    /// `MeterTypeId::new` rejected `raw` — malformed, wrong-base, or
    /// multi-segment `gts_type_id`.
    #[must_use]
    pub fn invalid_meter_type_id(raw: &str, reason: &str) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "gts_type_id".to_owned(),
            reason: ValidationReason::InvalidBaseGtsId,
            detail: format!("gts_type_id `{raw}` rejected: {reason}"),
        }
    }

    /// `AggregationFold::from_str` received a string outside the declared
    /// set. Raised when a resolved declaration names a fold this major
    /// version does not serve.
    #[must_use]
    pub fn invalid_aggregation_fold(raw: &str) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "aggregation_fold".to_owned(),
            reason: ValidationReason::Validation,
            detail: format!(
                "unknown aggregation fold `{raw}`; expected one of SUM, COUNT, MAX, MIN, LATEST"
            ),
        }
    }

    /// Build a record-surface validating-newtype `InvalidArgument` whose wire
    /// `detail` is the newtype's self-describing reason. `field` attributes
    /// the violation (`metadata`, `resource_ref`, …).
    #[must_use]
    fn newtype_validation(field: &str, detail: impl Into<String>) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: field.to_owned(),
            reason: ValidationReason::Validation,
            detail: detail.into(),
        }
    }

    /// `MetadataKey::new` rejected the input. `field` is `metadata`.
    #[must_use]
    pub fn invalid_metadata_key(detail: impl Into<String>) -> Self {
        Self::newtype_validation("metadata", detail)
    }

    /// `MetadataFilter::new` rejected the input. `field` is `metadata_filter`.
    #[must_use]
    pub fn invalid_metadata_filter(detail: impl Into<String>) -> Self {
        Self::newtype_validation("metadata_filter", detail)
    }

    /// `ResourceRef::new` rejected the input. `field` is `resource_ref`.
    #[must_use]
    pub fn invalid_resource_ref(detail: impl Into<String>) -> Self {
        Self::newtype_validation("resource_ref", detail)
    }

    /// `SubjectRef::new` rejected the input. `field` is `subject_ref`.
    #[must_use]
    pub fn invalid_subject_ref(detail: impl Into<String>) -> Self {
        Self::newtype_validation("subject_ref", detail)
    }

    /// `IdempotencyKey::new` rejected the input. `field` is `idempotency_key`.
    #[must_use]
    pub fn invalid_idempotency_key(detail: impl Into<String>) -> Self {
        Self::newtype_validation("idempotency_key", detail)
    }

    /// `ReasonCode::new` rejected the input. `field` is `reason_code`.
    #[must_use]
    pub fn invalid_reason_code(detail: impl Into<String>) -> Self {
        Self::newtype_validation("reason_code", detail)
    }

    /// An entry carried no idempotency key. `field` is `idempotency_key`.
    ///
    /// Both kinds carry one: an invalidation repeats its target's, which is
    /// how the gateway finds the entry being withdrawn (DESIGN §3.1, Target
    /// resolution).
    #[must_use]
    pub fn missing_idempotency_key() -> Self {
        Self::newtype_validation(
            "idempotency_key",
            "idempotency_key is required on every entry; an invalidation repeats its target's",
        )
    }

    /// [`crate::CreateUsageRecord::try_into_usage_record`] was handed a
    /// submission that declares `entry_type: invalidation`.
    ///
    /// The submission is a withdrawal, and a withdrawal has to name the
    /// entry it withdraws — a reference only the gateway can resolve, so
    /// [`crate::CreateUsageRecord::try_into_invalidation_record`] is the
    /// projection that takes it. The measurement projection refuses rather
    /// than dropping the reason, which would admit the submission as an
    /// ordinary record and collide it with its own target.
    ///
    /// **[`Self::Internal`], not [`Self::InvalidArgument`].** Reaching this
    /// means the gateway chose the projection that contradicts the submission
    /// it holds — a host-contract breach, not anything the emitter did, so the
    /// `detail` is addressed to whoever wired the gateway and must not travel
    /// to a REST caller as a 400 against a field they may have sent correctly.
    ///
    /// **It cannot fire for a self-contradicting submission**: the projection
    /// checks `entry_type` against `reason_code` first, so an `invalidation`
    /// stating no reason is already gone as a 400
    /// ([`Self::reason_code_required_on_invalidation`]), and a `record` does
    /// not satisfy this guard whatever reason code it carries. What is left is
    /// a projection chosen wrongly, and nothing else.
    #[must_use]
    pub fn withdrawal_needs_its_target() -> Self {
        Self::internal(
            "try_into_usage_record was handed a submission declaring entry_type invalidation; a \
             withdrawal is projected with try_into_invalidation_record and its resolved target",
        )
    }

    /// [`crate::CreateUsageRecord::try_into_invalidation_record`] was handed
    /// a submission that declares `entry_type: record`.
    ///
    /// Such a submission carries no reason code — the projection has already
    /// checked the two against each other — and a reason code is what a
    /// withdrawal states beside its declared kind.
    ///
    /// **[`Self::Internal`] on the same grounds as
    /// [`Self::withdrawal_needs_its_target`], and on the same narrowed
    /// precondition.** A body declaring `entry_type: invalidation` with no
    /// `reason_code` is already a 400
    /// ([`Self::reason_code_required_on_invalidation`]) before the projection
    /// reaches this, so the only submission that gets here consistently
    /// declares a `record` — the gateway picked the withdrawal projection for
    /// a measurement, which is not the emitter's to answer for.
    #[must_use]
    pub fn missing_reason_code() -> Self {
        Self::internal(
            "try_into_invalidation_record was handed a submission declaring entry_type record; \
             the gateway reads entry_type before it chooses a projection",
        )
    }

    /// A submission declared `entry_type: invalidation` and stated no reason
    /// code. `field` is `reason_code`.
    ///
    /// DESIGN §3.1's "Entry type and reason code" row requires one on that
    /// branch, and `CreateUsageRecordRequest`'s `invalidation` branch lists
    /// `reason_code` among its required properties.
    ///
    /// **[`Self::InvalidArgument`], not [`Self::Internal`]** — unlike
    /// [`Self::missing_reason_code`], which reports the same field missing.
    /// Both halves of the contradiction are caller-supplied (DESIGN §3.1,
    /// Field ownership), so this is an emitter error whichever projection it
    /// is handed to. The declared kind is taken as what they meant, so the
    /// violation is attributed to the half they have to add.
    #[must_use]
    pub fn reason_code_required_on_invalidation() -> Self {
        Self::newtype_validation(
            "reason_code",
            "reason_code is required when entry_type is invalidation",
        )
    }

    /// A submission declared `entry_type: record` and stated a reason code.
    /// `field` is `reason_code`.
    ///
    /// DESIGN §3.1's "Entry type and reason code" row: `reason_code` MUST
    /// NOT appear on a `record`, which the published schema spells as
    /// `reason_code: false` on the `record` branch of
    /// `CreateUsageRecordRequest`.
    ///
    /// Refused rather than reconciled, because both ways of reconciling it
    /// are worse. Dropping the reason admits a withdrawal as an ordinary
    /// measurement under `entry_type = record`, colliding it with the entry
    /// it meant to withdraw; reading the kind off the reason instead is the
    /// inference DESIGN §3.1 forbids outright. The violation is attributed
    /// to `reason_code` because the declared kind is taken as what the
    /// emitter meant, so that is the half to remove.
    ///
    /// **[`Self::InvalidArgument`], not [`Self::Internal`]** — unlike
    /// [`Self::withdrawal_needs_its_target`], which fires on the declared
    /// `entry_type` and attributes no field at all. See
    /// [`Self::reason_code_required_on_invalidation`] for the line between
    /// the two.
    #[must_use]
    pub fn reason_code_forbidden_on_record() -> Self {
        Self::newtype_validation(
            "reason_code",
            "reason_code must not appear when entry_type is record; an entry that withdraws \
             another declares entry_type invalidation",
        )
    }

    /// An attribution component exceeded its character cap. `field` is the
    /// component's dotted path, e.g. `resource_ref.resource_id`.
    #[must_use]
    pub fn attribution_too_long(field: &str, max: usize) -> Self {
        Self::newtype_validation(field, format!("{field} must be at most {max} characters"))
    }

    /// Ingestion supplied a metadata key not declared in the referenced
    /// meter's resolved closed `metadata_fields`. Attributed to the record
    /// resource, with `resource_name` carrying the offending `gts_type_id`.
    #[must_use]
    pub fn unknown_metadata_key(gts_type_id: &MeterTypeId, key: &str) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: Some(gts_type_id.as_ref().to_owned()),
            field: "metadata".to_owned(),
            reason: ValidationReason::UnknownMetadataKey,
            detail: format!("unknown metadata key '{key}' for meter {gts_type_id}"),
        }
    }

    /// A `$filter` predicate named a field reserved to a typed parameter
    /// (`gts_type_id`, or the covered-period bounds `window_start` /
    /// `window_end`). Each already travels as a typed parameter alongside
    /// `$filter`, so a predicate naming one in `$filter` would express a
    /// second, possibly contradictory, constraint on something already
    /// fixed — rejected rather than silently honored. Attributed to
    /// `$filter`.
    #[must_use]
    pub fn reserved_filter_field(field: &str) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "$filter".to_owned(),
            reason: ValidationReason::Validation,
            detail: format!(
                "'{field}' is reserved and cannot be named in $filter: it travels \
                 as a typed parameter, not a filterable property"
            ),
        }
    }

    /// A `$filter` predicate named a field the public contract does not
    /// publish — the generated filter schema declares three more than the
    /// published eight (`id` and the two covered-period bounds) for reasons
    /// unrelated to `$filter` admissibility. Distinct from
    /// [`Self::reserved_filter_field`] on purpose: a reserved field travels as
    /// a typed parameter and the caller can move the predicate there, while
    /// this one is simply not on the surface. Attributed to `$filter`.
    ///
    /// The set is rendered **from** [`crate::PUBLISHED_FILTER_FIELDS`], the
    /// same constant the gear's guard tests against, rather than respelled
    /// here — as [`Self::inadmissible_order_key`] renders
    /// [`crate::KEYSET_SAFE_RECORD_FIELDS`] — so the message cannot outlive
    /// the guard it describes.
    #[must_use]
    pub fn unpublished_filter_field(field: &str) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "$filter".to_owned(),
            reason: ValidationReason::Validation,
            detail: format!(
                "'{field}' is not a filterable field: $filter admits one of {:?}",
                crate::PUBLISHED_FILTER_FIELDS,
            ),
        }
    }

    /// A `$filter` predicate compared a closed-label field (`entry_type`,
    /// `origin`) against a literal outside its label set.
    ///
    /// Two columns, two different wrong answers absent this check:
    /// `entry_type` is a `PostgreSQL` enum the plugin casts the literal to, so
    /// an off-label value raised `22P02` and reached the caller as a
    /// server-class error; `origin` is `text` with a `CHECK` constraint, so an
    /// off-label value silently answered an empty page. Both are caller errors
    /// and both now draw this `InvalidArgument`, attributed to `$filter` like
    /// its siblings [`Self::reserved_filter_field`] and
    /// [`Self::unpublished_filter_field`].
    ///
    /// `labels` is rendered from the SDK's own closed sets
    /// ([`crate::EntryType::wire_labels`] / [`crate::RecordOrigin::wire_labels`])
    /// rather than respelled at the call site, for
    /// [`Self::unpublished_filter_field`]'s reason.
    #[must_use]
    pub fn inadmissible_filter_literal(field: &str, value: &str, labels: &[&str]) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "$filter".to_owned(),
            reason: ValidationReason::Validation,
            detail: format!(
                "'{value}' is not an admissible {field} label: {field} admits one of {labels:?}"
            ),
        }
    }

    /// A `metadata_filter` carried more predicates than the published cap of
    /// 16 (`usage-collector-v1.yaml`'s `POST /records/aggregate` operation
    /// description — prose, not `maxItems`). `actual` counts entries in the
    /// caller's slice, which may exceed the number of distinct keys when one
    /// key is named twice: slightly stricter than counting distinct keys, and
    /// the correct bound, since entries are what size the fingerprint and the
    /// SQL. Attributed to `metadata`, the wire field the REST handler used for
    /// this cap. No `gts_type_id`: the cap is identical for every meter.
    ///
    /// `detail` says "predicates", not "distinct", because the count is over
    /// entries.
    #[must_use]
    pub fn too_many_metadata_filters(actual: usize, cap: usize) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "metadata".to_owned(),
            reason: ValidationReason::Validation,
            detail: format!("{actual} metadata filter predicates exceeds cap {cap}"),
        }
    }

    /// One `metadata_filter` predicate carried more values than the
    /// published `maxItems: 32` on `MetadataFilter.values`. Attributed to
    /// `metadata.<key>`, the wire field the REST handler already used for
    /// this cap.
    #[must_use]
    pub fn too_many_metadata_filter_values(key: &str, actual: usize, cap: usize) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: format!("metadata.{key}"),
            reason: ValidationReason::Validation,
            detail: format!("{actual} values on `metadata.{key}` exceeds cap {cap}"),
        }
    }

    /// A caller order mixed sort directions across its keys. The keyset
    /// continuation is a row-value tuple comparison, which only composes
    /// over a single direction, so a mixed-direction order can never
    /// become a usable keyset — it is refused rather than forwarded to a
    /// plugin that would reject it late and unspecifically. Attributed to
    /// `$orderby`, the surface a caller names an order on, and naming the
    /// first key whose direction deviates so the caller can see which of
    /// their keys to flip.
    #[must_use]
    pub fn mixed_direction_order(deviating_field: &str) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "$orderby".to_owned(),
            reason: ValidationReason::Validation,
            detail: format!(
                "order keys must all share one sort direction, but \
                 '{deviating_field}' sorts against the first key: keyset \
                 pagination is a row-value tuple comparison and cannot \
                 compose a mixed-direction tuple"
            ),
        }
    }

    /// A caller order named a key that is not sound to paginate on — a
    /// domain-optional attribute, one derived from an optional attribute, or a
    /// name that is not a record attribute at all. Every caller key leads the
    /// effective keyset, and a row-value tuple whose leading column is NULL
    /// compares as NULL, so NULL-keyed rows would silently drop out of the
    /// page. The classification is [`crate::is_keyset_safe_record_field`], a
    /// fail-closed allowlist — hence the same rejection for an unknown name.
    /// Attributed to `$orderby`, and naming the whole admissible set
    /// ([`crate::KEYSET_SAFE_RECORD_FIELDS`]).
    #[must_use]
    pub fn inadmissible_order_key(field: &str) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "$orderby".to_owned(),
            reason: ValidationReason::Validation,
            detail: format!(
                "order key '{field}' is not supported: keyset pagination needs a \
                 record attribute present on every entry in its own right, so an \
                 optional or derived attribute, or an unrecognised name, is \
                 refused; order by one of {:?}",
                crate::KEYSET_SAFE_RECORD_FIELDS,
            ),
        }
    }

    /// A caller order named one key more than once.
    ///
    /// Nothing downstream dedups an order: neither the keyset-safety check,
    /// nor `toolkit_odata::ODataOrderBy::ensure_tiebreaker` (which only
    /// *skips* a field already named), nor `ODataOrderBy::from_signed_tokens`.
    /// A repeated key would therefore ride all the way to a minted keyset
    /// carrying one boundary value, and one signed token, per repetition —
    /// `$orderby=resource_id,resource_id,resource_id` alone exceeds
    /// `crate::keyset::MAX_KEYSET_BYTES` before the canonical tiebreaker is
    /// appended, breaching the cursor's published `maxLength: 4096`.
    ///
    /// **The ground is that published cursor bound, not a semantic
    /// order-uniqueness rule**, and this detail deliberately does not claim
    /// one. Attributed to `$orderby`, naming the repeated key.
    #[must_use]
    pub fn duplicate_order_key(field: &str) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "$orderby".to_owned(),
            reason: ValidationReason::Validation,
            detail: format!(
                "order key '{field}' is named more than once: a repeated key widens the \
                 keyset the server would have to mint without narrowing the sort, and risks \
                 the cursor's published 4096-character bound; name each key at most once"
            ),
        }
    }

    /// A continuation token's bound order is not a usable keyset.
    ///
    /// A cursor request carries its keyset in the token, so by the time the
    /// read path sees it there is nothing left to normalize: the order
    /// either already is the keyset the page was minted under, or the token
    /// did not come from a conforming plugin. Appending a missing key would
    /// leave the order wider than the boundary values the token carries and
    /// hand the plugin a misaligned continuation — a silently wrong page,
    /// where refusing is merely a refused one.
    ///
    /// The wire code is not chosen here: this carries `toolkit_odata`'s own
    /// `InvalidCursor` and the host lift converts it (Spec §3.13). The wire
    /// field is [`CursorField::Cursor`], chosen here, upstream having no
    /// notion of which cursor-bearing parameter is meant.
    ///
    /// The detail names the defect and the recovery and deliberately does not
    /// blame a component: a token can be forged, truncated or replayed by the
    /// caller as easily as mis-minted by a plugin, and the caller can act on
    /// neither hypothesis.
    #[must_use]
    pub fn inadmissible_cursor_keyset(defect: impl Into<String>) -> Self {
        let defect = defect.into();
        Self::CursorRejected {
            source: toolkit_odata::Error::InvalidCursor,
            detail: format!(
                "the cursor's bound order is not a usable keyset ({defect}); \
                 restart pagination without a cursor"
            ),
            field: CursorField::Cursor,
        }
    }

    /// A continuation token names a direction this read path does not serve.
    ///
    /// A sibling of [`Self::inadmissible_cursor_keyset`], not a reuse of it:
    /// by the time the raw path's `d: "bwd"` refusal runs, the token's order
    /// has already been bound and found a sound keyset, and what is wrong is
    /// `cursor.d` alone. Reusing the keyset wrapper there told the caller
    /// their order was unusable when it was not.
    ///
    /// Same wire code and field as [`Self::inadmissible_cursor_keyset`], for
    /// the reasons that constructor gives.
    #[must_use]
    pub fn unsupported_cursor_direction(defect: impl Into<String>) -> Self {
        let defect = defect.into();
        Self::CursorRejected {
            source: toolkit_odata::Error::InvalidCursor,
            detail: format!(
                "the cursor's direction is not supported ({defect}); restart pagination \
                 without a cursor"
            ),
            field: CursorField::Cursor,
        }
    }

    /// A feed `cursor` this gear will not continue.
    ///
    /// The feed's twin of [`Self::inadmissible_until_keyset`], and the one the
    /// `cursor` half of the feed's decode path wants rather than
    /// [`Self::inadmissible_cursor_keyset`]: a feed cursor carries no caller
    /// order at all (DESIGN §3.1, Order admissibility), so its `SortDir` is
    /// inert and every defect reported here is about the token itself — too
    /// long, not a feed cursor, not one position, or a position the plugin
    /// could not have issued.
    ///
    /// Same wire code and field as the raw path's, for the reasons
    /// [`Self::inadmissible_cursor_keyset`] gives.
    #[must_use]
    pub fn inadmissible_feed_cursor(defect: impl Into<String>) -> Self {
        let defect = defect.into();
        Self::CursorRejected {
            source: toolkit_odata::Error::InvalidCursor,
            detail: format!(
                "the `cursor` is not a usable feed cursor ({defect}); \
                 resend the request without `cursor` to restart the subscription from its \
                 oldest retained position, or with one this feed issued"
            ),
            field: CursorField::Cursor,
        }
    }

    /// As [`Self::inadmissible_cursor_keyset`], for the feed's `until` bound.
    ///
    /// The `until` parameter is validated on exactly the same terms as
    /// `cursor` and differs only in which parameter the violation names
    /// (`usage-collector-v1.yaml`, `Until`).
    #[must_use]
    pub fn inadmissible_until_keyset(defect: impl Into<String>) -> Self {
        let defect = defect.into();
        Self::CursorRejected {
            source: toolkit_odata::Error::InvalidCursor,
            detail: format!(
                "the `until` bound is not a usable feed cursor ({defect}); \
                 resend the request without `until`, or with one this feed issued"
            ),
            field: CursorField::Until,
        }
    }

    /// A continuation token was minted over a different query than the
    /// request carrying it.
    ///
    /// `CursorV1::f` exists so a caller who changes their query between pages
    /// is refused rather than served a keyset continuation that means nothing
    /// over their new row set — and a wrong page is a `200`, so nothing else
    /// in the stack would notice. The bound query is every input that selects
    /// rows: the caller's `$filter` plus the three typed parameters, none of
    /// which is a `$filter` conjunct, so each enters the fingerprint
    /// explicitly.
    ///
    /// This carries `toolkit_odata`'s own `FilterMismatch` and the gear spells
    /// the wire field itself (Spec §3.13). Reusing upstream's filter-hash
    /// comparison is also the right classification: a changed range, meter or
    /// metadata filter is a changed query from the caller's side, and a code
    /// per typed parameter would split one caller-visible condition across
    /// four.
    ///
    /// The detail names the recovery rather than the mismatching value: the
    /// fingerprint is opaque.
    #[must_use]
    pub fn cursor_query_mismatch() -> Self {
        Self::CursorRejected {
            source: toolkit_odata::Error::FilterMismatch,
            detail: "the cursor was minted over a different query: continue a page by \
                     resending the same request, cursor apart, or restart pagination \
                     without a cursor. The cursor binds `gts_type_id`, the `from` / \
                     `to` range, `$filter` and every `metadata.<key>` filter, so \
                     changing any of them invalidates it"
                .to_owned(),
            field: CursorField::Cursor,
        }
    }

    /// A feed `cursor` minted over a different subscription than the
    /// request carrying it.
    ///
    /// The feed's twin of [`Self::until_query_mismatch`], and the one the
    /// `cursor` half of the feed's fingerprint check wants rather than
    /// [`Self::cursor_query_mismatch`]: a feed cursor binds the
    /// subscription's GTS type set (DESIGN §3.3), and the feed admits none of
    /// the inputs the raw path's detail enumerates, so naming them to a feed
    /// consumer would send them looking for a parameter they could not have
    /// sent.
    ///
    /// Same `FilterMismatch` source as the raw path's, and for the same
    /// reason (Spec §3.13).
    #[must_use]
    pub fn cursor_subscription_mismatch() -> Self {
        Self::CursorRejected {
            source: toolkit_odata::Error::FilterMismatch,
            detail: "the cursor was minted over a different subscription: resend it with the \
                     same `gts_type_id` set the cursor was issued under, or restart the \
                     subscription without a cursor"
                .to_owned(),
            field: CursorField::Cursor,
        }
    }

    /// As [`Self::cursor_query_mismatch`], for the feed's `until` bound.
    #[must_use]
    pub fn until_query_mismatch() -> Self {
        Self::CursorRejected {
            source: toolkit_odata::Error::FilterMismatch,
            detail: "the `until` bound was minted over a different subscription: resend it \
                     with the same `gts_type_id` set the bound was issued under, or drop \
                     `until` to read the live feed"
                .to_owned(),
            field: CursorField::Until,
        }
    }

    /// A `group_by` dimension named a metadata key the queried meter's
    /// resolved declaration does not declare. The admissible `group_by`
    /// surface is recomputed per request from the declaration (Spec §3.11),
    /// so this can never be satisfied by adjusting a stale cache — only by
    /// naming a key the declaration actually carries. Attributed to
    /// `group_by`, with `resource_name` carrying the offending
    /// `gts_type_id` — the same operator-log shape as
    /// [`Self::unknown_metadata_key`].
    #[must_use]
    pub fn undeclared_metadata_dimension(gts_type_id: &MeterTypeId, key: &str) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: Some(gts_type_id.as_ref().to_owned()),
            field: "group_by".to_owned(),
            reason: ValidationReason::UnknownMetadataKey,
            detail: format!(
                "unknown metadata key '{key}' in group_by for meter {gts_type_id}: not declared"
            ),
        }
    }

    /// A `group_by` list naming one dimension more than once.
    ///
    /// DESIGN's `AggregationDimension` row admits each dimension "each at most
    /// once" and the yaml declares `uniqueItems: true` on `group_by`.
    /// Attributed to `group_by`, with `resource_name` carrying the offending
    /// `gts_type_id` — the same operator-log shape as
    /// [`Self::undeclared_metadata_dimension`]. Deliberately not
    /// [`ValidationReason::MetadataFieldDuplicate`]: that variant's wire
    /// string names the meter type's `metadata_fields` *declaration*, a
    /// different surface, and would be wrong for a repeated fixed dimension
    /// such as `tenant_id`. No new published reason code is minted either,
    /// which would widen the wire contract; the specifics live in `detail`.
    #[must_use]
    pub fn repeated_grouping_dimension(
        gts_type_id: &MeterTypeId,
        dimension: &AggregationDimension,
    ) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: Some(gts_type_id.as_ref().to_owned()),
            field: "group_by".to_owned(),
            reason: ValidationReason::Validation,
            detail: format!(
                "grouping dimension `{}` appears more than once in group_by for meter \
                 {gts_type_id}: each admissible dimension may be named at most once, in any \
                 combination and any order",
                dimension_wire_name(dimension),
            ),
        }
    }

    /// An invalidation departed from its target in a field it must copy.
    ///
    /// `field` names **the field that differs**, which is the whole point
    /// of the diagnostic: the entry is a faithful copy in every
    /// caller-supplied field, departing in exactly two — `entry_type` and
    /// `reason_code` — so a rejection that only said "mismatch" would leave
    /// the emitter diffing two payloads by hand.
    #[must_use]
    pub fn invalidation_field_mismatch(field: &str, target: Uuid) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: Some(target.to_string()),
            field: field.to_owned(),
            reason: ValidationReason::InvalidationFieldMismatch,
            detail: format!(
                "`{field}` differs from usage record {target}; an invalidation copies every \
                 caller-supplied field of the entry it withdraws"
            ),
        }
    }

    // ── NotFound (404) ──────────────────────────────────────────────────

    /// A lookup referenced a `UsageRecord.id` that does not exist.
    #[must_use]
    pub fn usage_record_not_found(id: Uuid) -> Self {
        Self::NotFound {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            name: id.to_string(),
            reason: NotFoundReason::UsageRecordNotFound,
            detail: format!("usage record not found: {id}"),
        }
    }

    /// The target an invalidation locates through its own fields resolved to
    /// nothing.
    ///
    /// `NotFound` rather than a conflict: the ledger holds no entry under the
    /// derived identifier. `name` carries that identifier, and
    /// [`NotFoundReason::InvalidationTargetNotFound`] separates this
    /// in-process from the other `NotFound` a submission can raise — a wire
    /// client still has only `detail` to tell the cases apart.
    ///
    /// **The detail names the four locating inputs, not a field**, as DESIGN
    /// §3.1's Target resolution row requires: a typo in any of them surfaces
    /// here rather than as a field mismatch, and naming `invalidates` instead
    /// would send the emitter hunting for a property
    /// [`crate::CreateUsageRecord`] does not have.
    #[must_use]
    pub fn invalidation_target_not_found(target: Uuid) -> Self {
        Self::NotFound {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            name: target.to_string(),
            reason: NotFoundReason::InvalidationTargetNotFound,
            detail: format!(
                "no usage record matches this invalidation's target, derived as {target}: \
                 the target is identified by tenant, GTS type, idempotency key, and covered \
                 period, all read from this entry's own fields, so a typo in any of the four \
                 surfaces here rather than as a field mismatch"
            ),
        }
    }

    // ── Conflict / Aborted (409) ────────────────────────────────────────

    /// A second invalidation of a record under a reason code other than the
    /// stored one: the dedup conflict of an invalidation. Every invalidation of
    /// one record repeats that record's tenant, type, key and period and reads
    /// `entry_type = invalidation`, so all of them share one dedup identity and
    /// the store reports an ordinary `IdempotencyConflict`; the gateway, knowing
    /// the dispatched entry is an invalidation, reports it as this. `name` is
    /// the target; `invalidated_by` and `reason_code` name the invalidation in
    /// place.
    #[must_use]
    pub fn already_invalidated(args: AlreadyInvalidatedArgs) -> Self {
        let AlreadyInvalidatedArgs {
            target,
            invalidated_by,
            reason_code,
        } = args;
        Self::Conflict {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            name: target.to_string(),
            detail: format!(
                "usage record {target} is already invalidated by {invalidated_by} with reason code {}",
                reason_code.as_str()
            ),
            outcome: ConflictOutcome::AlreadyInvalidated {
                invalidated_by,
                reason_code,
            },
        }
    }

    /// Same `idempotency_key`, canonical-field-different payload. `name`
    /// carries the previously persisted record id so the caller can
    /// reconcile.
    #[must_use]
    pub fn idempotency_conflict(idempotency_key: &str, existing_id: Uuid) -> Self {
        Self::Conflict {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            name: existing_id.to_string(),
            detail: format!(
                "idempotency key {idempotency_key} already bound to record {existing_id}"
            ),
            outcome: ConflictOutcome::IdempotencyConflict,
        }
    }

    /// An invalidation's target has not converged under the active plugin's
    /// dedup level. `name` is the target. Retryable through the wire
    /// `context.retryable = true` **and** through [`Self::retry_after`]
    /// (DESIGN §3.3), though [`Self::is_retryable`] stays true for
    /// `ServiceUnavailable` alone. `retry_after_seconds` is `None` unless the
    /// host supplies the configured `target_not_converged_retry_after_secs`
    /// delay (DESIGN §3.8), which its error lift does.
    #[must_use]
    pub fn target_not_converged(target: Uuid, retry_after_seconds: Option<u64>) -> Self {
        Self::Conflict {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            name: target.to_string(),
            detail: format!(
                "this invalidation's target, derived as {target}, names an entry whose \
                 dedup identity has not converged yet; retry"
            ),
            outcome: ConflictOutcome::TargetNotConverged {
                retry_after_seconds,
            },
        }
    }

    // ── ServiceUnavailable (503) ────────────────────────────────────────

    /// No scoped storage-plugin client was available (host-structural
    /// readiness) — a dispatch that reached no plugin at all (DESIGN §3.8).
    /// `retry_after_seconds` is the configured `unavailable_retry_after_secs`
    /// delay where the host supplies one; this crate carries no config of
    /// its own, so a caller building one directly (as the blanket
    /// `DomainError` -> `UsageCollectorError` conversion does, with no config
    /// in scope) gets `None`.
    #[must_use]
    pub fn plugin_unavailable(retry_after_seconds: Option<u64>) -> Self {
        Self::service_unavailable("storage plugin unavailable", retry_after_seconds)
    }

    /// The `types-registry` lookup the host uses to bind the scoped client
    /// returned an unavailable result. `retry_after_seconds` is the same
    /// configured default [`Self::plugin_unavailable`] takes, for the same
    /// reason: this is the other half of "a dispatch that reached no plugin
    /// at all" (DESIGN §3.8).
    #[must_use]
    pub fn types_registry_unavailable(retry_after_seconds: Option<u64>) -> Self {
        Self::service_unavailable("types-registry unavailable", retry_after_seconds)
    }

    /// Plugin-reported transient / PDP-transport outage. `detail` is curated
    /// for operator triage; `retry_after_seconds` is an optional hint.
    #[must_use]
    pub fn service_unavailable(
        detail: impl Into<String>,
        retry_after_seconds: Option<u64>,
    ) -> Self {
        Self::ServiceUnavailable {
            retry_after_seconds,
            detail: detail.into(),
        }
    }

    // ── ResourceExhausted (429) ─────────────────────────────────────────

    /// Per-subject ingestion allowance exhausted.
    #[must_use]
    pub fn ingestion_quota_exceeded(args: IngestionQuotaExceededArgs) -> Self {
        let IngestionQuotaExceededArgs {
            allowance,
            submitted,
            retry_after_seconds,
        } = args;
        Self::ResourceExhausted {
            retry_after_seconds,
            detail: format!("submitted {submitted} entries against an allowance of {allowance}"),
        }
    }

    /// Unclassified failure. `detail` MUST be DSN-free / pre-redacted.
    #[must_use]
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::Internal {
            detail: detail.into(),
        }
    }

    /// `true` for [`Self::ServiceUnavailable`] alone — plugin `Transient`,
    /// host readiness, and PDP-transport failures all lift there. **Not the
    /// complete retry decision** (DESIGN §3.3 Error Contract states this in
    /// those words): [`Self::Conflict`] carrying
    /// [`crate::reason::ConflictReason::TargetNotConverged`] is also
    /// retryable, through the wire `context.retryable = true` rather than
    /// through this predicate. [`Self::retry_after`] is the one query a
    /// retry-aware caller should actually dispatch on.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::ServiceUnavailable { .. })
    }

    /// The delay before retrying is worthwhile, for every outcome DESIGN
    /// §3.3 Error Contract names retryable — `ServiceUnavailable`,
    /// `ResourceExhausted`, and `Conflict(TargetNotConverged)` — and `None`
    /// for every other classification. This is "the complete retry
    /// decision" DESIGN describes: a caller dispatches on this rather than
    /// on [`Self::is_retryable`], which only ever answers for
    /// `ServiceUnavailable`.
    ///
    /// `ServiceUnavailable` and `Conflict(TargetNotConverged)` answer `None`
    /// from a bare, config-free construction: this crate carries no
    /// configuration of its own and cannot synthesize a delay it was never
    /// given.
    ///
    /// **The host supplies one, which is why this reads `Some` in practice.**
    /// DESIGN §3.8 declares `unavailable_retry_after_secs` and
    /// `target_not_converged_retry_after_secs` as deployment configuration,
    /// and the host threads both through its `DomainError` ->
    /// `UsageCollectorError` lift (`usage-collector/src/domain/error.rs`), so
    /// a hintless plugin `Transient`, a dispatch that reached no plugin, and a
    /// bare `TargetNotConverged` all resolve to `Some(configured_default)` by
    /// the time the gear answers. The host also stamps the resolved delay onto
    /// the wire `context.retry_after_seconds`, so a REST caller reads what an
    /// in-process caller reads here. This crate's own constructors take the
    /// delay as a plain `Option<u64>` rather than a config object.
    #[must_use]
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            Self::ServiceUnavailable {
                retry_after_seconds: Some(seconds),
                ..
            }
            | Self::ResourceExhausted {
                retry_after_seconds: seconds,
                ..
            }
            | Self::Conflict {
                outcome:
                    ConflictOutcome::TargetNotConverged {
                        retry_after_seconds: Some(seconds),
                    },
                ..
            } => Some(std::time::Duration::from_secs(*seconds)),
            _ => None,
        }
    }
}

/// Plugin-side error vocabulary returned by every
/// [`crate::plugin_api::UsageCollectorPluginV1`] method.
///
/// Translated into [`UsageCollectorError`] by the host at the dispatch
/// boundary, routed through the host-internal domain-error vocabulary (this
/// crate intentionally provides no direct `From` to the public envelope).
/// Structural unavailability is host-side and surfaces as a
/// [`UsageCollectorError::ServiceUnavailable`], not as a plugin error.
///
/// Plugins classify a failure into one of three non-domain buckets:
///
/// - [`Self::Transient`] — retryable backend failure (downstream timeout,
///   connection reset, upstream 5xx). Lifts to
///   [`UsageCollectorError::ServiceUnavailable`].
/// - [`Self::Internal`] — non-retryable unclassified failure (plugin
///   invariant broken, uncategorized backend error). Lifts to
///   [`UsageCollectorError::Internal`].
/// - The record variants below — typed domain outcomes.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum UsageCollectorPluginError {
    /// Retryable backend failure — safe to retry (downstream timeout,
    /// connection reset, upstream 5xx). Lifts to
    /// [`UsageCollectorError::ServiceUnavailable`] at the dispatch
    /// boundary, forwarding the optional `retry_after_seconds` hint, and
    /// is observed as retryable by [`UsageCollectorError::is_retryable`].
    #[error("transient plugin error: {detail}")]
    Transient {
        /// Operator-facing detail (DSN-free, pre-redacted at the plugin).
        detail: String,
        /// Optional retry-after hint (seconds) forwarded onto the
        /// `ServiceUnavailable` envelope's `Retry-After` slot. Plugins
        /// that have no actionable hint pass `None`.
        retry_after_seconds: Option<u64>,
    },

    /// Idempotency conflict at the persistence boundary: an entry with this
    /// dedup identity is already stored, and its caller-supplied fields differ
    /// from the submission's ([`crate::models::UsageRecord::caller_supplied_eq`]).
    /// `existing` is the stored entry, so the gateway can name it, and when the
    /// dispatched entry is an invalidation, report the conflict as
    /// `AlreadyInvalidated` naming `existing.id` and its reason code.
    ///
    /// The entry is reference-keyed like every other record crossing this
    /// SPI. The gateway re-attaches the identifier from the submission that
    /// conflicted — a conflict shares the submission's dedup identity, so the
    /// two name one meter — **after** checking that the two references agree.
    #[error("idempotency conflict: key {idempotency_key} already bound to record {}", .existing.id)]
    IdempotencyConflict {
        /// The dispatched entry's idempotency key. Caller-supplied on both
        /// entry kinds: an invalidation repeats its target's.
        idempotency_key: String,
        /// The stored entry the key is already bound to.
        existing: Box<crate::stored::StoredUsageRecord>,
    },

    /// A lookup referenced a `UsageRecord.id` the store does not hold —
    /// `get_usage_record`, or the target of a submitted invalidation.
    #[error("usage record not found: {id}")]
    UsageRecordNotFound {
        /// The `UsageRecord.id` the lookup asked for. Caller-supplied on
        /// `get_usage_record`; on an invalidation's target lookup it is
        /// derived from the withdrawal's own identity inputs with
        /// `entry_type = record`, never sent
        /// (`cpt-cf-usage-collector-adr-record-identity-derivation`).
        id: Uuid,
    },

    /// A converged-only lookup cannot yet decide whether `id` exists: its
    /// identity has not converged under the plugin's dedup level. Only
    /// `get_usage_record(.., converged_only = true)` may answer it. The
    /// gateway lifts it to a retryable `Conflict(TargetNotConverged)`.
    #[error("usage record not converged: {id}")]
    UsageRecordNotConverged {
        /// The `UsageRecord.id` the lookup named.
        id: Uuid,
    },

    /// A cursor names a position after which retention has already removed an
    /// entry of a subscribed GTS type, so the continuation cannot be served
    /// whole.
    ///
    /// The plugin decides this from what it still holds, not from the cursor's
    /// own age — a sweep clamps that age to the retention boundary, so an age
    /// test would serve a silently truncated range. `DESIGN.md` §3.3 lifts it
    /// to [`UsageCollectorError::InvalidArgument`] carrying
    /// [`crate::reason::ValidationReason::CursorBeyondRetention`] against the
    /// `cursor` field.
    ///
    /// Carries no detail, matching `DESIGN.md` §3.3's table: the
    /// operator-facing description is the gateway's to author, the refusal
    /// being a caller-actionable argument fault rather than a backend failure.
    #[error("the cursor names a position retention has already passed")]
    CursorBeyondRetention,

    /// Non-retryable unclassified plugin-side failure (plugin invariant
    /// broken, uncategorized backend error). Use [`Self::Transient`] for
    /// retryable backend errors.
    #[error("plugin internal error: {0}")]
    Internal(String),
}

impl UsageCollectorPluginError {
    /// Constructs a [`UsageCollectorPluginError::IdempotencyConflict`] against
    /// the stored entry.
    #[must_use]
    pub fn idempotency_conflict(
        idempotency_key: impl Into<String>,
        existing: crate::stored::StoredUsageRecord,
    ) -> Self {
        Self::IdempotencyConflict {
            idempotency_key: idempotency_key.into(),
            existing: Box::new(existing),
        }
    }

    /// Constructs a [`UsageCollectorPluginError::Internal`].
    #[must_use]
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::Internal(detail.into())
    }

    /// Constructs a [`UsageCollectorPluginError::Transient`] with no retry
    /// hint. Plugins that know a sensible delay should use
    /// [`Self::transient_with_retry`] instead.
    #[must_use]
    pub fn transient(detail: impl Into<String>) -> Self {
        Self::Transient {
            detail: detail.into(),
            retry_after_seconds: None,
        }
    }

    /// Constructs a [`UsageCollectorPluginError::Transient`] carrying an
    /// optional retry hint. The hint is forwarded to the
    /// [`UsageCollectorError::ServiceUnavailable`] envelope at the
    /// dispatch boundary.
    #[must_use]
    pub fn transient_with_retry(
        detail: impl Into<String>,
        retry_after_seconds: Option<u64>,
    ) -> Self {
        Self::Transient {
            detail: detail.into(),
            retry_after_seconds,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "error_tests.rs"]
mod error_tests;
