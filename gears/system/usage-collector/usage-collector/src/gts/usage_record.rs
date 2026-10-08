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
mod tests {
    use std::collections::BTreeSet;

    use usage_collector_sdk::{AggregationFold, USAGE_RECORD_RESOURCE};

    /// `$id` form every registered `InventoryTypeSchema` document carries
    /// (`gts-macros`'s `GtsSchema::SCHEMA_ID` rendering), used to find this
    /// one entry among every schema linked into the process.
    fn want_id() -> serde_json::Value {
        serde_json::json!(format!("gts://{USAGE_RECORD_RESOURCE}"))
    }

    /// The oracle is `all_inventory_type_schemas()` — the parsed link-time
    /// inventory, not a type-id string list: asserting against
    /// `USAGE_RECORD_RESOURCE` would pass with no registration at all, since
    /// that constant exists independently of this file. Reading the parsed
    /// inventory also proves the emitted document is valid JSON. Removing the
    /// `#[gts_type_schema]` registration fails here with every other schema's
    /// `$id` listed and this one absent.
    #[test]
    fn the_ingestion_resource_type_is_registered_in_the_link_time_inventory() {
        let schemas = toolkit_gts::all_inventory_type_schemas().expect("schemas parse cleanly");
        assert!(
            schemas.iter().any(|s| s["$id"] == want_id()),
            "gts.cf.core.uc.usage_record.v1~ must reach the inventory; got $ids {:?}",
            schemas.iter().map(|s| s["$id"].clone()).collect::<Vec<_>>()
        );
    }

    /// Guards the one structural fact `domain::type_resolver::metadata`
    /// depends on: the base type "always declares an open `metadata`
    /// (`additionalProperties: {"type": "string"}`)…so a lookup for the key is
    /// never absent." A closed or absent `metadata` subschema would make that
    /// false for the document `types-registry` serves back for this id — which
    /// is what a `properties = ""` registration would produce.
    #[test]
    fn the_registered_document_is_abstract_with_an_open_metadata_surface() {
        let schemas = toolkit_gts::all_inventory_type_schemas().expect("schemas parse cleanly");
        let schema = schemas
            .iter()
            .find(|s| s["$id"] == want_id())
            .expect("usage_record base schema registered");
        assert_eq!(
            schema["x-gts-abstract"],
            serde_json::json!(true),
            "base must be x-gts-abstract: true, matching the hand-authored \
             contract: {schema}"
        );
        assert_eq!(
            schema["properties"]["metadata"]["additionalProperties"],
            serde_json::json!({"type": "string"}),
            "base's `metadata` must stay open (additionalProperties: \
             {{\"type\": \"string\"}}) — domain::type_resolver::metadata \
             depends on it structurally: {schema}"
        );
    }

    /// `metadata` must not publish as `required` under the real contract's
    /// `$id`: the hand-authored contract does not require it, and it is
    /// optional on the wire. Without `#[schemars(extend("required" =
    /// ["id"]))]`, `schemars` derives `required` from field optionality alone
    /// — both fields are non-`Option` — and publishes `["id", "metadata"]`,
    /// which is what removing that line fails here with.
    #[test]
    fn metadata_is_not_published_as_required() {
        let schemas = toolkit_gts::all_inventory_type_schemas().expect("schemas parse cleanly");
        let schema = schemas
            .iter()
            .find(|s| s["$id"] == want_id())
            .expect("usage_record base schema registered");
        let required = schema["required"].as_array().expect("required is an array");
        assert!(
            !required.contains(&serde_json::json!("metadata")),
            "metadata must not be required — the hand-authored contract \
             under this $id does not require it: {schema}"
        );
    }

    /// Verifies `traits_schema` by execution, not inspection: builds a real
    /// `gts::GtsStore`, registers this base schema plus a synthetic derived
    /// meter schema carrying `x-gts-traits`, and asserts `validate_schema` on
    /// the derived id returns `Ok`. Without `traits_schema =
    /// inline(UsageRecordTraitsV1)` on `UsageRecordV1` this fails with
    /// `StoreError::ValidationError` containing "x-gts-traits values provided
    /// but no x-gts-traits-schema is defined in the inheritance chain".
    ///
    /// The derived fixture puts `x-gts-traits` at the schema **top level**
    /// rather than nested inside `allOf`, unlike
    /// `docs/schemas/example.stored_volume.v1.schema.json`: that file's nested
    /// placement is independently rejected by `gts`'s `validate_gts_keywords`,
    /// a pre-existing defect in the published example.
    #[test]
    fn a_derived_meter_carrying_x_gts_traits_is_admitted_by_gts_validate_schema() {
        let schemas = toolkit_gts::all_inventory_type_schemas().expect("schemas parse cleanly");
        let base_schema = schemas
            .iter()
            .find(|s| s["$id"] == want_id())
            .expect("usage_record base schema registered")
            .clone();

        let mut store = gts::GtsStore::new();
        store
            .register_schema(USAGE_RECORD_RESOURCE, &base_schema)
            .expect("base schema registers in a fresh store");

        let derived_id = format!("{USAGE_RECORD_RESOURCE}example.metering._.stored_volume.v1~");
        let derived_schema = serde_json::json!({
            "$id": format!("gts://{derived_id}"),
            "$schema": "http://json-schema.org/draft-07/schema#",
            "x-gts-final": true,
            "x-gts-traits": {
                "aggregation_fold": "SUM",
                "canonical_unit": "byte-hours",
                "retention": "P125D",
                "nominal_sampling_interval": "PT1H"
            },
            "allOf": [
                { "$ref": format!("gts://{USAGE_RECORD_RESOURCE}") },
                {
                    "properties": {
                        "metadata": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "region": { "type": "string" }
                            }
                        }
                    }
                }
            ]
        });
        store
            .register_schema(&derived_id, &derived_schema)
            .expect("derived schema registers in a fresh store");

        let result = store.validate_schema(&derived_id);
        assert!(
            result.is_ok(),
            "derived meter schema carrying x-gts-traits must be admitted by \
             gts's own validate_schema (OP#12 derivation + OP#13 traits); \
             got {result:?}"
        );
    }

    /// Guards an asymmetric drift hazard between `AggregationFoldTrait` (this
    /// module, compiled into the registered `x-gts-traits-schema`) and
    /// `usage_collector_sdk::AggregationFold` (the gear's own wire type): the
    /// SDK gaining a fold the trait enum lacks fails registration loudly, but
    /// the trait enum gaining one the SDK lacks would let a meter type
    /// register successfully and then fail every ingestion against it in
    /// `ResolvedDeclaration::from_schema`'s `aggregation_fold` parse arm with
    /// `declaration_incomplete` — a type admitted to the registry that no
    /// entry could ever be metered against.
    ///
    /// `CanonicalUnitTrait` has no equivalent pin: the gear carries
    /// `canonical_unit` as an opaque `String`, so this trait enum is the sole
    /// enforcement point and there is nothing to drift from.
    #[test]
    fn aggregation_fold_trait_enum_matches_the_sdks_wire_set() {
        let schemas = toolkit_gts::all_inventory_type_schemas().expect("schemas parse cleanly");
        let schema = schemas
            .iter()
            .find(|s| s["$id"] == want_id())
            .expect("usage_record base schema registered");
        let declared: BTreeSet<&str> =
            schema["x-gts-traits-schema"]["properties"]["aggregation_fold"]["enum"]
                .as_array()
                .expect("aggregation_fold enum is an array")
                .iter()
                .map(|v| v.as_str().expect("enum member is a string"))
                .collect();
        let sdk: BTreeSet<&str> = [
            AggregationFold::Sum,
            AggregationFold::Count,
            AggregationFold::Max,
            AggregationFold::Min,
            AggregationFold::Latest,
        ]
        .iter()
        .map(|f| f.as_str())
        .collect();
        assert_eq!(
            declared, sdk,
            "the registered base's aggregation_fold trait enum must match \
             usage_collector_sdk::AggregationFold's wire set exactly, or a \
             meter type could register against a fold no entry can ever \
             carry"
        );
    }
}
