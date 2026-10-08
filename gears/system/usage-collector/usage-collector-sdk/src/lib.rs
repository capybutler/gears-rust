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
//! - [`MeterRef`] / [`StoredUsageRecord`] — what the Plugin SPI speaks. The
//!   meter crosses it as a *Registry Reference* rather than a GTS
//!   identifier; `MeterRef` carries the identifier alongside for
//!   diagnostics, which a plugin may log and MUST NOT persist.
//! - [`TimeRange`] — the validated `[from, to)` read-path range. Not a
//!   domain model: a read-path query parameter, never a persisted entity,
//!   which is exactly the distinction its accessor names
//!   (`lower_inclusive` / `upper_exclusive`, not `window_start` /
//!   `window_end`) keep separate from a record's covered period.
//! - [`feed`] — the usage feed's [`FeedPosition`], [`FeedStart`],
//!   [`FeedPage`] and [`FeedSubscription`], plus the
//!   [`MAX_FEED_POSITION_BYTES`] bound a plugin's position has to fit. The
//!   feed is the replay-safe read path a charging consumer uses instead of
//!   [`UsageCollectorClientV1::list_usage_records`]. [`FeedStart`] and
//!   [`FeedPage`] are generic over the position each surface speaks, which is
//!   what keeps a plugin's own position off the wire; the Plugin SPI reads a
//!   page and the consumer trait's
//!   [`UsageCollectorClientV1::read_usage_feed`] reads one over the wire
//!   cursor (see [`api`]).
//! - [`reconciliation`] — [`ReconciliationMetadata`], the accepted count,
//!   [`QuantitySummary`] and two watermarks one `(tenant, GTS type)` scope
//!   reports, and [`ReconciliationScope`], the REST-level spelling of that
//!   pair. `QuantitySummary` is one of two branches, decided by the declared
//!   fold: an accrued total for `SUM`, or an observation count with the
//!   latest observation for every other fold. An operator surface: it says
//!   whether a consumer has a gap without folding the meter.
//! - `contract` (feature-gated, off by default) — the DESIGN §3.3 plugin
//!   contract suite every conforming storage plugin MUST pass. A plugin
//!   crate enables the `contract` feature on its dev-dependency and calls
//!   `contract::run_all` against its own backend; see the module docs. The
//!   in-memory backend the suite is itself validated against is `cfg(test)`
//!   and is not part of this feature's surface.
//! - [`UsageCollectorError`] / [`UsageCollectorPluginError`] — flat error envelopes.
//!   This crate does NOT depend on `toolkit-canonical-errors`; the host crate
//!   owns the lift to RFC-9457 `Problem` on the REST surface.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod api;
#[cfg(feature = "contract")]
pub mod contract;
pub mod error;
pub mod feed;
pub mod gts;
pub mod id;
pub mod keyset;
pub mod models;
pub mod plugin_api;
pub mod quantity;
pub mod reason;
pub mod reconciliation;
pub mod serde_helpers;
pub mod stored;
pub mod time_range;

pub use api::UsageCollectorClientV1;
pub use error::{
    AlreadyInvalidatedArgs, ConflictOutcome, CursorField, IngestionQuotaExceededArgs,
    UsageCollectorError, UsageCollectorPluginError,
};
pub use feed::{
    FeedPage, FeedPosition, FeedPositionInvalid, FeedStart, FeedSubscription,
    FeedSubscriptionInvalid, MAX_FEED_POSITION_BYTES,
};
pub use gts::{USAGE_RECORD_RESOURCE, UsageCollectorPluginSpecV1};
pub use id::{USAGE_RECORD_ID_NAMESPACE, canonical_period_bound, derive_usage_record_id};
pub use keyset::{Keyset, KeysetInvalid, MAX_KEYSET_BYTES, RecordPage};
pub use models::{
    AggregationBucket, AggregationDimension, AggregationFold, AggregationResult,
    BACKFILL_ROUTE_PATH, CreateUsageRecord, EntryType, IdempotencyKey, Invalidation,
    KEYSET_SAFE_RECORD_FIELDS, MAX_AGGREGATION_BUCKETS, MetadataFilter, MetadataKey, MeterTypeId,
    PUBLISHED_FILTER_FIELDS, RECORD_ID_FIELD, ReasonCode, RecordOrigin, ResourceRef, SubjectRef,
    USAGE_RECORD_BASE_TYPE, UsageRecord, UsageRecordFilterField, UsageRecordQuery,
    WINDOW_END_FIELD, WINDOW_START_FIELD, is_keyset_safe_record_field,
};
pub use plugin_api::UsageCollectorPluginV1;
pub use quantity::{MAX_QUANTITY_SIGNIFICANT_DIGITS, UsageQuantity};
pub use reason::{ConflictReason, NotFoundReason, ValidationReason};
pub use reconciliation::{
    ObservedQuantity, QuantitySummary, ReconciliationMetadata, ReconciliationScope,
};
pub use stored::{MeterRef, StoredUsageRecord};
pub use time_range::TimeRange;
