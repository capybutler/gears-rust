//! Unit tests for [`TypesRegistryDeclarationSource`].
//!
//! Coverage bar (see the task): a registered schema fetches; an unregistered
//! type is a **definite** not-found; a missing `TypesRegistryClient` on the
//! hub is *not* a not-found (it's an availability problem — the absence of a
//! client says nothing about whether the type exists).

use std::sync::Arc;

use serde_json::json;
use toolkit::client_hub::ClientHub;
use types_registry_sdk::testing::MockTypesRegistryClient;
use types_registry_sdk::{GtsTypeId, GtsTypeSchema, TypesRegistryClient};
use usage_collector_sdk::MeterTypeId;

use crate::domain::ports::declarations::DeclarationSource;

use super::{TypesRegistryDeclarationSource, build_default_resolver};

const METER: &str = "gts.cf.core.uc.usage_record.v1~example.metering._.stored_volume.v1~";
const BASE: &str = "gts.cf.core.uc.usage_record.v1~";

fn meter_id() -> MeterTypeId {
    MeterTypeId::new(METER).expect("valid meter id")
}

/// Builds a registered base + derived type-schema pair.
///
/// Two departures from the task sketch, both already documented as known
/// pitfalls elsewhere in this crate (`type_resolver::declaration_tests`,
/// `type_resolver::resolver_tests`):
///
/// - `GtsTypeId` has no `FromStr`/`.parse()` in the `gts` crate; ids are
///   built with `GtsTypeId::try_new`.
/// - `x-gts-traits` is placed at the **top level** of the derived schema's
///   raw JSON, not nested inside an `allOf` branch —
///   `GtsTypeSchema::extract_traits` only ever reads
///   `schema.get("x-gts-traits")` at the top level, so a trait block nested
///   inside `allOf` is invisible to it.
fn registered_schema() -> GtsTypeSchema {
    let base = GtsTypeSchema::try_new(
        GtsTypeId::try_new(BASE).expect("base type id"),
        json!({ "type": "object" }),
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
            "x-gts-traits": {
                "aggregation_fold": "SUM",
                "canonical_unit": "bytes",
                "retention": "P125D"
            }
        }),
        None,
        Some(Arc::new(base)),
    )
    .expect("derived schema")
}

fn hub_with(client: MockTypesRegistryClient) -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::default());
    hub.register::<dyn TypesRegistryClient>(Arc::new(client));
    hub
}

#[tokio::test]
async fn fetches_a_registered_schema() {
    let hub = hub_with(MockTypesRegistryClient::new().with_type_schemas([registered_schema()]));
    let source = TypesRegistryDeclarationSource::new(hub);

    let schema = source.fetch(&meter_id()).await.expect("fetches");
    assert_eq!(schema.type_id.as_ref(), METER);
}

#[tokio::test]
async fn an_unregistered_type_is_a_definite_not_found() {
    let hub = hub_with(MockTypesRegistryClient::new());
    let source = TypesRegistryDeclarationSource::new(hub);

    let err = source.fetch(&meter_id()).await.expect_err("not registered");
    assert!(
        err.is_declaration_not_found(),
        "an unregistered type must be a definite answer so the resolver does \
         not serve a stale entry for it, got: {err:?}"
    );
}

#[tokio::test]
async fn a_missing_registry_client_is_not_a_not_found() {
    // No TypesRegistryClient on the hub is an availability problem, not a
    // statement that the type does not exist.
    let source = TypesRegistryDeclarationSource::new(Arc::new(ClientHub::default()));

    let err = source.fetch(&meter_id()).await.expect_err("no client");
    assert!(
        !err.is_declaration_not_found(),
        "a missing client must not be classified as a definite not-found, got: {err:?}"
    );
    assert!(
        matches!(
            err,
            crate::domain::error::DomainError::TypesRegistryUnavailable(_)
        ),
        "expected TypesRegistryUnavailable, got: {err:?}"
    );
}

#[tokio::test]
async fn build_default_resolver_wires_a_working_resolver_over_the_hub() {
    // Bootstrap-layer smoke test: `module.rs` calls this to build the
    // production Type Resolver. Proves the wiring — adapter over `hub`,
    // wrapped in the given cache policy — actually resolves, not just that
    // it constructs without panicking.
    let hub = hub_with(MockTypesRegistryClient::new().with_type_schemas([registered_schema()]));

    let resolver = build_default_resolver(hub, 300, 10_000);

    let declaration = resolver.resolve(&meter_id()).await.expect("resolves");
    assert_eq!(declaration.gts_type_id.as_str(), METER);
}
