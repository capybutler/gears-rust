//! Wire DTOs for the foundation REST surface.
//!
//! `UsageRecord` create / deactivation — batch create request / response
//! shapes for `POST /usage-collector/v1/records`. Deactivation returns no
//! body (HTTP 204 No Content) so it carries no response DTO. List-page
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
use rust_decimal::Decimal;
use time::OffsetDateTime;
use toolkit_canonical_errors::Problem;
use usage_collector_sdk::{
    AggregationBucket, AggregationDimension, AggregationResult, MetadataKey, ResourceRef,
    SubjectRef, TimeRange, UsageCollectorError, UsageRecord, UsageRecordStatus,
};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// UsageRecord create DTOs
// ---------------------------------------------------------------------------

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

/// Per-record create payload. Carries `gts_type_id` as a permissive `String`
/// so a bad-prefix value surfaces as the per-record `Problem` instead of axum's default
/// `text/plain` 422 for the entire batch. per-record problem envelopes
/// still surface for closed-shape membership, size-cap, and key
/// validation. Intentionally has no identity field: `id` is
/// gateway-derived via `usage_collector_sdk::derive_usage_record_id`,
/// mirroring the `UsageRecord::id` doc.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct CreateUsageRecordRequest {
    pub gts_type_id: String,
    pub tenant_id: Uuid,
    pub resource_ref: ResourceRefDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_ref: Option<SubjectRefDto>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    #[serde(with = "rust_decimal::serde::str")]
    pub value: Decimal,
    /// Mandatory caller-supplied idempotency key per
    /// `cpt-cf-usage-collector-dod-usage-emission-fr-idempotency`. The
    /// dedup identity is the 5-tuple
    /// `(tenant_id, gts_type_id, idempotency_key, window_start, window_end)`
    /// (`cpt-cf-usage-collector-adr-record-identity-derivation`), so one
    /// stable per-meter key covers many periods; a missing key surfaces as
    /// a request-deserialization failure.
    pub idempotency_key: String,
    /// When set, marks this submission as a counter compensation
    /// referencing a previously emitted ordinary usage row. Absent on
    /// ordinary submissions. The four-cell value matrix and L1 referential
    /// rule are enforced at the gateway per
    /// `cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corrects_id: Option<Uuid>,
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
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct CreateUsageRecordsRequest {
    pub records: Vec<CreateUsageRecordRequest>,
}

/// Wire-projection of [`usage_collector_sdk::UsageRecord`]. `gts_type_id` is
/// flattened to `String` so the type can derive `utoipa::ToSchema` without
/// pulling `utoipa` into the SDK crate; both covered-period bounds are
/// emitted as RFC 3339 to match the SDK wire shape. `status` is projected to
/// its lowercase string form for the same reason.
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
    #[serde(with = "rust_decimal::serde::str")]
    pub value: Decimal,
    /// Mandatory caller-supplied idempotency key per
    /// `cpt-cf-usage-collector-dod-usage-emission-fr-idempotency`. Every
    /// persisted record carries a non-empty key.
    pub idempotency_key: String,
    /// Present when this record is a counter compensation referencing a
    /// previously emitted ordinary usage row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corrects_id: Option<Uuid>,
    /// Lifecycle status: `"active"` on a fresh insert, `"inactive"` after a
    /// depth-1 deactivation cascade.
    pub status: String,
    /// Inclusive start of the covered period the entry measures.
    #[serde(with = "time::serde::rfc3339")]
    pub window_start: OffsetDateTime,
    /// Exclusive end of the covered period — the bound the read paths
    /// select on, via the mandatory `from` / `to` range
    /// (`cpt-cf-usage-collector-adr-window-end-selection`), and the
    /// leading key of the raw path's `(window_end, id)` page order.
    #[serde(with = "time::serde::rfc3339")]
    pub window_end: OffsetDateTime,
}

impl From<UsageRecord> for UsageRecordDto {
    fn from(value: UsageRecord) -> Self {
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
            value: value.value,
            idempotency_key: value.idempotency_key.into_inner(),
            corrects_id: value.corrects_id,
            status: match value.status {
                UsageRecordStatus::Active => "active".to_owned(),
                UsageRecordStatus::Inactive => "inactive".to_owned(),
            },
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

// ---------------------------------------------------------------------------
// Aggregated-query DTOs
// ---------------------------------------------------------------------------

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

/// Aggregated-query request body for
/// `POST /usage-collector/v1/records/aggregate`. Carries the mandatory
/// `time_range` and the group-by dimensions; the typed `gts_type_id`, the
/// `OData` `$filter`, and the `metadata.<key>` side-channel remain query
/// parameters (mirroring `GET /usage-collector/v1/records`). Carries no
/// aggregation parameter (matches `AggregationRequest` in
/// `docs/usage-collector-v1.yaml`): the fold is resolved from the queried
/// type's declaration, so no request is well-formed and semantically wrong.
///
/// `time_range` has no `#[serde(default)]` — the contract marks it
/// required, so a body omitting it is a deserialization failure rather than
/// an unbounded scan.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct QueryAggregatedUsageRecordsRequest {
    pub time_range: TimeRangeDto,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_by: Vec<AggregationDimensionDto>,
}

impl QueryAggregatedUsageRecordsRequest {
    /// Projects the wire `group_by` dimensions into their typed SDK form.
    ///
    /// A free method rather than a `TryFrom` impl: the target,
    /// `Vec<AggregationDimension>`, is foreign to this crate (both `Vec`
    /// and `AggregationDimension` live outside it), so a blanket
    /// `TryFrom<QueryAggregatedUsageRecordsRequest> for Vec<AggregationDimension>`
    /// would violate the orphan rule.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorError`] when a `Metadata` dimension carries a
    /// key [`MetadataKey::new`] rejects (e.g. empty or oversized).
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
/// (the same string-on-the-wire discipline as `UsageRecord.value`, but
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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "dto_tests.rs"]
mod dto_tests;
