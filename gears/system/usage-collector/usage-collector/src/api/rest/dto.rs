//! Wire DTOs for the foundation REST surface.
//!
//! `UsageRecord` create — batch create request / response shapes for
//! `POST /usage-collector/v1/records`. A withdrawal is an ordinary entry
//! on that same create surface, so it needs no DTO of its own. List-page
//! envelopes use [`toolkit_odata::Page`] directly; `OData` query parameters
//! (`limit`, `cursor`) are parsed by the toolkit `OData` extractor and need
//! no module-local DTO. Every usage-type declaration is now owned by
//! `types-registry`; this gear registers no usage-type REST surface and
//! declares no usage-type DTO.
//!
//! Every wire-facing type is declared as a thin DTO with
//! `#[toolkit_macros::api_dto(...)]` so the emitted OAS references a stable
//! schema component; the SDK models stay free of `utoipa::ToSchema` to keep
//! `utoipa` out of the plugin SDK's transitive deps.

use std::collections::BTreeMap;

use bigdecimal::BigDecimal;
use time::OffsetDateTime;
use toolkit_canonical_errors::Problem;
use toolkit_odata::{CursorV1, PageInfo};
use usage_collector_sdk::{
    AggregationBucket, AggregationDimension, AggregationResult, FeedPage, MetadataKey,
    QuantitySummary, ReconciliationMetadata, ReconciliationScope, ResourceRef, SubjectRef,
    TimeRange, UsageCollectorError, UsageRecord,
};
use uuid::Uuid;

// UsageRecord create DTOs

/// Wire-projection of [`usage_collector_sdk::ResourceRef`]. Mirrors the SDK
/// shape verbatim; the SDK struct stays `utoipa`-free.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct ResourceRefDto {
    pub resource_id: String,
    pub resource_type: String,
}

impl From<ResourceRef> for ResourceRefDto {
    fn from(value: ResourceRef) -> Self {
        Self {
            resource_id: value.resource_id().to_owned(),
            resource_type: value.resource_type().to_owned(),
        }
    }
}

impl TryFrom<ResourceRefDto> for ResourceRef {
    type Error = UsageCollectorError;

    fn try_from(value: ResourceRefDto) -> Result<Self, Self::Error> {
        ResourceRef::new(value.resource_id, value.resource_type)
    }
}

/// Wire-projection of [`usage_collector_sdk::SubjectRef`].
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct SubjectRefDto {
    pub subject_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_type: Option<String>,
}

impl From<SubjectRef> for SubjectRefDto {
    fn from(value: SubjectRef) -> Self {
        Self {
            subject_id: value.subject_id().to_owned(),
            subject_type: value.subject_type().map(str::to_owned),
        }
    }
}

impl TryFrom<SubjectRefDto> for SubjectRef {
    type Error = UsageCollectorError;

    fn try_from(value: SubjectRefDto) -> Result<Self, Self::Error> {
        SubjectRef::new(value.subject_id, value.subject_type)
    }
}

/// Per-record create payload. Carries `gts_type_id` and `entry_type` as
/// permissive `String`s so that a bad value is **attributed to the property
/// it came from**: `decode_record_entry` reads the offending property out of
/// serde's message, and `serde_field_name` recognises only ``unknown field
/// `…` `` and ``missing field `…` ``. A typed field's own refusal says
/// ``unknown variant `correction` ``, which matches neither, so the violation
/// would land on `records` and leave the caller diffing a whole entry. Parsed
/// at the fold point instead, the rejection names the offending property.
///
/// Intentionally has no identity field: `id` is gateway-derived via
/// `usage_collector_sdk::derive_usage_record_id`.
///
/// `deny_unknown_fields` is the schema's `additionalProperties: false`
/// (`usage-collector-v1.yaml` `CreateUsageRecordRequest`): every
/// server-owned field — `id`, `accepted_at`, `origin` and `invalidates` —
/// is rejected here as an unknown field rather than read and overridden.
///
/// **Traceability.** The `invalidates` half of that sentence is step 1 of
/// `cpt-cf-usage-collector-algo-withdrawal-linkage-stamping` ("Reject the
/// submission where the caller supplied a target linkage on the wire"); the
/// in-process surface refuses the same property through
/// `usage_collector_sdk`'s own `deny_unknown_fields` codec, so neither admits
/// one. [`Self::entry_type`] is the caller-supplied, defaultless discriminator
/// `cpt-cf-usage-collector-dod-explicit-entry-type` requires, shared by both
/// ingestion routes — the backfill route registers this same request shape.
// @cpt-algo:cpt-cf-usage-collector-algo-withdrawal-linkage-stamping:p1
// @cpt-dod:cpt-cf-usage-collector-dod-explicit-entry-type:p1
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct CreateUsageRecordRequest {
    /// The kind this submission declares — `record` or `invalidation`.
    /// Caller-supplied, required, and with no default
    /// (`usage-collector-v1.yaml` `EntryType`; DESIGN §3.1, "Entry type and
    /// reason code").
    ///
    /// **Never inferred.** An invalidation is a faithful copy of its target
    /// and repeats its target's idempotency key, so a withdrawal stripped of
    /// its discriminator would derive its target's own identity and be
    /// absorbed as a retry of it, withdrawing nothing. It is also an input to
    /// the derived `id`, which is what keeps a withdrawal's identity apart
    /// from the identity of the entry it withdraws.
    ///
    /// Carried as a permissive `String` for the reason the struct doc gives.
    pub entry_type: String,
    pub gts_type_id: String,
    pub tenant_id: Uuid,
    pub resource_ref: ResourceRefDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_ref: Option<SubjectRefDto>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    /// The measured quantity as its wire text (a JSON string). Parsed into
    /// [`usage_collector_sdk::UsageQuantity`] where the DTO is folded into the
    /// domain type, so an out-of-range value rejects its own entry rather
    /// than the whole batch.
    pub quantity: String,
    /// The caller's key. Not an `Option`, because the schema lists it in
    /// `required` on **both** branches: a withdrawal repeats its target's key
    /// rather than deriving one of its own, and that repetition is what lets
    /// the gateway find the target from the submission alone (DESIGN §3.1,
    /// "Target resolution"). No prefix is reserved.
    ///
    /// Typed rather than defaulted so the served schema says so too: omitting
    /// the property decodes as ``missing field `idempotency_key` ``, which
    /// `serde_field_name` recognises. An explicit `null` decodes to a *type*
    /// error naming no property at all, which is why
    /// `explicit_null_idempotency_key` runs ahead of the decode.
    pub idempotency_key: String,
    /// Why the withdrawal was issued. Required when [`Self::entry_type`] is
    /// `invalidation` and MUST NOT appear on a `record` (DESIGN §3.1,
    /// "Entry type and reason code"). The two are caller-supplied halves of
    /// one statement, and the SDK projection checks them against each other
    /// before anything else, reporting a disagreement as an
    /// `InvalidArgument` naming `reason_code`.
    ///
    /// It carries no target: `invalidates` is server-assigned (DESIGN §3.1,
    /// "Field ownership") and a submitted one is refused as an unknown field,
    /// so an emitter cannot name a record it never measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    /// Inclusive start of the covered period this submission measures (RFC
    /// 3339, offset mandatory).
    #[serde(with = "time::serde::rfc3339")]
    pub window_start: OffsetDateTime,
    /// Exclusive end of the covered period (RFC 3339, offset mandatory).
    /// Equal bounds submit a point event.
    #[serde(with = "time::serde::rfc3339")]
    pub window_end: OffsetDateTime,
}

/// Batch create request body for `POST /usage-collector/v1/records`.
///
/// `records` is decoded one entry at a time by the handler
/// (`decode_record_entry`), after the batch-cap check, so one malformed
/// entry rejects only its own index rather than the whole request. The
/// schema still publishes the entry shape via `#[schema(value_type = ...)]`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct CreateUsageRecordsRequest {
    #[schema(value_type = Vec<CreateUsageRecordRequest>)]
    pub records: Vec<serde_json::Value>,
}

/// Wire-projection of [`usage_collector_sdk::UsageRecord`]. `gts_type_id`,
/// `entry_type` and `origin` are flattened to `String` so the type can derive
/// `utoipa::ToSchema` without pulling `utoipa` into the SDK crate — each is a
/// closed SDK enum whose wire form is already the lowercased variant name, so
/// the `String`s mirror an encoding that does exist. Both covered-period
/// bounds are emitted as RFC 3339 to match the SDK wire shape.
///
/// [`usage_collector_sdk::UsageRecord`] stores `origin` as a field and carries
/// no discriminator beside it, so this projection reads `entry_type` back
/// through `UsageRecord::entry_type()`. That is a read-path projection and
/// nothing more: on the ingestion shape the discriminator is caller-supplied
/// and required ([`CreateUsageRecordRequest::entry_type`]), never inferred
/// (DESIGN §3.1, "Entry type and reason code").
///
/// **Traceability: the read-path half of
/// `cpt-cf-usage-collector-dod-withdrawal-linkage` and the whole of
/// `cpt-cf-usage-collector-flow-inspect-withdrawal-linkage`.** One shape
/// serves every ledger read path the gear publishes, so [`Self::invalidates`],
/// [`Self::reason_code`] and [`Self::entry_type`] reach a consumer on all of
/// them. The projection below copies `invalidation.target` through unchanged
/// and derives nothing, so the linkage a consumer reads is the identifier the
/// gateway stamped. The flow's error scenarios are properties of this shape
/// rather than of any code: there is **no** reverse pointer from a withdrawn
/// entry to its invalidation — a consumer holding the withdrawn entry derives
/// the invalidation's identifier from that entry's own fields
/// (`cpt-cf-usage-collector-flow-reproduce-identity-offline`); there is **no**
/// status field or lifecycle flag for a withdrawn entry to acquire, so it
/// reads back exactly as accepted; and no route reverses an accepted
/// invalidation.
// @cpt-dod:cpt-cf-usage-collector-dod-withdrawal-linkage:p1
// @cpt-flow:cpt-cf-usage-collector-flow-inspect-withdrawal-linkage:p1
//
// Traceability (usage-query): that same one-shape-per-read-path property,
// copying every field through unchanged with no marker, flag or derived field
// added, realizes `algo-raw-ledger-projection`, `dod-raw-returns-persisted-pair`
// and `dod-unstripped-ledger-fields` — every field that DoD's closed list names
// is present below and `From<UsageRecord>` maps it, unstripped.
// @cpt-algo:cpt-cf-usage-collector-algo-raw-ledger-projection:p1
// @cpt-dod:cpt-cf-usage-collector-dod-raw-returns-persisted-pair:p1
// @cpt-dod:cpt-cf-usage-collector-dod-unstripped-ledger-fields:p1
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct UsageRecordDto {
    pub id: Uuid,
    pub gts_type_id: String,
    pub tenant_id: Uuid,
    pub resource_ref: ResourceRefDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_ref: Option<SubjectRefDto>,
    /// Closed-shape key/value map per the OAS `RecordMetadata` schema.
    /// Omitted from the wire when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    /// The persisted quantity as a JSON string, digit for digit as stored.
    pub quantity: String,
    /// Mandatory caller-supplied idempotency key per
    /// `cpt-cf-usage-collector-dod-usage-emission-fr-idempotency`. Every
    /// persisted record carries a non-empty key.
    pub idempotency_key: String,
    /// Gear-assigned instant of acceptance (RFC 3339). On an absorbed retry,
    /// the stored entry's instant.
    #[serde(with = "time::serde::rfc3339")]
    pub accepted_at: OffsetDateTime,
    /// Which ingestion path admitted the entry — `live` or `backfill`.
    /// Server-assigned, `required` on the OAS `UsageRecord`, so it is never
    /// omitted. Flattened to `String` for the same reason as
    /// [`Self::gts_type_id`]: to keep `utoipa` out of the SDK crate.
    pub origin: String,
    /// The entry this one withdraws, absent on an ordinary measurement.
    /// Both-or-neither with [`Self::reason_code`] — the SDK carries the
    /// pair as one field, so a response can never show half of it.
    ///
    /// `readOnly` on the wire, and the one property on these two shapes that
    /// genuinely is: the gateway derives it and stamps it (DESIGN §3.1,
    /// "Field ownership"), so it appears here and nowhere on the ingestion
    /// shape, which refuses a submitted one as an unknown field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalidates: Option<Uuid>,
    /// Why the withdrawal was issued. Absent on an ordinary measurement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    /// `record` or `invalidation`, **derived** from [`Self::invalidates`]
    /// rather than stored
    /// (`cpt-cf-usage-collector-adr-append-only-invalidation`): the SDK record
    /// carries the withdrawal and its reason as one field and no discriminator
    /// beside them. `required` on the OAS `UsageRecord`, so never omitted, and
    /// **not** `readOnly` — the ingestion shape declares an `entry_type` of its
    /// own, caller-supplied and required with no default
    /// ([`CreateUsageRecordRequest::entry_type`]). Derived here, supplied
    /// there.
    pub entry_type: String,
    /// Inclusive start of the covered period the entry measures.
    #[serde(with = "time::serde::rfc3339")]
    pub window_start: OffsetDateTime,
    /// Exclusive end of the covered period — the bound the read paths
    /// select on, via the mandatory `from` / `to` range
    /// (`cpt-cf-usage-collector-adr-window-end-selection`), and a key
    /// every raw-path page order names.
    #[serde(with = "time::serde::rfc3339")]
    pub window_end: OffsetDateTime,
}

impl From<UsageRecord> for UsageRecordDto {
    fn from(value: UsageRecord) -> Self {
        // Read before the destructuring below moves `invalidation` out:
        // `entry_type()` borrows the record, so computing it afterwards is
        // a borrow-after-move the compiler refuses.
        let entry_type = value.entry_type().as_str().to_owned();
        let (invalidates, reason_code) = match value.invalidation {
            Some(invalidation) => (
                Some(invalidation.target),
                Some(invalidation.reason.into_inner()),
            ),
            None => (None, None),
        };
        Self {
            id: value.id,
            gts_type_id: value.gts_type_id.to_string(),
            tenant_id: value.tenant_id,
            resource_ref: value.resource_ref.into(),
            subject_ref: value.subject_ref.map(Into::into),
            metadata: value
                .metadata
                .into_iter()
                .map(|(k, v)| (k.into_inner(), v))
                .collect(),
            quantity: value.quantity.to_string(),
            idempotency_key: value.idempotency_key.into_inner(),
            accepted_at: value.accepted_at,
            origin: value.origin.as_str().to_owned(),
            invalidates,
            reason_code,
            entry_type,
            window_start: value.window_start,
            window_end: value.window_end,
        }
    }
}

/// Per-record outcome inside [`CreateUsageRecordsResponse`]. Externally
/// tagged on `outcome` (snake-case via the `api_dto`-applied `rename_all`)
/// so accepted and rejected records share a uniform envelope keyed by
/// input order. An accepted entry carries the persisted record body; a
/// rejected entry carries the per-record canonical `Problem` body
/// verbatim — the same envelope a single-record failure would surface,
/// folded into the batch response.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
#[serde(tag = "outcome")]
pub enum CreateUsageRecordResultDto {
    Accepted {
        index: usize,
        record: UsageRecordDto,
    },
    Rejected {
        index: usize,
        error: Problem,
    },
}

/// Batch create response body. `results` preserves input order; a partial
/// failure surfaces as HTTP `207 Multi-Status` with per-record `Rejected`
/// entries (callers must inspect each `outcome`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct CreateUsageRecordsResponse {
    pub results: Vec<CreateUsageRecordResultDto>,
}

// Aggregated-query DTOs

/// Wire projection of [`usage_collector_sdk::AggregationDimension`]. The
/// closed dimensions are encoded as snake-case bare strings; the
/// `metadata` form carries the declared key inline as
/// `{"metadata": "<key>"}` to mirror the SDK's `Metadata(MetadataKey)`
/// variant under the same lowercased external tag. (The macro-applied
/// `rename_all = "snake_case"` matches the SDK encoding here.)
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub enum AggregationDimensionDto {
    TenantId,
    ResourceId,
    ResourceType,
    SubjectId,
    SubjectType,
    Metadata(String),
}

impl TryFrom<AggregationDimensionDto> for AggregationDimension {
    type Error = UsageCollectorError;

    fn try_from(value: AggregationDimensionDto) -> Result<Self, Self::Error> {
        Ok(match value {
            AggregationDimensionDto::TenantId => AggregationDimension::TenantId,
            AggregationDimensionDto::ResourceId => AggregationDimension::ResourceId,
            AggregationDimensionDto::ResourceType => AggregationDimension::ResourceType,
            AggregationDimensionDto::SubjectId => AggregationDimension::SubjectId,
            AggregationDimensionDto::SubjectType => AggregationDimension::SubjectType,
            AggregationDimensionDto::Metadata(key) => {
                AggregationDimension::Metadata(MetadataKey::new(key)?)
            }
        })
    }
}

impl From<AggregationDimension> for AggregationDimensionDto {
    fn from(value: AggregationDimension) -> Self {
        match value {
            AggregationDimension::TenantId => AggregationDimensionDto::TenantId,
            AggregationDimension::ResourceId => AggregationDimensionDto::ResourceId,
            AggregationDimension::ResourceType => AggregationDimensionDto::ResourceType,
            AggregationDimension::SubjectId => AggregationDimensionDto::SubjectId,
            AggregationDimension::SubjectType => AggregationDimensionDto::SubjectType,
            AggregationDimension::Metadata(key) => {
                AggregationDimensionDto::Metadata(key.into_inner())
            }
        }
    }
}

/// Wire projection of [`usage_collector_sdk::TimeRange`] — the mandatory
/// bounded range on the aggregate path (`AggregationRequest.time_range` in
/// `docs/usage-collector-v1.yaml`). The raw path carries the same range as
/// the `from` / `to` query parameters instead, because a `GET` has no body.
///
/// Both bounds deserialize through `time::serde::rfc3339`, which rejects an
/// offset-less timestamp: a bare local time would attribute usage to
/// whatever offset the server happened to assume.
///
/// `Copy`, like the SDK [`TimeRange`] it projects onto — two plain
/// timestamps — so reading it out of a request body does not partially
/// move the body away from the group-by projection that follows.
#[derive(Debug, Clone, Copy)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct TimeRangeDto {
    /// Inclusive lower bound (RFC 3339, offset mandatory).
    #[serde(with = "time::serde::rfc3339")]
    pub from: OffsetDateTime,
    /// Exclusive upper bound (RFC 3339, offset mandatory).
    #[serde(with = "time::serde::rfc3339")]
    pub to: OffsetDateTime,
}

impl TryFrom<TimeRangeDto> for TimeRange {
    type Error = UsageCollectorError;

    fn try_from(value: TimeRangeDto) -> Result<Self, Self::Error> {
        Self::new(value.from, value.to)
    }
}

/// Request body for `POST /usage-collector/v1/records/aggregate`.
///
/// Named for the `AggregationRequest` component in
/// `docs/usage-collector-v1.yaml`. It carries no `Dto` suffix: that suffix
/// elsewhere in this module disambiguates a DTO from an SDK type of the same
/// bare name, and there is no `AggregationRequest` in `usage_collector_sdk` to
/// collide with. It mirrors the component's field set exactly, with
/// `additionalProperties: false` and `required: [time_range]`.
///
/// The typed `gts_type_id`, the `OData` `$filter`, and the `metadata.<key>`
/// side-channel are query parameters instead (mirroring
/// `GET /usage-collector/v1/records`), declared on the operation rather than
/// on this schema. There is no aggregation parameter: the fold is resolved
/// from the queried type's declaration, so no request is well-formed and
/// semantically wrong.
///
/// Traceability: the REST half of
/// `cpt-cf-usage-collector-dod-no-aggregation-parameter` —
/// `#[serde(deny_unknown_fields)]` below refuses a body naming a
/// fold-selecting field as an unknown field.
// @cpt-dod:cpt-cf-usage-collector-dod-no-aggregation-parameter:p1
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct AggregationRequest {
    pub time_range: TimeRangeDto,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_by: Vec<AggregationDimensionDto>,
}

impl AggregationRequest {
    /// Projects the wire `group_by` dimensions into their typed SDK form.
    ///
    /// A free method rather than a `TryFrom` impl: the target,
    /// `Vec<AggregationDimension>`, is foreign to this crate (both `Vec`
    /// and `AggregationDimension` live outside it), so a blanket
    /// `TryFrom<AggregationRequest> for Vec<AggregationDimension>`
    /// would violate the orphan rule.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorError`] when a `Metadata` dimension carries a
    /// key [`MetadataKey::new`] rejects (e.g. empty or oversized).
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    pub fn into_group_by(self) -> Result<Vec<AggregationDimension>, UsageCollectorError> {
        self.group_by
            .into_iter()
            .map(AggregationDimension::try_from)
            .collect()
    }
}

/// Wire projection of [`usage_collector_sdk::AggregationBucket`]. `value`
/// is an arbitrary-precision `bigdecimal::BigDecimal` carried as a JSON
/// string via `usage_collector_sdk::serde_helpers::bigdecimal_str_option`
/// (the same string-on-the-wire discipline as `UsageRecordDto.quantity`, but
/// without `Decimal`'s magnitude ceiling); `None` materializes as `null`
/// per the SDK contract for empty-set buckets.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AggregationBucketDto {
    #[serde(default)]
    pub key: Vec<String>,
    #[serde(
        default,
        with = "usage_collector_sdk::serde_helpers::bigdecimal_str_option"
    )]
    #[schema(value_type = Option<String>, example = "79228162514264337593543950400")]
    pub value: Option<BigDecimal>,
}

impl From<AggregationBucket> for AggregationBucketDto {
    fn from(value: AggregationBucket) -> Self {
        Self {
            key: value.key,
            value: value.value,
        }
    }
}

/// Aggregated-query response body. Mirrors
/// [`usage_collector_sdk::AggregationResult`] — buckets are emitted in
/// plugin-defined order and a no-grouping query yields a single bucket
/// with an empty `key`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AggregationResultDto {
    pub buckets: Vec<AggregationBucketDto>,
}

impl From<AggregationResult> for AggregationResultDto {
    fn from(value: AggregationResult) -> Self {
        Self {
            buckets: value.buckets.into_iter().map(Into::into).collect(),
        }
    }
}

// Feed DTOs

/// Wire-projection of [`usage_collector_sdk::FeedPage<CursorV1>`]
/// (`usage-collector-v1.yaml` `FeedPage`).
///
/// Not [`toolkit_odata::Page`]: that type's array field is `items`, while the
/// published `FeedPage` schema names its array `entries` — a page of the usage
/// feed is a different resource from a raw-query result page (DESIGN §3.1).
/// [`PageInfo`] is reused verbatim for `page_info`, so the only thing this
/// projection adds over the domain [`FeedPage`] is `limit`, which that type
/// carries nowhere. The contract fixes `prev_cursor` at `null` always — the
/// feed reads forward only (DESIGN §3.2) — which `try_from_feed_page`
/// satisfies by construction. (A plain code span rather than an intra-doc
/// link: that constructor is crate-private and this type is public.)
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct FeedPageDto {
    pub entries: Vec<UsageRecordDto>,
    pub page_info: PageInfo,
}

impl FeedPageDto {
    /// Project a service-returned feed page onto the wire shape.
    ///
    /// `limit` is the page-size bound the request resolved to
    /// (`domain::feed::resolve_limit`'s return), echoed onto
    /// `page_info.limit` — see the struct doc for why it has to arrive as
    /// a parameter rather than being read off `page`.
    ///
    /// # Errors
    ///
    /// [`UsageCollectorError::Internal`] if [`CursorV1::encode`] fails.
    /// Unreachable for a cursor `domain::feed::mint_cursor` built — every
    /// field it sets is a plain string or a closed enum, and
    /// `CursorV1::encode` fails only on a `serde_json` serialization error.
    /// Kept as a `Result` rather than an `.expect()` all the same, because
    /// this function's only caller is the REST handler, not the type
    /// constructing the cursor.
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    pub(crate) fn try_from_feed_page(
        page: FeedPage<CursorV1>,
        limit: u64,
    ) -> Result<Self, UsageCollectorError> {
        let next_cursor = page
            .next
            .map(|cursor| cursor.encode())
            .transpose()
            .map_err(|e| {
                UsageCollectorError::internal(format!("feed cursor failed to encode: {e}"))
            })?;
        Ok(Self {
            entries: page.entries.into_iter().map(UsageRecordDto::from).collect(),
            page_info: PageInfo {
                next_cursor,
                prev_cursor: None,
                limit,
            },
        })
    }
}

// Reconciliation DTOs

/// Wire-projection of the scope object `usage-collector-v1.yaml`'s
/// `ReconciliationMetadata.scope` declares **inline** — tenant and GTS type,
/// the pair `domain::reconciliation::build_scope` derives from the request's
/// own parameters. Built at the handler, not derived from the service's
/// answer: [`usage_collector_sdk::ReconciliationMetadata`] carries no scope.
///
/// The yaml declares no standalone `ReconciliationScope` component, so every
/// field using this type carries `#[schema(inline)]`, which keeps `utoipa`
/// from registering a document-level component with nothing on the contract
/// side to match it.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ReconciliationScopeDto {
    pub tenant_id: Uuid,
    pub gts_type_id: String,
}

impl From<ReconciliationScope> for ReconciliationScopeDto {
    fn from(value: ReconciliationScope) -> Self {
        Self {
            tenant_id: value.tenant_id,
            gts_type_id: value.gts_type_id.to_string(),
        }
    }
}

/// The branches of `quantity_summary`, as the yaml's `oneOf` declares them:
/// exactly one is present, and which one is decided by the queried meter's
/// declared fold rather than by anything the caller sent.
///
/// `accrued_sum` and `latest_observation` are JSON **strings**, never numbers,
/// so neither round-trips through a float on a billing surface.
/// `latest_observation` carries no `skip_serializing_if`: the yaml requires
/// the property on every `Observations`-branch body and fixes it at `null`
/// when the range selected nothing, so an absent field would be a different,
/// undocumented shape.
///
/// `utoipa::ToSchema`'s derive reads the `#[serde(untagged)]` representation
/// attribute and emits the matching `oneOf` with no discriminant property. The
/// yaml declares no standalone `QuantitySummary` component, so every field
/// using this type also carries `#[schema(inline)]`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
#[serde(untagged)]
pub enum QuantitySummaryDto {
    Accrued {
        accrued_sum: String,
    },
    Observations {
        observation_count: u64,
        latest_observation: Option<String>,
    },
}

impl From<QuantitySummary> for QuantitySummaryDto {
    fn from(value: QuantitySummary) -> Self {
        match value {
            QuantitySummary::Accrued(sum) => Self::Accrued {
                // `sum.to_string()` would emit scientific notation past
                // `BigDecimal::Display`'s scale threshold (e.g. `3.4E-7`), and
                // `AggregatedQuantity`'s published pattern admits no exponent
                // form. `to_plain_string()` is called directly rather than
                // routed through `serde_helpers::bigdecimal_str_option` (the
                // sibling `AggregationBucketDto.value` codec) because that is
                // a serde `with`-module for an `Option<BigDecimal>` field and
                // this is a `String` built by hand. Both render through the
                // one primitive that is safe for the wire.
                accrued_sum: sum.to_plain_string(),
            },
            QuantitySummary::Observations(observed) => Self::Observations {
                observation_count: observed.as_ref().map_or(0, |o| o.count.get()),
                latest_observation: observed.map(|o| o.latest.to_string()),
            },
        }
    }
}

/// `usage-collector-v1.yaml`'s `ReconciliationMetadata`.
///
/// Not a page. `cpt-cf-usage-collector-dod-canonical-page-envelope` binds the
/// list-shaped read paths and deliberately does not reach this one, so there
/// is no `PageInfo`, no cursor and no page size — the gear serves one scope
/// per call and enumerates nothing.
///
/// The field is named `scope` because the yaml names it that. It is the
/// tenant-and-GTS-type target these figures cover, unrelated to the
/// reporting *granularity* the wire query parameter also spells `scope`, and
/// unrelated again to the compiled PDP scope the gear calls `scope`
/// everywhere else; `domain::reconciliation`'s module doc carries the
/// warning in full.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ReconciliationMetadataDto {
    #[schema(inline)]
    pub scope: ReconciliationScopeDto,
    pub accepted_count: u64,
    #[schema(inline)]
    pub quantity_summary: QuantitySummaryDto,
    /// `null` when the scope holds no entries — **not omitted**: the yaml
    /// lists this property `required` on `ReconciliationMetadata`, so it
    /// carries no `skip_serializing_if`, unlike every optional field
    /// elsewhere in this module. Rendered as RFC 3339 text rather than a
    /// typed `OffsetDateTime` field, so the absent case is a plain
    /// `Option<String>` rather than needing `time::serde::rfc3339`'s
    /// separate `Option`-flavoured module.
    pub accepted_at_watermark: Option<String>,
    /// `null` when the scope holds no entries — see
    /// [`Self::accepted_at_watermark`] for why this carries no
    /// `skip_serializing_if` either.
    pub window_end_watermark: Option<String>,
}

impl ReconciliationMetadataDto {
    /// Project a service-returned [`ReconciliationMetadata`] onto the wire
    /// shape, combined with the [`ReconciliationScope`] the handler built
    /// from the request's own parameters via `domain::reconciliation::build_scope`
    /// — `ReconciliationMetadata` itself carries no scope.
    pub(crate) fn from_metadata(
        scope: ReconciliationScope,
        metadata: ReconciliationMetadata,
    ) -> Self {
        Self {
            scope: scope.into(),
            accepted_count: metadata.accepted_count,
            quantity_summary: metadata.quantity_summary.into(),
            accepted_at_watermark: metadata.max_accepted_at.map(format_watermark),
            window_end_watermark: metadata.max_window_end.map(format_watermark),
        }
    }
}

/// Render a watermark as RFC 3339 text, falling back to `Display` on the
/// same unreachable-in-practice failure mode
/// `usage_collector_sdk::error`'s own `rfc3339` helper documents: `Rfc3339`
/// rejects a year outside `0..10_000` or a non-zero-second offset, neither of
/// which a value already accepted onto a persisted watermark can carry.
fn format_watermark(ts: OffsetDateTime) -> String {
    ts.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| ts.to_string())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "dto_tests.rs"]
mod dto_tests;
