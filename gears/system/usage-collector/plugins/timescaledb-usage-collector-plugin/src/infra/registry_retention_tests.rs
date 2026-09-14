use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use toolkit::client_hub::ClientHub;
use types_registry_sdk::testing::MockTypesRegistryClient;
use types_registry_sdk::{GtsTypeId, GtsTypeSchema, TypesRegistryClient};

use super::{TypesRegistryRetentionSource, retention_from_traits};
use crate::domain::ports::{RetentionError, RetentionSource};

const BASE: &str = "gts.cf.core.uc.usage_record.v1~";
const METER: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1~";
const DAY: u64 = 86_400;

/// A base + derived chain, so the traits are read through
/// `GtsTypeSchema::effective_traits` exactly as production reads them.
fn meter_schema(traits: &serde_json::Value) -> GtsTypeSchema {
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
            "allOf": [ { "$ref": format!("gts://{BASE}") } ],
            "x-gts-traits": traits.clone()
        }),
        None,
        Some(Arc::new(base)),
    )
    .expect("meter schema")
}

fn source_over(mock: MockTypesRegistryClient) -> TypesRegistryRetentionSource {
    let hub = Arc::new(ClientHub::default());
    hub.register::<dyn TypesRegistryClient>(Arc::new(mock));
    TypesRegistryRetentionSource::new(hub)
}

#[test]
fn a_day_count_parses_to_whole_days() {
    assert_eq!(
        retention_from_traits(&json!({ "retention": "P125D" })),
        Ok(Duration::from_secs(125 * DAY))
    );
}

#[test]
#[allow(clippy::duration_suboptimal_units)]
fn hours_are_a_fixed_length_and_are_accepted() {
    assert_eq!(
        retention_from_traits(&json!({ "retention": "PT36H" })),
        Ok(Duration::from_secs(36 * 3600))
    );
}

#[test]
fn years_and_months_are_rejected_as_not_fixed_length() {
    for calendar in ["P1Y", "P1M"] {
        assert!(
            matches!(
                retention_from_traits(&json!({ "retention": calendar })),
                Err(RetentionError::InvalidTrait(_))
            ),
            "{calendar} is not a fixed number of seconds and must not be guessed at"
        );
    }
}

#[test]
fn a_zero_retention_is_rejected() {
    assert!(matches!(
        retention_from_traits(&json!({ "retention": "PT0S" })),
        Err(RetentionError::InvalidTrait(_))
    ));
}

#[test]
fn a_non_string_retention_is_rejected() {
    assert!(matches!(
        retention_from_traits(&json!({ "retention": 125 })),
        Err(RetentionError::InvalidTrait(_))
    ));
}

#[test]
fn an_absent_retention_is_reported_as_missing() {
    assert_eq!(
        retention_from_traits(&json!({ "aggregation_fold": "SUM" })),
        Err(RetentionError::MissingTrait)
    );
}

#[tokio::test]
async fn a_registered_type_resolves_through_its_effective_traits() {
    let source = source_over(
        MockTypesRegistryClient::new().with_type_schemas([meter_schema(
            &json!({ "aggregation_fold": "SUM", "canonical_unit": "count", "retention": "P400D" }),
        )]),
    );
    assert_eq!(
        source.retention(METER).await,
        Ok(Duration::from_secs(400 * DAY))
    );
}

#[tokio::test]
async fn an_unregistered_type_is_not_found() {
    let source = source_over(MockTypesRegistryClient::new());
    assert_eq!(source.retention(METER).await, Err(RetentionError::NotFound));
}

#[tokio::test]
async fn a_malformed_type_id_is_unavailable_rather_than_not_found() {
    let source = source_over(MockTypesRegistryClient::new());
    assert!(matches!(
        source.retention("not-a-type-id").await,
        Err(RetentionError::Unavailable(_))
    ));
}

#[tokio::test]
async fn a_missing_registry_client_is_unavailable() {
    let source = TypesRegistryRetentionSource::new(Arc::new(ClientHub::default()));
    assert!(matches!(
        source.retention(METER).await,
        Err(RetentionError::Unavailable(_))
    ));
}
