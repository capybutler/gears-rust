//! The reserved ingestion base type.
//!
//! `gts.cf.core.uc.usage_record.v2~` is the type every usage-collector
//! permission instance and error envelope names as `resource_type`
//! (`usage_collector_sdk::v2::USAGE_RECORD_RESOURCE`). Registering it here is
//! load-bearing: `gts_store.rs`'s `validate_schema` / `resolve_schema_refs`
//! walk `GtsId::chain_ids()` and fail on an absent base, and every concrete
//! meter id in this gear derives from `…usage_record.v2~`, so without this
//! registration no meter type can register at all.
//!
//! The authoritative structural contract for an accepted entry is
//! hand-authored at `docs/schemas/usage_record.v2.schema.json`, which is
//! documentation only — nothing under `src/` reads it. `#[gts_type_schema]`
//! generates the registered document from the annotated struct below via
//! `schemars`, so the contract's top-level `oneOf` and `$ref`-linked nested
//! types are out of reach here. What this struct must reproduce is the one
//! fact `domain::type_resolver::metadata` depends on structurally: the base
//! always declares an open `metadata`
//! (`additionalProperties: {"type": "string"}`), so a lookup for a key is
//! never absent.
//!
//! `traits_schema = inline(UsageRecordTraitsV2)` declares the
//! `x-gts-traits-schema` the chain needs. Without it every meter type in this
//! gear fails `types-registry` admission (`InvalidSchema`, *"x-gts-traits
//! values provided but no x-gts-traits-schema is defined in the inheritance
//! chain"*). No `traits = …` defaults are supplied and none are needed: an
//! abstract base declaring required traits with no defaults validates
//! cleanly, and the completeness check applies to the concrete meter that
//! closes them.
//!
//! `base = true` structs must declare an identity field, so `id` is present
//! and inert: the real schema's `id` is a server-derived entry UUID, not a GTS
//! instance identifier. `metadata` is excluded from `required` via
//! `#[schemars(extend("required" = [...]))]` because `schemars` would
//! otherwise derive it as required from field optionality alone, publishing a
//! requirement the real contract does not state.

use std::collections::BTreeMap;

use toolkit_gts::{GtsTraitsSchema, gts_type_schema};

/// The reserved, abstract ingestion base type.
///
/// GTS Type Identifier: `gts.cf.core.uc.usage_record.v2~`
#[derive(Debug)]
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.uc.usage_record.v2~"),
    description = "Abstract base type for every meter the Usage Collector \
                    accepts entries against",
    properties = "id,metadata",
    base = true,
    gts_abstract = true,
    traits_schema = inline(UsageRecordTraitsV2)
)]
#[schemars(extend("required" = ["id"]))]
pub struct UsageRecordV2 {
    /// Required by `gts-macros`' base-struct contract; inert — see the module
    /// doc.
    pub id: gts::GtsInstanceId,
    /// The per-meter extension surface. Open (`additionalProperties: {"type":
    /// "string"}`) so the base's own definition is never absent. A derived
    /// meter type closes the set by overriding this
    /// property with its own `additionalProperties: false` declaration.
    /// Excluded from `required` — see the module doc.
    pub metadata: BTreeMap<String, String>,
}

/// Trait values a concrete meter type declares, mirrored from
/// `docs/schemas/usage_record.v2.schema.json`'s `x-gts-traits-schema`.
/// Schema-emission only: nothing in the gear constructs or parses this type
/// directly — it exists so `#[gts_type_schema]`'s `traits_schema =
/// inline(...)` can embed the trait subschema into the registered base
/// document.
#[derive(Debug, serde::Serialize, schemars::JsonSchema, GtsTraitsSchema)]
#[serde(deny_unknown_fields)]
pub struct UsageRecordTraitsV2 {
    /// The single aggregation this meter declares.
    pub aggregation_fold: AggregationFoldTrait,
    /// The canonical metering unit bound to this meter.
    // Length-bounded only: the published list below is guidance, and
    // registration-time validation that a unit belongs to it is not
    // implemented yet. Plain `//` so this deferral note stays out of the
    // `description` the registry serves operators.
    #[schemars(length(min = 1, max = 64))]
    #[schemars(extend("examples" = ["bytes", "byte-hours", "count", "seconds"]))]
    pub canonical_unit: String,
    /// ISO 8601 duration: how long entries of this meter are retained.
    pub retention: String,
    /// ISO 8601 duration, informational only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nominal_sampling_interval: Option<String>,
}

/// Wire literals mirror the hand-authored contract's `aggregation_fold` enum
/// exactly.
#[expect(
    dead_code,
    reason = "schema-emission only; `schemars` reads the variants reflectively, so \
              nothing in the gear ever constructs one"
)]
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

#[cfg(test)]
#[path = "usage_record_tests.rs"]
mod usage_record_tests;
