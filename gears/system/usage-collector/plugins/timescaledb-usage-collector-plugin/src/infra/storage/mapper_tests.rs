use std::collections::BTreeMap;

use serde_json::Value as JsonValue;

use usage_collector_sdk::{MetadataKey, UsageKind, UsageRecordStatus};

use super::{
    kind_to_sql, metadata_jsonb_to_map, metadata_map_to_jsonb, parse_kind, parse_status,
    status_to_sql,
};

// ── status round-trip ────────────────────────────────────────────────────────

#[test]
fn parse_status_round_trips_through_sql_form() {
    for status in [UsageRecordStatus::Active, UsageRecordStatus::Inactive] {
        let sql = status_to_sql(status);
        assert_eq!(parse_status(sql).unwrap(), status);
    }
}

#[test]
fn parse_status_rejects_unknown() {
    assert!(parse_status("archived").is_err());
}

// ── kind round-trip ──────────────────────────────────────────────────────────

#[test]
fn parse_kind_round_trips_through_sql_form() {
    for kind in [UsageKind::Counter, UsageKind::Gauge] {
        let sql = kind_to_sql(kind);
        assert_eq!(parse_kind(sql).unwrap(), kind);
    }
}

#[test]
fn kind_to_sql_emits_lowercase_wire_tokens() {
    assert_eq!(kind_to_sql(UsageKind::Counter), "counter");
    assert_eq!(kind_to_sql(UsageKind::Gauge), "gauge");
}

#[test]
fn parse_kind_rejects_unknown() {
    assert!(parse_kind("histogram").is_err());
}

// ── metadata jsonb <-> map round-trip ────────────────────────────────────────

#[test]
fn metadata_map_to_jsonb_then_back_round_trips() {
    let mut map = BTreeMap::new();
    map.insert(MetadataKey::new("region").unwrap(), "eu-west".to_owned());
    map.insert(MetadataKey::new("tier").unwrap(), "gold".to_owned());

    let json = metadata_map_to_jsonb(&map);
    let back = metadata_jsonb_to_map(json).unwrap();
    assert_eq!(back, map);
}

#[test]
fn empty_metadata_round_trips() {
    let map: BTreeMap<MetadataKey, String> = BTreeMap::new();
    let json = metadata_map_to_jsonb(&map);
    assert_eq!(json, JsonValue::Object(serde_json::Map::new()));
    assert!(metadata_jsonb_to_map(json).unwrap().is_empty());
}

#[test]
fn metadata_jsonb_null_maps_to_empty() {
    assert!(metadata_jsonb_to_map(JsonValue::Null).unwrap().is_empty());
}

#[test]
fn metadata_jsonb_non_object_is_rejected() {
    assert!(metadata_jsonb_to_map(JsonValue::String("x".to_owned())).is_err());
}

#[test]
fn metadata_jsonb_non_string_value_is_rejected() {
    let mut obj = serde_json::Map::new();
    obj.insert("region".to_owned(), JsonValue::Bool(true));
    assert!(metadata_jsonb_to_map(JsonValue::Object(obj)).is_err());
}
