//! Usage Collector SDK — public contract surfaces for the `usage-collector` gear:
//!
//! - [`UsageCollectorClientV1`] — consumer SDK trait, obtained from `ClientHub`.
//! - [`UsageCollectorPluginV1`] — storage plugin SPI trait.
//! - [`UsageCollectorPluginSpecV1`] — GTS plugin spec for discovery/binding.
//! - Domain models: [`UsageRecord`], [`MeterTypeId`], [`AggregationResult`],
//!   [`ResourceRef`], [`SubjectRef`], and the aggregation surface
//!   ([`AggregationDimension`], [`AggregationBucket`]). Every meter's fold,
//!   canonical unit and metadata surface is a declaration owned by
//!   `types-registry` and resolved by the host — this crate declares no
//!   usage-type catalog of its own.
//!   List pagination uses [`toolkit_odata::ODataQuery`]
//!   / [`toolkit_odata::Page`]. The filterable-field schema for
//!   `list_usage_records` is declared by [`UsageRecordQuery`] (macro-derived
//!   via `ODataFilterable`); dynamic metadata-key filtering rides a typed
//!   [`MetadataFilter`] side channel.
//! - [`TimeRange`] — the validated `[from, to)` read-path range. Not a
//!   domain model: a read-path query parameter, never a persisted entity,
//!   which is exactly the distinction its accessor names
//!   (`lower_inclusive` / `upper_exclusive`, not `window_start` /
//!   `window_end`) keep separate from a record's covered period.
//! - [`UsageCollectorError`] / [`UsageCollectorPluginError`] — flat error envelopes.
//!   This crate does NOT depend on `toolkit-canonical-errors`; the host crate
//!   owns the lift to RFC-9457 `Problem` on the REST surface.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod api;
pub mod error;
pub mod gts;
pub mod id;
pub mod models;
pub mod plugin_api;
pub mod reason;
pub mod serde_helpers;
pub mod time_range;

pub use api::UsageCollectorClientV1;
pub use error::{UsageCollectorError, UsageCollectorPluginError};
pub use gts::{USAGE_RECORD_RESOURCE, UsageCollectorPluginSpecV1};
pub use id::{USAGE_RECORD_ID_NAMESPACE, canonical_period_bound, derive_usage_record_id};
pub use models::{
    AggregationBucket, AggregationDimension, AggregationFold, AggregationResult,
    BACKFILL_ROUTE_PATH, CreateUsageRecord, EntryType, IdempotencyKey, Invalidation,
    KEYSET_SAFE_RECORD_FIELDS, MAX_AGGREGATION_BUCKETS, MetadataFilter, MetadataKey, MeterTypeId,
    RECORD_ID_FIELD, ReasonCode, RecordOrigin, ResourceRef, SubjectRef, USAGE_RECORD_BASE_TYPE,
    UsageRecord, UsageRecordFilterField, UsageRecordQuery, WINDOW_END_FIELD, WINDOW_START_FIELD,
    is_keyset_safe_record_field,
};
pub use plugin_api::UsageCollectorPluginV1;
pub use reason::{ConflictReason, NotFoundReason, ValidationReason};
pub use time_range::TimeRange;
