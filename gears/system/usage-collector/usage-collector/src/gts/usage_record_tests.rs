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
    let declared: BTreeSet<&str> = schema["x-gts-traits-schema"]["properties"]["aggregation_fold"]
        ["enum"]
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
