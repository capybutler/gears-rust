//! Usage Collector SDK error types.
//!
//! Two `thiserror::Error` enums make up the SDK's error vocabulary:
//!
//! - [`UsageCollectorError`] — public envelope returned by every
//!   [`crate::api::UsageCollectorClientV1`] method. A flat, AIP-193-shaped
//!   set of **seven category variants**: the discriminator
//!   inside a category is a typed [`crate::reason`] sub-enum
//!   ([`ValidationReason`] / [`ConflictReason`]) rather than a dedicated
//!   variant per failure.
//! - [`UsageCollectorPluginError`] — plugin-side vocabulary returned by
//!   every [`crate::plugin_api::UsageCollectorPluginV1`] method.
//!
//! This crate does NOT depend on `toolkit-canonical-errors`; the host crate
//! owns the lift to RFC-9457 `Problem` at the REST boundary. The category +
//! typed reason + `resource_type` carried here are exactly what the lift
//! projects onto the canonical envelope, so callers dispatch on the variant
//! (and, within a category, the typed reason) rather than parsing strings.

use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::gts::USAGE_RECORD_RESOURCE;
use crate::models::{MeterTypeId, WINDOW_END_FIELD};
use crate::reason::{ConflictReason, ValidationReason};

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
/// non-zero seconds, both of which an in-process caller can construct even
/// though no RFC 3339 wire payload can express either. So the fallback is a
/// genuine last resort, not dead code — it trades a panic for a `Display`
/// rendering that is at least self-consistent. Callers reduce how often it
/// can be reached by normalizing to UTC before constructing the error,
/// which is what [`crate::CreateUsageRecord::try_into_usage_record`] does.
fn rfc3339(instant: OffsetDateTime) -> String {
    instant
        .format(&Rfc3339)
        .unwrap_or_else(|_| instant.to_string())
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
        /// Attributed request field (`value`, `records`, `metadata`, …).
        field: String,
        /// Typed `field_violations[0].reason` discriminator.
        reason: ValidationReason,
        /// Wire `field_violations[0].description`.
        detail: String,
    },

    /// Referenced resource not found (HTTP 404). `resource_type` is the GTS
    /// type, `name` the raw identifier (a `gts_type_id` or a record UUID).
    #[error("not found [{resource_type}]: {detail}")]
    NotFound {
        /// GTS resource type — [`USAGE_RECORD_RESOURCE`].
        resource_type: String,
        /// Raw identifier whose row was not present.
        name: String,
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
    /// AIP-193 `Aborted`). `reason` is the typed [`ConflictReason`]
    /// discriminator carried on the wire `context.reason`; `resource_type` /
    /// `name` identify the row involved.
    #[error("conflict [{reason}]: {detail}")]
    Conflict {
        /// GTS resource type — [`USAGE_RECORD_RESOURCE`].
        resource_type: String,
        /// Raw identifier of the row involved (`gts_id` / record UUID).
        name: String,
        /// Typed `context.reason` discriminator.
        reason: ConflictReason,
        /// Wire `detail` message.
        detail: String,
    },

    /// Transient infrastructure unavailability (HTTP 503). Covers
    /// host-structural readiness (plugin / types-registry), plugin-reported
    /// transience, and PDP-transport outages — operator triage reads the
    /// curated `detail` string. Carries an optional `retry_after_seconds`
    /// hint. The only retryable classification ([`Self::is_retryable`]).
    #[error("service unavailable: {detail}")]
    ServiceUnavailable {
        /// Optional retry hint forwarded onto the `Retry-After` slot.
        retry_after_seconds: Option<u64>,
        /// Operator-facing detail (DSN-free, pre-redacted at construction).
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
            // `field` names the range as a whole rather than either bound:
            // both parsed, and the ordering between them is what failed.
            // The constructor is shared by both read paths and cannot know
            // which carrier the caller used, so the detail names both —
            // `field` alone would name nothing a raw-path caller sent.
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
    /// domain-optional attribute, one derived from an optional attribute,
    /// or a name that is not a record attribute at all. Every caller key
    /// leads the effective keyset, and a row-value tuple whose leading
    /// column is NULL compares as NULL, so NULL-keyed rows would silently
    /// drop out of the page; a derived attribute is one the SDK guarantees
    /// no plugin holds a key for. The
    /// classification is [`crate::is_keyset_safe_record_field`], which is
    /// a fail-closed allowlist — hence the same rejection for an unknown
    /// name. Attributed to `$orderby`, and naming the whole admissible set
    /// — [`crate::KEYSET_SAFE_RECORD_FIELDS`] is closed and short, so the
    /// caller is told what they may order by instead of only what they may
    /// not.
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

    /// A continuation token's bound order is not a usable keyset.
    ///
    /// A cursor request carries its keyset in the token, so by the time the
    /// read path sees it there is nothing left to normalize: the order
    /// either already is the keyset the page was minted under, or the token
    /// did not come from a conforming plugin. Appending a missing key would
    /// leave the order wider than the boundary values the token carries and
    /// hand the plugin a misaligned continuation — a silently wrong page,
    /// where refusing is merely a refused one. Attributed to `cursor`, the
    /// parameter the caller actually supplied, with `INVALID_CURSOR` — the
    /// same field and code `toolkit_odata`'s own decode failures use.
    ///
    /// The detail names the defect and the recovery, and deliberately does
    /// not blame a component: a token can be forged, truncated or replayed
    /// by the caller just as easily as mis-minted by a plugin, and the
    /// caller can act on neither hypothesis. The plugin-conformance
    /// reading belongs in the operator log at the refusal site.
    #[must_use]
    pub fn inadmissible_cursor_keyset(defect: impl Into<String>) -> Self {
        let defect = defect.into();
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "cursor".to_owned(),
            reason: ValidationReason::InvalidCursor,
            detail: format!(
                "the cursor's bound order is not a usable keyset ({defect}); \
                 restart pagination without a cursor"
            ),
        }
    }

    /// A continuation token was minted over a different query than the
    /// request carrying it.
    ///
    /// `CursorV1::f` exists so a caller who changes their query between
    /// pages is refused rather than served a keyset continuation that means
    /// nothing over their new row set — and a wrong page is a `200`, so
    /// nothing else in the stack would notice. The bound query is every
    /// input that selects rows: the caller's `$filter` and the three typed
    /// parameters — `gts_type_id`, the read range, and `metadata_filter`.
    /// None of the three is a `$filter` conjunct, so each has to enter the
    /// fingerprint explicitly; otherwise a page-2 request could carry the
    /// same cursor against a different meter, range or metadata filter and
    /// be served.
    ///
    /// Attributed to `cursor` with `FILTER_MISMATCH` — the code
    /// `usage-collector-v1.yaml` already enumerates for that parameter, and
    /// the one `toolkit_odata`'s own filter-hash comparison emits, because
    /// a changed range, meter or metadata filter is a changed query from
    /// the caller's side. Minting a new code per typed parameter would
    /// split one caller-visible condition across four.
    ///
    /// The detail names the recovery rather than the mismatching value: the
    /// fingerprint is opaque and a caller can do nothing with either half
    /// of the comparison.
    #[must_use]
    pub fn cursor_query_mismatch() -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "cursor".to_owned(),
            reason: ValidationReason::FilterMismatch,
            detail: "the cursor was minted over a different query: continue a page by \
                     resending the same request, cursor apart, or restart pagination \
                     without a cursor. The cursor binds `gts_type_id`, the `from` / \
                     `to` range, `$filter` and every `metadata.<key>` filter, so \
                     changing any of them invalidates it"
                .to_owned(),
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

    /// A REST submission carried a reference without a reason code, or the
    /// reverse. The two are both-or-neither
    /// (`cpt-cf-usage-collector-adr-append-only-invalidation`): the
    /// reference is what makes the entry an invalidation, and the reason
    /// carries the intent, so half the pair describes nothing. `field`
    /// names the **missing** half, which is the one the caller has to add.
    ///
    /// Reachable from the REST fold point alone. The domain carries the
    /// pair as one [`crate::Invalidation`], so an in-process caller cannot
    /// construct the shape this rejects, and nothing downstream of the fold
    /// re-checks it — the host's `record_request_into_domain` is its only
    /// caller, raising it from two call sites, one per direction of the
    /// half-shape.
    #[must_use]
    pub fn invalidation_reference_incomplete(missing_field: &str) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: missing_field.to_owned(),
            reason: ValidationReason::InvalidationReferenceIncomplete,
            detail: format!(
                "an invalidation carries both a target reference and a reason code; \
                 `{missing_field}` is missing"
            ),
        }
    }

    /// An invalidation's target was itself an invalidation. A correction
    /// cannot be reversed: withdrawal applies to measurements, so the
    /// entry that withdrew one is not itself withdrawable. The separate
    /// cap of one withdrawal per entry is the store's
    /// ([`Self::already_invalidated`]), not this check's.
    #[must_use]
    pub fn invalidation_target_not_record(target: Uuid) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: Some(target.to_string()),
            field: "invalidates".to_owned(),
            reason: ValidationReason::InvalidationTargetNotRecord,
            detail: format!("invalidates {target} references an invalidation, not a record"),
        }
    }

    /// An invalidation departed from its target in a field it must copy.
    ///
    /// `field` names **the field that differs**, which is the whole point
    /// of the diagnostic: the entry is a faithful copy in every
    /// caller-supplied field, departing only in its own idempotency key,
    /// `invalidates` and `reason_code`, so a rejection that only said
    /// "mismatch" would leave the emitter diffing two payloads by hand.
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
            detail: format!("usage record not found: {id}"),
        }
    }

    /// An invalidation's `invalidates` resolved to nothing.
    ///
    /// `NotFound` rather than a conflict: the reference names an entry the
    /// ledger does not hold. `name` carries the target uuid, which is what
    /// separates this from the other `NotFound` a submission can raise —
    /// an unresolvable meter names a `gts_type_id`, and a `gts_type_id`
    /// never parses as a [`Uuid`] while an entry id always does. A
    /// consumer telling the two apart has only `name` to do it with,
    /// because the category carries no wire `context.reason`; the `detail`
    /// text carries the human distinction.
    #[must_use]
    pub fn invalidation_target_not_found(target: Uuid) -> Self {
        Self::NotFound {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            name: target.to_string(),
            detail: format!("invalidates {target} does not reference an existing usage record"),
        }
    }

    // ── Conflict / Aborted (409) ────────────────────────────────────────

    /// The target already carries an accepted invalidation. Raised from the
    /// store's atomic check, never from a gateway pre-read — a gateway-side
    /// pre-read cannot exclude a concurrent second submission, so it would
    /// be a check that fails exactly when it matters
    /// (`cpt-cf-usage-collector-adr-append-only-invalidation`). `name` is
    /// the target so the caller can look the pair up; `detail` names the
    /// invalidation that already withdrew it.
    #[must_use]
    pub fn already_invalidated(target: Uuid, invalidated_by: Uuid) -> Self {
        Self::Conflict {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            name: target.to_string(),
            reason: ConflictReason::AlreadyInvalidated,
            detail: format!("usage record {target} is already invalidated by {invalidated_by}"),
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
            reason: ConflictReason::IdempotencyConflict,
            detail: format!(
                "idempotency key {idempotency_key} already bound to record {existing_id}"
            ),
        }
    }

    // ── ServiceUnavailable (503) ────────────────────────────────────────

    /// No scoped storage-plugin client was available (host-structural
    /// readiness).
    #[must_use]
    pub fn plugin_unavailable() -> Self {
        Self::ServiceUnavailable {
            retry_after_seconds: None,
            detail: "storage plugin unavailable".to_owned(),
        }
    }

    /// The `types-registry` lookup the host uses to bind the scoped client
    /// returned an unavailable result.
    #[must_use]
    pub fn types_registry_unavailable() -> Self {
        Self::ServiceUnavailable {
            retry_after_seconds: None,
            detail: "types-registry unavailable".to_owned(),
        }
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

    /// Unclassified failure. `detail` MUST be DSN-free / pre-redacted.
    #[must_use]
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::Internal {
            detail: detail.into(),
        }
    }

    /// `true` for retryable classifications — the principal semantic the SDK
    /// exposes to retry-aware callers. Plugin `Transient`, host readiness,
    /// and PDP-transport failures all lift to [`Self::ServiceUnavailable`].
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::ServiceUnavailable { .. })
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

    /// Idempotency conflict at the persistence boundary: the supplied
    /// `idempotency_key` is already bound to a different stored record.
    /// Carries the id of the previously persisted record (the plugin
    /// detects the conflict against a specific row, so the row's id is
    /// the actionable handle for the gateway).
    #[error("idempotency conflict: key {idempotency_key} already bound to record {existing_id}")]
    IdempotencyConflict {
        /// Caller-supplied idempotency key.
        idempotency_key: String,
        /// `UsageRecord.id` of the previously persisted row the key is
        /// already bound to.
        existing_id: Uuid,
    },

    /// A lookup referenced a `UsageRecord.id` the store does not hold —
    /// `get_usage_record`, or the target of a submitted invalidation.
    #[error("usage record not found: {id}")]
    UsageRecordNotFound {
        /// Caller-supplied target `UsageRecord.id`.
        id: Uuid,
    },

    /// A second withdrawal of a record that already carries one. This is
    /// the plugin's **one** invalidation obligation: only the store can
    /// make the check atomic with the entry it admits
    /// (`cpt-cf-usage-collector-adr-append-only-invalidation`). Carries the
    /// existing invalidation's id, so the gateway's rejection can name the
    /// entry that already withdrew the target.
    #[error("usage record {id} is already invalidated by {invalidated_by}")]
    AlreadyInvalidated {
        /// The target the submission tried to withdraw.
        id: Uuid,
        /// The invalidation entry that already withdrew it.
        invalidated_by: Uuid,
    },

    /// Non-retryable unclassified plugin-side failure (plugin invariant
    /// broken, uncategorized backend error). Use [`Self::Transient`] for
    /// retryable backend errors.
    #[error("plugin internal error: {0}")]
    Internal(String),
}

impl UsageCollectorPluginError {
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
