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
/// Two deliberate departures from the literal shape of
/// `docs/schemas/example.stored_volume.v1.schema.json`:
///
/// - `GtsTypeId` has no `FromStr`/`parse()` impl in the `gts` crate, so ids
///   are built with `GtsTypeId::try_new` rather than the `.parse()` calls
///   sketched in the task plan.
/// - `x-gts-traits` sits at the top level of the derived document rather
///   than nested inside its `allOf` overlay branch (the doc example's
///   shape). `GtsTypeSchema::extract_traits` — and so
///   `effective_traits()`, which walks each chain member's own `traits`
///   field populated once at construction — reads `x-gts-traits` from the
///   top level only; it does not recurse into `allOf` the way gts-rust's
///   admission-side `collect_traits_from_value` does. Confirmed against
///   `types-registry-sdk`'s own `models_tests.rs`, which never nests
///   `x-gts-traits` inside `allOf` either. See this task's final report for
///   why that is flagged as a concern rather than silently worked around.
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
