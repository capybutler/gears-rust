use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use toolkit::client_hub::ClientHub;
use toolkit_canonical_errors::CanonicalError;
use types_registry_sdk::testing::MockTypesRegistryClient;
use types_registry_sdk::{
    GtsInstance, GtsTypeId, GtsTypeSchema, InstanceQuery, RegisterResult, TypeSchemaQuery,
    TypesRegistryClient,
};
use usage_collector_sdk::v2::USAGE_RECORD_BASE_TYPE;
use uuid::Uuid;

use super::{TypesRegistryRetentionSource, retention_from_traits};
use crate::domain::ports::{RetentionError, RetentionSource};

/// The derived segment of the meter this module tests — the base type is
/// **composed** from [`USAGE_RECORD_BASE_TYPE`], never spelled out here. Same
/// idiom as `usage-collector/src/v2/gts/usage_record_tests.rs`'s
/// `derived_id`: nothing is prepended, because `gts_id!` already emits the
/// `gts.` prefix into the constant.
const METER_SUFFIX: &str = "cf.compute._.vcpu_hours.v1~";
fn meter_raw() -> String {
    format!("{USAGE_RECORD_BASE_TYPE}{METER_SUFFIX}")
}
const DAY: u64 = 86_400;

/// A base + derived chain, so the traits are read through
/// `GtsTypeSchema::effective_traits` exactly as production reads them.
fn meter_schema(traits: &serde_json::Value) -> GtsTypeSchema {
    let base = GtsTypeSchema::try_new(
        GtsTypeId::try_new(USAGE_RECORD_BASE_TYPE).expect("base type id"),
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
        GtsTypeId::try_new(&meter_raw()).expect("meter type id"),
        json!({
            "allOf": [ { "$ref": format!("gts://{USAGE_RECORD_BASE_TYPE}") } ],
            "x-gts-traits": traits.clone()
        }),
        None,
        Some(Arc::new(base)),
    )
    .expect("meter schema")
}

/// The meter's registry reference, as `types-registry` derives it for
/// [`meter_raw`]. Read off the SDK's own schema rather than computed here:
/// the derivation is the registry's, and this plugin never performs it.
fn meter_reference() -> Uuid {
    meter_schema(&json!({})).type_uuid
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
fn hours_are_a_fixed_length_and_are_accepted() {
    assert_eq!(
        retention_from_traits(&json!({ "retention": "PT36H" })),
        Ok(Duration::from_hours(36))
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
    let schema = meter_schema(
        &json!({ "aggregation_fold": "SUM", "canonical_unit": "count", "retention": "P400D" }),
    );
    let reference = schema.type_uuid;
    let source = source_over(MockTypesRegistryClient::new().with_type_schemas([schema]));
    assert_eq!(
        source.retention(reference).await,
        Ok(Duration::from_secs(400 * DAY))
    );
}

#[tokio::test]
async fn an_unregistered_type_is_not_found() {
    let source = source_over(MockTypesRegistryClient::new());
    assert_eq!(
        source.retention(meter_reference()).await,
        Err(RetentionError::NotFound)
    );
}

#[tokio::test]
async fn the_lookup_is_keyed_on_the_reference_and_not_on_the_identifier() {
    // The registry holds the meter under its own reference; this asks for a
    // different one. It must miss. The sweep now passes `usage_type_key`'s
    // `uuid` and has no identifier to fall back on, so a source that resolved
    // by identifier instead would answer this *successfully* — and would
    // answer `NotFound` for every reference the sweep actually presents,
    // which `drop_decision` turns into a chunk kept for ever.
    let source = source_over(
        MockTypesRegistryClient::new().with_type_schemas([meter_schema(
            &json!({ "aggregation_fold": "SUM", "canonical_unit": "count", "retention": "P400D" }),
        )]),
    );
    assert_eq!(
        source
            .retention(Uuid::from_u128(0x5ee1_0000_0000_0001))
            .await,
        Err(RetentionError::NotFound)
    );
}

#[tokio::test]
async fn a_missing_registry_client_is_unavailable() {
    let source = TypesRegistryRetentionSource::new(Arc::new(ClientHub::default()));
    assert!(matches!(
        source.retention(meter_reference()).await,
        Err(RetentionError::Unavailable(_))
    ));
}

/// A `TypesRegistryClient` whose `get_type_schema_by_uuid` sleeps before
/// delegating — standing in for a hung registry, a gap once
/// found: `retention()` awaited this call with no timeout, able to stall the
/// whole background sweep/sampler loop indefinitely. Every other method
/// delegates to `inner`, unreached by the retention path.
struct SlowRegistry {
    delay: Duration,
    inner: MockTypesRegistryClient,
}

impl SlowRegistry {
    fn new(delay: Duration, inner: MockTypesRegistryClient) -> Self {
        Self { delay, inner }
    }
}

#[async_trait::async_trait]
impl TypesRegistryClient for SlowRegistry {
    async fn register(&self, entities: Vec<Value>) -> Result<Vec<RegisterResult>, CanonicalError> {
        self.inner.register(entities).await
    }

    async fn register_type_schemas(
        &self,
        type_schemas: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        self.inner.register_type_schemas(type_schemas).await
    }

    async fn get_type_schema(&self, type_id: &str) -> Result<GtsTypeSchema, CanonicalError> {
        self.inner.get_type_schema(type_id).await
    }

    async fn get_type_schema_by_uuid(
        &self,
        type_uuid: Uuid,
    ) -> Result<GtsTypeSchema, CanonicalError> {
        tokio::time::sleep(self.delay).await;
        self.inner.get_type_schema_by_uuid(type_uuid).await
    }

    async fn get_type_schemas(
        &self,
        type_ids: Vec<String>,
    ) -> HashMap<String, Result<GtsTypeSchema, CanonicalError>> {
        self.inner.get_type_schemas(type_ids).await
    }

    async fn get_type_schemas_by_uuid(
        &self,
        type_uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsTypeSchema, CanonicalError>> {
        self.inner.get_type_schemas_by_uuid(type_uuids).await
    }

    async fn list_type_schemas(
        &self,
        query: TypeSchemaQuery,
    ) -> Result<Vec<GtsTypeSchema>, CanonicalError> {
        self.inner.list_type_schemas(query).await
    }

    async fn register_instances(
        &self,
        instances: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        self.inner.register_instances(instances).await
    }

    async fn get_instance(&self, id: &str) -> Result<GtsInstance, CanonicalError> {
        self.inner.get_instance(id).await
    }

    async fn get_instance_by_uuid(&self, uuid: Uuid) -> Result<GtsInstance, CanonicalError> {
        self.inner.get_instance_by_uuid(uuid).await
    }

    async fn get_instances(
        &self,
        ids: Vec<String>,
    ) -> HashMap<String, Result<GtsInstance, CanonicalError>> {
        self.inner.get_instances(ids).await
    }

    async fn get_instances_by_uuid(
        &self,
        uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsInstance, CanonicalError>> {
        self.inner.get_instances_by_uuid(uuids).await
    }

    async fn list_instances(
        &self,
        query: InstanceQuery,
    ) -> Result<Vec<GtsInstance>, CanonicalError> {
        self.inner.list_instances(query).await
    }
}

/// A registry call that never returns
/// must not stall `retention()` past the call's own bound. Driven in paused
/// virtual time, so the 2s bound and the registry's "hang" both pass
/// instantly rather than costing real wall-clock time.
#[tokio::test(start_paused = true)]
async fn a_hung_registry_call_is_bounded_rather_than_awaited_forever() {
    let hub = Arc::new(ClientHub::default());
    hub.register::<dyn TypesRegistryClient>(Arc::new(SlowRegistry::new(
        Duration::from_mins(1),
        MockTypesRegistryClient::new().with_type_schemas([meter_schema(
            &json!({ "aggregation_fold": "SUM", "canonical_unit": "count", "retention": "P400D" }),
        )]),
    )));
    let source = TypesRegistryRetentionSource::new(hub);

    let result = tokio::time::timeout(Duration::from_secs(5), source.retention(meter_reference()))
        .await
        .expect("retention() must bound the registry call itself, not rely on an outer caller");

    assert!(
        matches!(result, Err(RetentionError::Unavailable(_))),
        "a timed-out registry call is an availability problem, like any other \
         non-definite-not-found failure: {result:?}"
    );
}
