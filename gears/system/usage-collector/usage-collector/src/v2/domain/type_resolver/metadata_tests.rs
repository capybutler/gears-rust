use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

use types_registry_sdk::{GtsTypeId, GtsTypeSchema};
use usage_collector_sdk::v2::MeterTypeId;

use super::{CompiledMetadataSchema, MetadataViolation};

const METER: &str = "gts.cf.core.uc.usage_record.v2~example.metering._.stored_volume.v1~";
const BASE: &str = "gts.cf.core.uc.usage_record.v2~";

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

/// The violation set for `pairs`, panicking only on the unreachable
/// serialization failure. Empty means valid.
fn violations_for(
    compiled: &CompiledMetadataSchema,
    pairs: &[(&str, &str)],
) -> Vec<MetadataViolation> {
    compiled
        .validate(&metadata(pairs))
        .expect("a string-keyed string map always serializes")
}

/// The violations rendered the way the caller joins them for a diagnostic.
fn rendered(violations: &[MetadataViolation]) -> String {
    violations
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

#[test]
fn accepts_metadata_using_only_declared_keys() {
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &stored_volume_schema()).unwrap();
    assert_eq!(
        violations_for(
            &compiled,
            &[("region", "eu-west-1"), ("storage_class", "cold")]
        ),
        vec![],
        "declared keys accepted"
    );
}

#[test]
fn accepts_a_subset_of_declared_keys() {
    // The derived schema declares no `required`, so a subset is well-formed.
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &stored_volume_schema()).unwrap();
    assert_eq!(
        violations_for(&compiled, &[("region", "eu-west-1")]),
        vec![],
        "subset accepted"
    );
}

#[test]
fn accepts_empty_metadata() {
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &stored_volume_schema()).unwrap();
    assert_eq!(
        compiled.validate(&BTreeMap::new()).expect("infallible"),
        vec![],
        "empty accepted"
    );
}

#[test]
fn rejects_an_undeclared_key_and_names_it() {
    // 3.1 closed metadata shape: no free-form remainder, no escape hatch.
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &stored_volume_schema()).unwrap();
    let found = violations_for(&compiled, &[("region", "eu-west-1"), ("tier", "gold")]);
    assert_eq!(
        found,
        vec![MetadataViolation::UndeclaredKey {
            key: "tier".to_owned()
        }],
        "the undeclared key must be reported as such, by name: the kind is \
         what lets the caller pick UNKNOWN_METADATA_KEY over the catch-all"
    );
    assert!(
        rendered(&found).contains("tier"),
        "diagnostic must name the offending key, got: {}",
        rendered(&found)
    );
}

#[test]
fn rejects_a_value_violating_a_declared_constraint() {
    // The subschema constrains minLength, so an empty value is not merely an
    // odd string: it is outside the declared surface.
    //
    // Reported as a ValueConstraint, NOT an UndeclaredKey: `region` IS
    // declared. The distinction is the whole point of the typed violation —
    // it is what keeps this case off `UNKNOWN_METADATA_KEY`.
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &stored_volume_schema()).unwrap();
    let found = violations_for(&compiled, &[("region", "")]);
    assert!(
        matches!(
            found.as_slice(),
            [MetadataViolation::ValueConstraint { .. }]
        ),
        "expected exactly one ValueConstraint violation, got {found:?}"
    );
}

/// Mirrors the *real* `docs/schemas/usage_record.v2.schema.json` base: an
/// open `metadata` (`additionalProperties: {"type": "string"}`, no
/// `properties`). This is the shape every meter actually inherits from —
/// unlike the earlier synthetic case below, this one is not hypothetical.
fn realistic_base_schema() -> GtsTypeSchema {
    GtsTypeSchema::try_new(
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
    .unwrap()
}

#[test]
fn a_meter_with_no_metadata_override_inherits_the_open_base_and_admits_no_keys() {
    // `GtsTypeSchema::effective_properties` resolves `metadata` by
    // *override*, not by intersection: a meter that supplies no closing
    // override does not get "no metadata property at all" (which `compile`
    // could special-case) — it inherits the base's open definition
    // verbatim. Closure must therefore come from `declared_keys` being
    // empty and `validate` enforcing it directly, not from anything the
    // inherited subschema itself says.
    let base = realistic_base_schema();
    let meter = GtsTypeSchema::try_new(
        GtsTypeId::try_new(METER).expect("meter type id"),
        json!({
            "allOf": [
                { "$ref": format!("gts://{BASE}") }
            ]
        }),
        None,
        Some(Arc::new(base)),
    )
    .unwrap();
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &meter).unwrap();

    assert!(compiled.declared_keys().is_empty());
    assert_eq!(
        compiled.validate(&BTreeMap::new()).expect("infallible"),
        vec![],
        "empty accepted"
    );
    assert_eq!(
        violations_for(&compiled, &[("anything", "x")]),
        vec![MetadataViolation::UndeclaredKey {
            key: "anything".to_owned()
        }],
        "no metadata override means no admissible key, not an open one"
    );
}

#[test]
fn closure_does_not_depend_on_additional_properties_false_being_set() {
    // The derived override below declares `properties` but omits
    // `additionalProperties: false` (JSON Schema defaults it to open). If
    // closure were still delegated to the compiled schema, an undeclared
    // key would slip through. It must not: `validate` rejects it itself,
    // regardless of what the subschema's own keywords say.
    let base = realistic_base_schema();
    let meter = GtsTypeSchema::try_new(
        GtsTypeId::try_new(METER).expect("meter type id"),
        json!({
            "allOf": [
                { "$ref": format!("gts://{BASE}") },
                {
                    "properties": {
                        "metadata": {
                            "type": "object",
                            "properties": {
                                "region": { "type": "string" }
                            }
                        }
                    }
                }
            ]
        }),
        None,
        Some(Arc::new(base)),
    )
    .unwrap();
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &meter).unwrap();

    assert_eq!(
        compiled
            .declared_keys()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["region"]
    );
    assert_eq!(
        violations_for(&compiled, &[("region", "eu-west-1"), ("tier", "gold")]),
        vec![MetadataViolation::UndeclaredKey {
            key: "tier".to_owned()
        }],
        "an undeclared key must be rejected even without additionalProperties: false"
    );
}

#[test]
fn a_schema_chain_with_no_metadata_key_at_all_gets_an_empty_closed_default() {
    // Defensive/synthetic case, not the production-representative one (see
    // the two tests above for that): a chain that declares no `metadata`
    // key anywhere hits `compile`'s `None` default branch directly. Every
    // real usage_record-derived declaration inherits the base's open
    // `metadata`, so this branch does not fire in practice — but it is
    // harmless to keep closed anyway.
    let base = GtsTypeSchema::try_new(
        GtsTypeId::try_new(BASE).expect("base type id"),
        json!({ "type": "object" }),
        None,
        None,
    )
    .unwrap();
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &base).unwrap();

    assert!(compiled.declared_keys().is_empty());
    assert_eq!(
        compiled.validate(&BTreeMap::new()).expect("infallible"),
        vec![],
        "empty accepted"
    );
    assert_eq!(
        violations_for(&compiled, &[("anything", "x")]),
        vec![MetadataViolation::UndeclaredKey {
            key: "anything".to_owned()
        }]
    );
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
    let found = violations_for(&compiled, &[("region", ""), ("tier", "gold")]);
    assert_eq!(
        found.len(),
        2,
        "both violations must be collected, got {found:?}"
    );
    // Both KINDS are present, which is what makes this a mixed set and so
    // disqualifies it from `UNKNOWN_METADATA_KEY`.
    assert!(
        found
            .iter()
            .any(|v| matches!(v, MetadataViolation::UndeclaredKey { key } if key == "tier")),
        "the undeclared key must be reported by name, got {found:?}"
    );
    assert!(
        found
            .iter()
            .any(|v| matches!(v, MetadataViolation::ValueConstraint { .. })),
        "the declared-constraint violation on `region` must also be reported, got {found:?}"
    );
    let msg = rendered(&found);
    assert!(
        msg.contains("tier"),
        "diagnostic must name the undeclared key, got: {msg}"
    );
    assert!(
        msg.contains("shorter than 1 character"),
        "diagnostic must also report the declared-constraint violation on `region`, got: {msg}"
    );
    assert!(
        msg.contains("; "),
        "multiple violations must be joined, not collapsed to just one, got: {msg}"
    );
}
