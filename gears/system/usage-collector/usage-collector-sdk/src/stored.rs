//! The Plugin SPI's meter reference and storage-shaped ledger entry.
//!
//! These two types are what [`crate::UsageCollectorPluginV1`] speaks.
//! Neither crosses a wire: `UsageRecord` is the SDK/domain model a REST
//! response deserializes into, and `UsageRecordDto` is the wire shape. The
//! deliberate absence of a `serde` derive below is what keeps those three
//! roles from being confused.

use std::collections::BTreeMap;

use uuid::Uuid;

use crate::models::{
    EntryType, IdempotencyKey, Invalidation, MetadataKey, MeterTypeId, RecordOrigin, ResourceRef,
    SubjectRef, UsageRecord,
};
use crate::quantity::UsageQuantity;

/// How a meter crosses the Plugin SPI inward.
///
/// # What a plugin may do with each field
///
/// [`Self::uuid`] **is** the meter's identity: the value a plugin persists,
/// keys on, indexes and partitions by. It is the `types-registry` *Registry
/// Reference* (`cpt-cf-types-registry-adr-storage-identity-query-model`),
/// which guarantees one identifier maps to one reference for the life of
/// the installation — across tenants, processes, deployments, imports and
/// restores. That immutability is what makes it safe as a storage key.
///
/// [`Self::id`] is **diagnostic**. A plugin MAY put it in a log line or an
/// error detail, so an operator reading a backend's own output sees a meter
/// name rather than an opaque UUID. A plugin MUST NOT persist it, index it,
/// key on it, or derive either value from the other — deriving in
/// particular is barred by the ADR above, which is also why this type
/// carries both rather than one and a conversion.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MeterRef {
    /// The registry reference — the identity.
    pub uuid: Uuid,
    /// The GTS identifier — diagnostic only. See the type's docs.
    pub id: MeterTypeId,
}

impl MeterRef {
    /// Pairs a reference with the identifier it was issued for.
    ///
    /// Neither value is checked against the other, and nothing here could
    /// check it without the derivation the ADR bars. A caller supplying a
    /// mismatched pair gets a mismatched pair; the gear's own callers take
    /// both from one resolved declaration, so they cannot disagree.
    #[must_use]
    pub const fn new(uuid: Uuid, id: MeterTypeId) -> Self {
        Self { uuid, id }
    }
}

/// A ledger entry as storage holds it.
///
/// Field-for-field [`UsageRecord`], with `gts_type_id: MeterTypeId`
/// replaced by [`Self::gts_type_uuid`]. It is the same type in **both**
/// directions across the SPI: storage shape and record shape are one, and
/// the meter rides inward as a call parameter instead.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredUsageRecord {
    /// Deterministic gateway-derived entry identity.
    ///
    /// **Unrelated to [`Self::gts_type_uuid`].** This is `UUIDv5` over the
    /// six-tuple dedup identity, whose meter component is the GTS
    /// *identifier*
    /// (`cpt-cf-usage-collector-adr-record-identity-derivation`,
    /// [`crate::derive_usage_record_id`]). The reference is not an input to
    /// it. Re-keying identity onto the reference would change every stored
    /// `id` in an installation, which is why the SDK's own tests pin this
    /// derivation against a static expectation.
    pub id: Uuid,
    /// The registry reference of the meter this entry is metered against —
    /// the value storage keys on. See [`MeterRef`].
    pub gts_type_uuid: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Resource attribution composite (mandatory).
    pub resource_ref: ResourceRef,
    /// Optional subject attribution composite.
    pub subject_ref: Option<SubjectRef>,
    /// Caller-supplied metadata, validated gateway-side.
    pub metadata: BTreeMap<MetadataKey, String>,
    /// The measured quantity, in the meter's canonical unit.
    pub quantity: UsageQuantity,
    /// Caller-supplied dedup key.
    pub idempotency_key: IdempotencyKey,
    /// Gear-assigned instant of acceptance.
    pub accepted_at: time::OffsetDateTime,
    /// Which ingestion route admitted this entry.
    pub origin: RecordOrigin,
    /// The withdrawal this entry carries, absent on an ordinary measurement.
    pub invalidation: Option<Invalidation>,
    /// Inclusive start of the covered period.
    pub window_start: time::OffsetDateTime,
    /// Exclusive end of the covered period — the bound every read selects on.
    pub window_end: time::OffsetDateTime,
}

impl StoredUsageRecord {
    /// This entry's kind, derived from the withdrawal it carries.
    ///
    /// Not a field, for [`UsageRecord::entry_type`]'s reason: a stored
    /// discriminator is a second place the kind can be read, and the two
    /// can disagree.
    #[must_use]
    pub const fn entry_type(&self) -> EntryType {
        if self.invalidation.is_some() {
            EntryType::Invalidation
        } else {
            EntryType::Record
        }
    }

    /// Whether `other` carries the same caller-supplied fields as `self`.
    ///
    /// The reference-keyed counterpart of [`UsageRecord::caller_supplied_eq`],
    /// and equivalent to it: comparing references decides exactly what
    /// comparing identifiers decided, because one identifier maps to one
    /// reference for the life of the installation. Every other field, and
    /// every field's classification, is that method's — read its doc
    /// comment for why `invalidation` is compared and the three
    /// server-assigned fields are not.
    ///
    /// Both sides are destructured, so a field added to
    /// [`StoredUsageRecord`] fails to compile here until it is classified.
    #[must_use]
    pub fn caller_supplied_eq(&self, other: &Self) -> bool {
        let Self {
            id: _,
            gts_type_uuid,
            tenant_id,
            resource_ref,
            subject_ref,
            metadata,
            quantity,
            idempotency_key,
            accepted_at: _,
            origin: _,
            invalidation,
            window_start,
            window_end,
        } = self;
        let Self {
            id: _,
            gts_type_uuid: other_gts_type_uuid,
            tenant_id: other_tenant_id,
            resource_ref: other_resource_ref,
            subject_ref: other_subject_ref,
            metadata: other_metadata,
            quantity: other_quantity,
            idempotency_key: other_idempotency_key,
            accepted_at: _,
            origin: _,
            invalidation: other_invalidation,
            window_start: other_window_start,
            window_end: other_window_end,
        } = other;
        gts_type_uuid == other_gts_type_uuid
            && tenant_id == other_tenant_id
            && resource_ref == other_resource_ref
            && subject_ref == other_subject_ref
            && metadata == other_metadata
            && quantity == other_quantity
            && idempotency_key == other_idempotency_key
            && invalidation == other_invalidation
            && window_start == other_window_start
            && window_end == other_window_end
    }

    /// Re-attaches `gts_type_id`, producing the domain model.
    ///
    /// The identifier is supplied by the caller rather than derived here:
    /// the gear takes it from the [`MeterRef`] the call carried, or — on
    /// the one SPI method that carries no meter — from its reverse
    /// resolver. Nothing in this crate can derive one from the other.
    ///
    /// **This method does not verify that `gts_type_id` corresponds to
    /// [`Self::gts_type_uuid`]**, and cannot. A caller re-attaching an
    /// identifier to a record returned by a plugin must check the reference
    /// it expected against the one it got *before* calling this, or it will
    /// relabel a foreign record rather than detect one.
    #[must_use]
    pub fn into_usage_record(self, gts_type_id: MeterTypeId) -> UsageRecord {
        let Self {
            id,
            gts_type_uuid: _,
            tenant_id,
            resource_ref,
            subject_ref,
            metadata,
            quantity,
            idempotency_key,
            accepted_at,
            origin,
            invalidation,
            window_start,
            window_end,
        } = self;
        UsageRecord {
            id,
            gts_type_id,
            tenant_id,
            resource_ref,
            subject_ref,
            metadata,
            quantity,
            idempotency_key,
            accepted_at,
            origin,
            invalidation,
            window_start,
            window_end,
        }
    }
}

impl UsageRecord {
    /// Replaces this record's identifier with the reference it was issued
    /// for, producing the shape the Plugin SPI speaks.
    ///
    /// The reference is supplied rather than derived, for
    /// [`StoredUsageRecord::into_usage_record`]'s reason. The gear's callers
    /// take it from `ResolvedDeclaration::type_uuid`, which came from
    /// `types-registry` itself.
    #[must_use]
    pub fn into_stored(self, gts_type_uuid: Uuid) -> StoredUsageRecord {
        let Self {
            id,
            gts_type_id: _,
            tenant_id,
            resource_ref,
            subject_ref,
            metadata,
            quantity,
            idempotency_key,
            accepted_at,
            origin,
            invalidation,
            window_start,
            window_end,
        } = self;
        StoredUsageRecord {
            id,
            gts_type_uuid,
            tenant_id,
            resource_ref,
            subject_ref,
            metadata,
            quantity,
            idempotency_key,
            accepted_at,
            origin,
            invalidation,
            window_start,
            window_end,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "stored_tests.rs"]
mod stored_tests;
