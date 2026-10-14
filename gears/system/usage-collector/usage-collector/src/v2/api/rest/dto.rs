//! Wire DTOs for the foundation REST surface.
//!
//! `UsageRecord` create — batch create request / response shapes for
//! `POST /usage-collector/v2/records`. A withdrawal is an ordinary entry
//! on that same create surface, so it needs no DTO of its own. List-page
//! envelopes use [`toolkit_odata::Page`] directly; `OData` query parameters
//! (`limit`, `cursor`) are parsed by the toolkit `OData` extractor and need
//! no module-local DTO. Every usage-type declaration is owned by
//! `types-registry`, so this gear declares no usage-type DTO.
//!
//! Every wire-facing type is declared as a thin DTO with
//! `#[toolkit_macros::api_dto(...)]` so the emitted OAS references a stable
//! schema component; the SDK models stay free of `utoipa::ToSchema` to keep
//! `utoipa` out of the plugin SDK's transitive deps.
//!
//! ## Why ten of these carry `#[schema(as = …V2)]`
//!
//! The host resolves one `OpenApiRegistry` for the whole application, so both
//! generations of this gear write into a single `components.schemas`
//! namespace, and one name arriving with two different definitions is a hard
//! error by design. Ten idents here are also declared, with different shapes,
//! by generation 1's `crate::api::rest::dto`, so generation 2 takes an alias
//! for each — generation 1 is frozen, its published document byte-identical
//! to `main`'s. The remaining DTOs collide with nothing and are deliberately
//! not aliased, as are the platform / SDK-level schemas (`Problem`,
//! `Timestamp`, …), which register identically and whose renaming would make
//! this document disagree with the platform's own vocabulary.
//!
//! `V2` goes *before* `Dto`, never after: `openapi_contract_tests`
//! translates a registered name into the published one by
//! `strip_suffix("Dto")`, so `UsageRecordV2Dto` publishes as
//! `UsageRecordV2` while `UsageRecordDtoV2` would ship the implementation
//! marker. The three aliases whose Rust types never carried a `Dto` take the
//! bare `…V2`, and the placement carries to derived names
//! (`Page_UsageRecordV2Dto` publishes as `Page_UsageRecordV2`).

use std::collections::BTreeMap;

use bigdecimal::BigDecimal;
use time::OffsetDateTime;
use toolkit_canonical_errors::Problem;
use toolkit_odata::{CursorV1, PageInfo};
use usage_collector_sdk::v2::{
    AggregationBucket, AggregationDimension, AggregationResult, FeedPage, MetadataKey, MeterTypeId,
    QuantitySummary, ReconciliationMetadata, ResourceRef, SubjectRef, TimeRange,
    UsageCollectorError, UsageRecord,
};
use uuid::Uuid;

// UsageRecord create DTOs

/// Wire-projection of [`usage_collector_sdk::v2::ResourceRef`]. Mirrors the SDK
/// shape verbatim; the SDK struct stays `utoipa`-free.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
#[schema(as = ResourceRefV2Dto)]
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

/// Wire-projection of [`usage_collector_sdk::v2::SubjectRef`].
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
#[schema(as = SubjectRefV2Dto)]
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
/// `usage_collector_sdk::v2::derive_usage_record_id`.
///
/// `deny_unknown_fields` is the schema's `additionalProperties: false`: every
/// server-owned field — `id`, `accepted_at`, `origin` and `invalidates` — is
/// rejected here as an unknown field rather than read and overridden. Both
/// ingestion routes register this same request shape.
// @cpt-algo:cpt-cf-usage-collector-algo-withdrawal-linkage-stamping:p1
// @cpt-dod:cpt-cf-usage-collector-dod-explicit-entry-type:p1
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[schema(as = CreateUsageRecordRequestV2)]
#[serde(deny_unknown_fields)]
pub struct CreateUsageRecordRequest {
    /// The kind this submission declares — `record` or `invalidation`.
    /// Caller-supplied, required, and with no default.
    ///
    /// **Never inferred.** An invalidation is a faithful copy of its target
    /// and repeats its target's idempotency key, so a withdrawal stripped of
    /// its discriminator would derive its target's own identity and be
    /// absorbed as a retry of it, withdrawing nothing.
    pub entry_type: String,
    pub gts_type_id: String,
    pub tenant_id: Uuid,
    pub resource_ref: ResourceRefDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_ref: Option<SubjectRefDto>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    /// The measured quantity as its wire text (a JSON string). Parsed into
    /// [`usage_collector_sdk::v2::UsageQuantity`] where the DTO is folded into the
    /// domain type, so an out-of-range value rejects its own entry rather
    /// than the whole batch.
    pub quantity: String,
    /// The caller's key. Not an `Option`, because the schema lists it in
    /// `required` on **both** branches: a withdrawal repeats its target's key
    /// rather than deriving one of its own, and that repetition is what lets
    /// the gateway find the target from the submission alone. No prefix is
    /// reserved.
    ///
    /// An explicit `null` decodes to a *type* error naming no property at
    /// all, which is why `explicit_null_idempotency_key` runs ahead of the
    /// decode.
    pub idempotency_key: String,
    /// Why the withdrawal was issued. Required when [`Self::entry_type`] is
    /// `invalidation` and MUST NOT appear on a `record`; the SDK projection
    /// checks the pair against each other before anything else, reporting a
    /// disagreement as an `InvalidArgument` naming `reason_code`.
    ///
    /// It carries no target: `invalidates` is server-assigned and a submitted
    /// one is refused as an unknown field, so an emitter cannot name a record
    /// it never measured.
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

/// Batch create request body for `POST /usage-collector/v2/records`.
///
/// `records` is decoded one entry at a time by the handler
/// (`decode_record_entry`), after the batch-cap check, so one malformed
/// entry rejects only its own index rather than the whole request. The
/// schema still publishes the entry shape via `#[schema(value_type = ...)]`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[schema(as = CreateUsageRecordsRequestV2)]
#[serde(deny_unknown_fields)]
pub struct CreateUsageRecordsRequest {
    #[schema(value_type = Vec<CreateUsageRecordRequest>)]
    pub records: Vec<serde_json::Value>,
}

/// Wire-projection of [`usage_collector_sdk::v2::UsageRecord`]. `gts_type_id`,
/// `entry_type` and `origin` are flattened to `String` so the type can derive
/// `utoipa::ToSchema` without pulling `utoipa` into the SDK crate — each is a
/// closed SDK enum whose wire form is already the lowercased variant name, so
/// the `String`s mirror an encoding that does exist. Both covered-period
/// bounds are emitted as RFC 3339 to match the SDK wire shape.
///
/// [`usage_collector_sdk::v2::UsageRecord`] stores `origin` as a field and
/// carries no discriminator beside it, so this projection reads `entry_type`
/// back through `UsageRecord::entry_type()`. That is a read-path projection
/// and nothing more: on the ingestion shape the discriminator is
/// caller-supplied and required ([`CreateUsageRecordRequest::entry_type`]).
///
/// One shape serves every ledger read path the gear publishes, and every
/// field is copied through unchanged — no marker, flag or derived field is
/// added, and there is no reverse pointer from a withdrawn entry to its
/// invalidation, no status field and no route that reverses an accepted
/// invalidation.
// @cpt-dod:cpt-cf-usage-collector-dod-withdrawal-linkage:p1
// @cpt-flow:cpt-cf-usage-collector-flow-inspect-withdrawal-linkage:p1
// @cpt-algo:cpt-cf-usage-collector-algo-raw-ledger-projection:p1
// @cpt-dod:cpt-cf-usage-collector-dod-raw-returns-persisted-pair:p1
// @cpt-dod:cpt-cf-usage-collector-dod-unstripped-ledger-fields:p1
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
#[schema(as = UsageRecordV2Dto)]
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
    /// Mandatory caller-supplied idempotency key; every persisted record
    /// carries a non-empty one.
    pub idempotency_key: String,
    /// Gear-assigned instant of acceptance (RFC 3339). On an absorbed retry,
    /// the stored entry's instant.
    #[serde(with = "time::serde::rfc3339")]
    pub accepted_at: OffsetDateTime,
    /// Which ingestion path admitted the entry — `live` or `backfill`.
    /// Server-assigned and `required` on the wire, so never omitted.
    pub origin: String,
    /// The entry this one withdraws, absent on an ordinary measurement.
    /// Both-or-neither with [`Self::reason_code`] — the SDK carries the
    /// pair as one field, so a response can never show half of it.
    ///
    /// `readOnly` on the wire: the gateway derives and stamps it, so it
    /// appears here and nowhere on the ingestion shape, which refuses a
    /// submitted one as an unknown field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalidates: Option<Uuid>,
    /// Why the withdrawal was issued. Absent on an ordinary measurement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    /// `record` or `invalidation`, **derived** from [`Self::invalidates`]
    /// rather than stored: the SDK record carries the withdrawal and its
    /// reason as one field and no discriminator beside them. `required` but
    /// **not** `readOnly` — the ingestion shape declares an `entry_type` of
    /// its own ([`CreateUsageRecordRequest::entry_type`]). Derived here,
    /// supplied there.
    pub entry_type: String,
    /// Inclusive start of the covered period the entry measures.
    #[serde(with = "time::serde::rfc3339")]
    pub window_start: OffsetDateTime,
    /// Exclusive end of the covered period — the bound the read paths select
    /// on, via the mandatory `from` / `to` range, and a key every raw-path
    /// page order names.
    #[serde(with = "time::serde::rfc3339")]
    pub window_end: OffsetDateTime,
}

impl From<UsageRecord> for UsageRecordDto {
    fn from(value: UsageRecord) -> Self {
        // Read before the destructuring below moves `invalidation` out.
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
#[schema(as = CreateUsageRecordResultV2Dto)]
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
#[schema(as = CreateUsageRecordsResponseV2)]
pub struct CreateUsageRecordsResponse {
    pub results: Vec<CreateUsageRecordResultDto>,
}

// Aggregated-query DTOs

/// Wire projection of [`usage_collector_sdk::v2::AggregationDimension`]. The
/// closed dimensions are encoded as snake-case bare strings; the
/// `metadata` form carries the declared key inline as
/// `{"metadata": "<key>"}` to mirror the SDK's `Metadata(MetadataKey)`
/// variant under the same lowercased external tag. (The macro-applied
/// `rename_all = "snake_case"` matches the SDK encoding here.)
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
#[schema(as = AggregationDimensionV2Dto)]
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

/// Wire projection of [`usage_collector_sdk::v2::TimeRange`] — the mandatory
/// bounded range on the aggregate path. The raw path carries the same range
/// as the `from` / `to` query parameters instead, because a `GET` has no body.
///
/// Both bounds deserialize through `time::serde::rfc3339`, which rejects an
/// offset-less timestamp: a bare local time would attribute usage to whatever
/// offset the server happened to assume.
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

/// Request body for `POST /usage-collector/v2/records/aggregate`.
///
/// Mirrors the `AggregationRequest` component's field set exactly, with
/// `additionalProperties: false` and `required: [time_range]`. The typed
/// `gts_type_id`, the `OData` `$filter` and the `metadata.<key>`
/// side-channel are query parameters on the operation instead, mirroring
/// `GET /usage-collector/v2/records`.
///
/// There is no aggregation parameter — the fold is resolved from the queried
/// type's declaration — so `deny_unknown_fields` below refuses a body naming
/// a fold-selecting field.
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
    /// `Vec<AggregationDimension>`, is foreign to this crate, so the impl
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

/// Wire projection of [`usage_collector_sdk::v2::AggregationBucket`]. `value`
/// is an arbitrary-precision `bigdecimal::BigDecimal` carried as a JSON
/// string, the same string-on-the-wire discipline as `UsageRecordDto.quantity`;
/// `None` materializes as `null` for an empty-set bucket.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
#[schema(as = AggregationBucketV2Dto)]
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
/// [`usage_collector_sdk::v2::AggregationResult`] — buckets are emitted in
/// plugin-defined order and a no-grouping query yields a single bucket
/// with an empty `key`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
#[schema(as = AggregationResultV2Dto)]
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

/// Wire-projection of [`usage_collector_sdk::v2::FeedPage<CursorV1>`].
///
/// Not [`toolkit_odata::Page`]: that type's array field is `items`, while the
/// published `FeedPage` schema names its array `entries`. [`PageInfo`] is
/// reused verbatim for `page_info`, so the only thing this projection adds
/// over the domain [`FeedPage`] is `limit`. The contract fixes `prev_cursor`
/// at `null` always — the feed reads forward only — which
/// `try_from_feed_page` satisfies by construction.
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
    /// `page_info.limit`; the domain [`FeedPage`] carries it nowhere, so it
    /// arrives as a parameter.
    ///
    /// # Errors
    ///
    /// [`UsageCollectorError::Internal`] if [`CursorV1::encode`] fails —
    /// unreachable for a cursor `domain::feed::mint_cursor` built, but kept
    /// as a `Result` rather than an `.expect()` because this function's only
    /// caller is the REST handler, not the type constructing the cursor.
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

/// Wire-projection of the scope object `ReconciliationMetadata.scope`
/// declares **inline** — tenant and GTS type, built at the handler from the
/// request's own parameters rather than derived from the service's answer:
/// [`usage_collector_sdk::v2::ReconciliationMetadata`] carries no scope.
///
/// A REST-level concept with no SPI counterpart (DESIGN §3.1: v1 admits
/// `(tenant, gts_type)` alone, "so no SPI type corresponds to it"), which is
/// why it lives here and not in the contract crate.
///
/// The contract declares no standalone `ReconciliationScope` component, so
/// every field using this type carries `#[schema(inline)]`, which keeps
/// `utoipa` from registering one.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ReconciliationScopeDto {
    pub tenant_id: Uuid,
    pub gts_type_id: String,
}

impl ReconciliationScopeDto {
    /// The scope a request names, from the request's own parameters.
    pub(crate) fn of(tenant_id: Uuid, gts_type_id: &MeterTypeId) -> Self {
        Self {
            tenant_id,
            gts_type_id: gts_type_id.to_string(),
        }
    }
}

/// The branches of `quantity_summary`, as the contract's `oneOf` declares
/// them: exactly one is present, and which one is decided by the queried
/// meter's declared fold rather than by anything the caller sent.
///
/// `accrued_sum` and `latest_observation` are JSON **strings**, never numbers,
/// so neither round-trips through a float on a billing surface.
/// `latest_observation` carries no `skip_serializing_if`: the contract
/// requires the property on every `Observations`-branch body and fixes it at
/// `null` when the range selected nothing, so an absent field would be a
/// different, undocumented shape.
///
/// The contract declares no standalone `QuantitySummary` component, so every
/// field using this type also carries `#[schema(inline)]`.
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
                // `BigDecimal::Display`'s scale threshold (e.g. `3.4E-7`),
                // and `AggregatedQuantity`'s published pattern admits no
                // exponent form.
                accrued_sum: sum.to_plain_string(),
            },
            QuantitySummary::Observations(observed) => Self::Observations {
                observation_count: observed.as_ref().map_or(0, |o| o.count.get()),
                latest_observation: observed.map(|o| o.latest.to_string()),
            },
        }
    }
}

/// The contract's `ReconciliationMetadata`.
///
/// Not a page: the gear serves one scope per call and enumerates nothing, so
/// there is no `PageInfo`, no cursor and no page size.
///
/// The `scope` field is the tenant-and-GTS-type target these figures cover —
/// unrelated to the reporting *granularity* the wire query parameter also
/// spells `scope`, and unrelated again to the compiled PDP scope the gear
/// calls `scope` everywhere else; `domain::reconciliation`'s module doc
/// carries the warning in full.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ReconciliationMetadataDto {
    #[schema(inline)]
    pub scope: ReconciliationScopeDto,
    pub accepted_count: u64,
    #[schema(inline)]
    pub quantity_summary: QuantitySummaryDto,
    /// `null` when the scope holds no entries — **not omitted**: the
    /// contract lists this property `required`, so it carries no
    /// `skip_serializing_if`, unlike every optional field elsewhere in this
    /// module.
    pub accepted_at_watermark: Option<String>,
    /// `null` when the scope holds no entries — see
    /// [`Self::accepted_at_watermark`] for why this carries no
    /// `skip_serializing_if` either.
    pub window_end_watermark: Option<String>,
}

impl ReconciliationMetadataDto {
    /// Project a service-returned [`ReconciliationMetadata`] onto the wire
    /// shape, combined with the [`ReconciliationScopeDto`] the handler built
    /// from the request's own parameters — `ReconciliationMetadata` itself
    /// carries no scope.
    pub(crate) fn from_metadata(
        scope: ReconciliationScopeDto,
        metadata: ReconciliationMetadata,
    ) -> Self {
        Self {
            scope,
            accepted_count: metadata.accepted_count,
            quantity_summary: metadata.quantity_summary.into(),
            accepted_at_watermark: metadata.max_accepted_at.map(format_watermark),
            window_end_watermark: metadata.max_window_end.map(format_watermark),
        }
    }
}

/// Render a watermark as RFC 3339 text, falling back to `Display` on the
/// unreachable-in-practice failure mode `Rfc3339` has: a year outside
/// `0..10_000` or a non-zero-second offset, neither of which a value already
/// accepted onto a persisted watermark can carry.
fn format_watermark(ts: OffsetDateTime) -> String {
    ts.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| ts.to_string())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "dto_tests.rs"]
mod dto_tests;
