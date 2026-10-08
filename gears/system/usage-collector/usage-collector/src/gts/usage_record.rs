//! The reserved ingestion base type.
//!
//! `gts.cf.core.uc.usage_record.v1~` is the type every usage-collector
//! permission instance and error envelope names as `resource_type`
//! (`usage_collector_sdk::USAGE_RECORD_RESOURCE`). Registering it here is
//! load-bearing rather than cosmetic: `gts_store.rs`'s `validate_schema` /
//! `resolve_schema_refs` walk `GtsId::chain_ids()` and fail on an absent
//! base, and every concrete meter id in this gear derives from
//! `…usage_record.v1~`, so without this registration no meter type can
//! register at all.
//!
//! ## What this struct does, and does not, reproduce
//!
//! The authoritative structural contract for an accepted entry —
//! `oneOf`-discriminated `record`/`invalidation` shapes, `$ref`-linked
//! `ResourceRef`/`SubjectRef` — is hand-authored at
//! `docs/schemas/usage_record.v1.schema.json`, which is documentation only:
//! nothing under this gear's `src/` reads it. At runtime,
//! `domain::type_resolver::metadata::CompiledMetadataSchema::compile`
//! validates entries against a [`types_registry_sdk::GtsTypeSchema`] obtained
//! from `types-registry` — and the document `types-registry` serves back for
//! this id *is* the one `#[gts_type_schema]` emits below, aggregated from the
//! link-time inventory at boot. So this struct is the base layer of the real
//! contract and must not assert anything the runtime premise contradicts.
//!
//! `#[gts_type_schema]` ingests no external JSON file — every schema it emits
//! is generated from an annotated Rust struct via `schemars` — so a top-level
//! `oneOf` and the `$ref`-linked nested types are out of reach here. Modeling
//! them would mean inventing parallel `ResourceRef`/`SubjectRef` types
//! deriving `schemars::JsonSchema`, which neither the DTOs (OpenAPI-facing)
//! nor the SDK models (deliberately `utoipa`-free) are — a second, partial,
//! drifting copy of the contract. What this struct *does* reproduce is the one
//! fact `type_resolver` depends on structurally: `type_resolver/metadata.rs`
//! states *"the base type always declares an open `metadata`
//! (`additionalProperties: {"type": "string"}`) …so a lookup for the key is
//! never absent"*, so `metadata` below emits exactly that open shape.
//!
//! ## `x-gts-traits-schema`
//!
//! **Declared**, via `traits_schema = inline(UsageRecordTraitsV1)`, mirroring
//! the hand-authored contract's `aggregation_fold` / `canonical_unit` /
//! `retention` / `nominal_sampling_interval`. No `traits = …` defaults are
//! supplied and none are needed: `gts`'s `GtsStore::validate_schema` calls
//! `traits.validate(!is_abstract)` and skips the required-trait completeness
//! loop in the abstract case, so an abstract base declaring required traits
//! with no defaults validates cleanly and the completeness check applies to
//! the concrete meter that closes them. Pinned by execution in
//! `tests::a_derived_meter_carrying_x_gts_traits_is_admitted_by_gts_validate_schema`.
//!
//! Without this trait schema every meter type in this gear fails
//! `types-registry` admission (`InvalidSchema`, *"x-gts-traits values provided
//! but no x-gts-traits-schema is defined in the inheritance chain"*), because
//! `ResolvedDeclaration::from_schema` reads `x-gts-traits` merged across the
//! chain on every meter.
//!
//! ## The `id` field
//!
//! `base = true` structs must declare an identity field (`gts-macros`'s
//! `validate_base_struct_fields`). `id: GtsInstanceId` here is inert: the real
//! schema's `id` is a server-derived entry UUID (`format: uuid`), not a GTS
//! instance identifier — the two share a name and nothing else. Unlike
//! `metadata`, `id` stays in `required` below, because the hand-authored
//! contract's own `required` list opens with it.
//!
//! `metadata` is deliberately excluded from `required` via
//! `#[schemars(extend("required" = [...]))]`: without the override `schemars`
//! would derive `required: ["id", "metadata"]` from field optionality alone
//! (both fields are non-`Option`), publishing `metadata` as required under the
//! real contract's `$id` even though that contract does not require it and it
//! is optional on the wire.

use std::collections::BTreeMap;

use toolkit_gts::{GtsTraitsSchema, gts_type_schema};

/// The reserved, abstract ingestion base type.
///
/// GTS Type Identifier: `gts.cf.core.uc.usage_record.v1~`
#[derive(Debug)]
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.uc.usage_record.v1~"),
    description = "Abstract base type for every meter the Usage Collector \
                    accepts entries against",
    properties = "id,metadata",
    base = true,
    gts_abstract = true,
    traits_schema = inline(UsageRecordTraitsV1)
)]
#[schemars(extend("required" = ["id"]))]
pub struct UsageRecordV1 {
    /// Required by `gts-macros`' base-struct contract; inert — see the
    /// module doc's "The `id` field" section.
    pub id: gts::GtsInstanceId,
    /// The per-meter extension surface. Open (`additionalProperties: {"type":
    /// "string"}`) so the base's own definition is never absent — see the
    /// module doc. A derived meter type closes the set by overriding this
    /// property with its own `additionalProperties: false` declaration.
    /// Excluded from `required` — see the module doc's "The `id` field"
    /// section.
    pub metadata: BTreeMap<String, String>,
}

/// Trait values a concrete meter type declares, mirrored from
/// `docs/schemas/usage_record.v1.schema.json`'s `x-gts-traits-schema`.
/// Schema-emission only: nothing in the gear constructs or parses this type
/// directly — it exists so `#[gts_type_schema]`'s `traits_schema =
/// inline(...)` can embed the trait subschema into the registered base
/// document. See the module doc, "`x-gts-traits-schema`".
#[derive(Debug, serde::Serialize, schemars::JsonSchema, GtsTraitsSchema)]
#[serde(deny_unknown_fields)]
pub struct UsageRecordTraitsV1 {
    /// The single aggregation this meter declares.
    pub aggregation_fold: AggregationFoldTrait,
    /// The canonical metering unit bound to this meter.
    pub canonical_unit: CanonicalUnitTrait,
    /// ISO 8601 duration: how long entries of this meter are retained.
    pub retention: String,
    /// ISO 8601 duration, informational only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nominal_sampling_interval: Option<String>,
}

/// Wire literals mirror the hand-authored contract's `aggregation_fold` enum
/// exactly.
#[allow(dead_code)] // schema-emission only; schemars reads the variants reflectively
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub enum AggregationFoldTrait {
    #[serde(rename = "SUM")]
    Sum,
    #[serde(rename = "COUNT")]
    Count,
    #[serde(rename = "MAX")]
    Max,
    #[serde(rename = "MIN")]
    Min,
    #[serde(rename = "LATEST")]
    Latest,
}

/// Wire literals mirror the hand-authored contract's `canonical_unit` enum
/// exactly.
#[allow(dead_code)] // schema-emission only; schemars reads the variants reflectively
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub enum CanonicalUnitTrait {
    #[serde(rename = "bytes")]
    Bytes,
    #[serde(rename = "byte-hours")]
    ByteHours,
    #[serde(rename = "count")]
    Count,
    #[serde(rename = "seconds")]
    Seconds,
}

#[cfg(test)]
#[path = "usage_record_tests.rs"]
mod usage_record_tests;
