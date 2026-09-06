use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

use types_registry_sdk::{GtsTypeId, GtsTypeSchema};
use usage_collector_sdk::MeterTypeId;

use super::CompiledMetadataSchema;

const METER: &str = "gts.cf.core.uc.usage_record.v1~example.metering._.stored_volume.v1~";
const BASE: &str = "gts.cf.core.uc.usage_record.v1~";

fn meter_id() -> MeterTypeId {
    MeterTypeId::new(METER).expect("valid meter id")
}

/// Mirrors `docs/schemas/example.stored_volume.v1.schema.json`: the base admits
/// any string value, the derived type closes the key set.
///
/// `GtsTypeId` has no `FromStr`/`.parse()` impl in the `gts` crate (see
/// `declaration_tests.rs`), so ids are built with `GtsTypeId::try_new`.
fn stored_volume_schema() -> GtsTypeSchema {
    let base = GtsTypeSchema::try_new(
        GtsTypeId::try_new(BASE).expect("base type id"),
        json!({
            "type": "object",
            "properties": {
                "metadata": {
                    "type": "object",
                    "additionalProperties": { "type": "string" }
                }
            }
        }),
        None,
        None,
    )
    .unwrap();

    GtsTypeSchema::try_new(
        GtsTypeId::try_new(METER).expect("meter type id"),
        json!({
            "allOf": [
                { "$ref": format!("gts://{BASE}") },
                {
                    "properties": {
                        "metadata": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "region": { "type": "string", "minLength": 1, "maxLength": 64 },
                                "storage_class": { "type": "string", "minLength": 1, "maxLength": 64 }
                            }
                        }
                    }
                }
            ]
        }),
        None,
        Some(Arc::new(base)),
    )
    .unwrap()
}

fn metadata(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[test]
fn declared_keys_are_exactly_the_derived_properties() {
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &stored_volume_schema()).unwrap();
    let keys: Vec<&str> = compiled
        .declared_keys()
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, vec!["region", "storage_class"]);
}

#[test]
fn accepts_metadata_using_only_declared_keys() {
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &stored_volume_schema()).unwrap();
    compiled
        .validate(&metadata(&[
            ("region", "eu-west-1"),
            ("storage_class", "cold"),
        ]))
        .expect("declared keys accepted");
}

#[test]
fn accepts_a_subset_of_declared_keys() {
    // The derived schema declares no `required`, so a subset is well-formed.
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &stored_volume_schema()).unwrap();
    compiled
        .validate(&metadata(&[("region", "eu-west-1")]))
        .expect("subset accepted");
}

#[test]
fn accepts_empty_metadata() {
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &stored_volume_schema()).unwrap();
    compiled.validate(&BTreeMap::new()).expect("empty accepted");
}

#[test]
fn rejects_an_undeclared_key_and_names_it() {
    // 3.1 closed metadata shape: no free-form remainder, no escape hatch.
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &stored_volume_schema()).unwrap();
    let err = compiled
        .validate(&metadata(&[("region", "eu-west-1"), ("tier", "gold")]))
        .expect_err("an undeclared key must be rejected before persistence");
    assert!(
        err.to_string().contains("tier"),
        "diagnostic must name the offending key, got: {err}"
    );
}

#[test]
fn rejects_a_value_violating_a_declared_constraint() {
    // The subschema constrains minLength, so an empty value is not merely an
    // odd string: it is outside the declared surface.
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &stored_volume_schema()).unwrap();
    assert!(compiled.validate(&metadata(&[("region", "")])).is_err());
}

#[test]
fn a_meter_declaring_no_metadata_property_admits_no_keys() {
    // A meter that declares no metadata surface has an empty one. Admitting
    // arbitrary keys would reopen the closed shape the base only half-closes.
    let base = GtsTypeSchema::try_new(
        GtsTypeId::try_new(BASE).expect("base type id"),
        json!({ "type": "object" }),
        None,
        None,
    )
    .unwrap();
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &base).unwrap();

    assert!(compiled.declared_keys().is_empty());
    compiled.validate(&BTreeMap::new()).expect("empty accepted");
    assert!(compiled.validate(&metadata(&[("anything", "x")])).is_err());
}

#[test]
fn rejects_a_metadata_subschema_that_does_not_compile() {
    let base = GtsTypeSchema::try_new(
        GtsTypeId::try_new(BASE).expect("base type id"),
        json!({
            "type": "object",
            "properties": {
                "metadata": { "type": "object", "properties": { "x": { "type": 42 } } }
            }
        }),
        None,
        None,
    )
    .unwrap();

    assert!(CompiledMetadataSchema::compile(&meter_id(), &base).is_err());
}

#[test]
fn reports_every_violation_at_once_not_only_the_first() {
    // A caller correcting a payload should see every problem in one
    // round-trip: an undeclared key AND a declared-constraint violation on a
    // different key, both reported together rather than only whichever
    // `jsonschema` happens to find first.
    //
    // `jsonschema`'s `ValidationError` Display does not echo the instance
    // path, so the `minLength` violation on `region` cannot be told apart by
    // key name in the message; its distinctive wording ("shorter than 1
    // character") is asserted instead, alongside the `additionalProperties`
    // violation naming `tier`.
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &stored_volume_schema()).unwrap();
    let err = compiled
        .validate(&metadata(&[("region", ""), ("tier", "gold")]))
        .expect_err("both violations must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("tier"),
        "diagnostic must name the undeclared key, got: {err}"
    );
    assert!(
        msg.contains("shorter than 1 character"),
        "diagnostic must also report the declared-constraint violation on `region`, got: {err}"
    );
    assert!(
        msg.contains("; "),
        "multiple violations must be joined, not collapsed to just one, got: {err}"
    );
}
