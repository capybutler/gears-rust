#![allow(clippy::needless_pass_by_value)]

use serde_json::json;
use std::sync::Arc;

use types_registry_sdk::{GtsTypeId, GtsTypeSchema};
use usage_collector_sdk::{AggregationFold, MeterTypeId};

use super::ResolvedDeclaration;

const METER: &str = "gts.cf.core.uc.usage_record.v1~example.metering._.stored_volume.v1~";
const BASE: &str = "gts.cf.core.uc.usage_record.v1~";

/// Builds a base + derived chain exercising the same trait-merge path
/// production does: [`GtsTypeSchema::effective_traits`].
///
/// Departures from the literal shape of
/// `docs/schemas/example.stored_volume.v1.schema.json`:
///
/// - `GtsTypeId` has no `FromStr`/`parse()` impl in the `gts` crate, so ids
///   are built with `GtsTypeId::try_new` rather than the `.parse()` calls
///   sketched in the task plan.
/// - The `metadata` property override sits in this fixture's top-level
///   `properties`, rather than nested inside `allOf[1].properties` the way
///   the doc example places it. This one is harmless:
///   `GtsTypeSchema::effective_properties()` walks non-`$ref` `allOf` items
///   as well as the top level (unlike `extract_traits`, which does not), so
///   either placement merges the same way. `x-gts-traits` itself is *not* a
///   departure — both this fixture and the doc example place it at the top
///   level, which is the placement `extract_traits`/`effective_traits()`
///   actually reads (the doc example used to nest it in `allOf` and was
///   fixed in a follow-up commit).
fn schema_with_traits(traits: serde_json::Value) -> GtsTypeSchema {
    let base = GtsTypeSchema::try_new(
        GtsTypeId::try_new(BASE).expect("base type id"),
        json!({
            "type": "object",
            "x-gts-abstract": true,
            "properties": {
                "metadata": { "type": "object", "additionalProperties": { "type": "string" } }
            }
        }),
        None,
        None,
    )
    .expect("base schema");

    GtsTypeSchema::try_new(
        GtsTypeId::try_new(METER).expect("meter type id"),
        json!({
            "allOf": [
                { "$ref": format!("gts://{BASE}") }
            ],
            "x-gts-traits": traits,
            "properties": {
                "metadata": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "region": { "type": "string" },
                        "storage_class": { "type": "string" }
                    }
                }
            }
        }),
        None,
        Some(Arc::new(base)),
    )
    .expect("derived schema")
}

#[test]
fn parses_every_declared_trait() {
    let schema = schema_with_traits(json!({
        "aggregation_fold": "SUM",
        "canonical_unit": "byte-hours",
        "retention": "P125D",
        "nominal_sampling_interval": "PT1H"
    }));

    let decl = ResolvedDeclaration::from_schema(MeterTypeId::new(METER).unwrap(), &schema)
        .expect("declaration parses");

    assert_eq!(decl.aggregation_fold, AggregationFold::Sum);
    assert_eq!(decl.canonical_unit, "byte-hours");
    assert_eq!(decl.nominal_sampling_interval.as_deref(), Some("PT1H"));
    // Confirms `CompiledMetadataSchema::compile` actually reads the
    // declared `metadata` property names (`effective_properties()`
    // merged), not just that it returns without erroring.
    let declared: Vec<&str> = decl
        .metadata_schema
        .declared_keys()
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(declared, vec!["region", "storage_class"]);
}

#[test]
fn nominal_sampling_interval_is_optional() {
    let schema = schema_with_traits(json!({
        "aggregation_fold": "COUNT",
        "canonical_unit": "count",
        "retention": "P400D"
    }));

    let decl = ResolvedDeclaration::from_schema(MeterTypeId::new(METER).unwrap(), &schema).unwrap();

    assert_eq!(decl.aggregation_fold, AggregationFold::Count);
    assert!(decl.nominal_sampling_interval.is_none());
}

#[test]
fn rejects_a_declaration_binding_no_unit() {
    // DESIGN 3.2: "Rejects an entry whose type binds no unit."
    let schema = schema_with_traits(json!({
        "aggregation_fold": "SUM",
        "retention": "P125D"
    }));

    let err = ResolvedDeclaration::from_schema(MeterTypeId::new(METER).unwrap(), &schema)
        .expect_err("a declaration with no canonical_unit must not resolve");
    assert!(
        err.to_string().contains("canonical_unit"),
        "diagnostic must name the missing trait, got: {err}"
    );
}

#[test]
fn rejects_a_declaration_with_no_fold() {
    let schema = schema_with_traits(json!({
        "canonical_unit": "bytes",
        "retention": "P125D"
    }));

    let err = ResolvedDeclaration::from_schema(MeterTypeId::new(METER).unwrap(), &schema)
        .expect_err("a declaration with no aggregation_fold must not resolve");
    assert!(err.to_string().contains("aggregation_fold"));
}

#[test]
fn rejects_a_fold_of_the_wrong_json_type_naming_the_type_found() {
    // A trait present but not a string must not collapse into the same
    // "declares no `aggregation_fold`" diagnostic as a genuinely absent one
    // — an operator who wrote a number is hunting for the wrong defect.
    let schema = schema_with_traits(json!({
        "aggregation_fold": 5,
        "canonical_unit": "bytes",
        "retention": "P125D"
    }));

    let err = ResolvedDeclaration::from_schema(MeterTypeId::new(METER).unwrap(), &schema)
        .expect_err("a non-string aggregation_fold must not resolve");
    let msg = err.to_string();
    assert!(
        msg.contains("aggregation_fold") && msg.contains("a number") && !msg.contains("no `"),
        "diagnostic must say the trait is present but not a string, naming the type found, got: {err}"
    );
}

#[test]
fn rejects_a_canonical_unit_of_the_wrong_json_type_naming_the_type_found() {
    let schema = schema_with_traits(json!({
        "aggregation_fold": "SUM",
        "canonical_unit": ["bytes"],
        "retention": "P125D"
    }));

    let err = ResolvedDeclaration::from_schema(MeterTypeId::new(METER).unwrap(), &schema)
        .expect_err("a non-string canonical_unit must not resolve");
    let msg = err.to_string();
    assert!(
        msg.contains("canonical_unit") && msg.contains("an array") && !msg.contains("no `"),
        "diagnostic must say the trait is present but not a string, naming the type found, got: {err}"
    );
}

#[test]
fn rejects_an_unknown_fold_rather_than_substituting_one() {
    // 2.2 constraint-plugin-contract-stability: a fold the gear does not
    // implement is an error, never a substitution.
    let schema = schema_with_traits(json!({
        "aggregation_fold": "AVG",
        "canonical_unit": "bytes",
        "retention": "P125D"
    }));

    let err = ResolvedDeclaration::from_schema(MeterTypeId::new(METER).unwrap(), &schema)
        .expect_err("AVG is not a fold this version serves");
    assert!(
        err.to_string().contains("aggregation_fold"),
        "diagnostic must name the offending trait, got: {err}"
    );
    assert!(
        err.is_declaration_not_found(),
        "an unserved fold must fail closed the same way as a missing trait, got: {err:?}"
    );
}

#[test]
fn rejects_a_schema_carrying_no_traits_at_all() {
    let base = GtsTypeSchema::try_new(
        GtsTypeId::try_new(BASE).expect("base type id"),
        json!({ "type": "object" }),
        None,
        None,
    )
    .expect("base schema");

    let err = ResolvedDeclaration::from_schema(MeterTypeId::new(METER).unwrap(), &base)
        .expect_err("a schema with no x-gts-traits at all must not resolve");
    assert!(
        err.to_string().contains("aggregation_fold"),
        "the first missing-trait check (aggregation_fold) must fire, got: {err}"
    );
}
