//! Unit tests for [`TypesRegistryDeclarationSource`].
//!
//! Coverage bar (see the task): a registered schema fetches; an unregistered
//! type is a **definite** not-found; a missing `TypesRegistryClient` on the
//! hub is *not* a not-found (it's an availability problem — the absence of a
//! client says nothing about whether the type exists).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{Value, json};
use toolkit::client_hub::ClientHub;
use toolkit_canonical_errors::CanonicalError;
use types_registry_sdk::testing::MockTypesRegistryClient;
use types_registry_sdk::{
    GtsInstance, GtsTypeId, GtsTypeSchema, InstanceQuery, RegisterResult, TypeSchemaQuery,
    TypesRegistryClient,
};
use usage_collector_sdk::MeterTypeId;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::declaration_mirror::{
    DeclarationMirror, MirrorError, NoopDeclarationMirror,
};
use crate::domain::ports::declarations::{DeclarationRegistrar, DeclarationSource};
use crate::domain::ports::metrics::NoopMetrics;

use super::{TypesRegistryDeclarationSource, build_default_resolvers};

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
async fn build_default_resolvers_wires_a_working_resolver_over_the_hub() {
    // Bootstrap-layer smoke test: `module.rs` calls this to build the
    // production Type Resolver. Proves the wiring — adapter over `hub`,
    // wrapped in the given cache policy — actually resolves, not just that
    // it constructs without panicking.
    let hub = hub_with(MockTypesRegistryClient::new().with_type_schemas([registered_schema()]));

    let (resolver, _reverse) = build_default_resolvers(
        hub,
        Arc::new(NoopDeclarationMirror),
        300,
        10_000,
        Arc::new(NoopMetrics),
    );

    let declaration = resolver.resolve(&meter_id()).await.expect("resolves");
    assert_eq!(declaration.gts_type_id.as_str(), METER);
}

/// A counting [`DeclarationMirror`] double, for pinning that
/// `build_default_resolvers`'s returned resolver is actually constructed
/// through `TypeResolver::with_rehydration`, not `TypeResolver::new`'s
/// no-op default.
///
/// **This is the gap fix round 1's review found.** The test immediately
/// above (`build_default_resolvers_wires_a_working_resolver_over_the_hub`)
/// passes `Arc::new(NoopDeclarationMirror)` and never asserts a write count,
/// so it cannot tell `with_rehydration` apart from `new` — both resolve the
/// meter successfully, because `resolve`'s own success never depends on the
/// mirror write succeeding (ADR statement 4: a failed write does not reject
/// the entry). A mutation back to `TypeResolver::new(source, config,
/// metrics)` — dropping `mirror` on the floor entirely — compiles, keeps
/// every existing lane green (unit, clippy, even e2e's 13), and silently
/// deletes DESIGN §3.7's whole restore path. A write/read count is the only
/// signal from outside that tells the two constructors apart.
#[derive(Default)]
struct CountingMirror {
    rows: StdMutex<HashMap<String, Value>>,
    writes: AtomicUsize,
    reads: AtomicUsize,
}

impl CountingMirror {
    fn arc() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Seeds a row directly, standing in for "this meter was resolved
    /// before the registry lost it" — for the restore-leg test below.
    fn seeded(id: &MeterTypeId, document: Value) -> Arc<Self> {
        let mirror = Self::default();
        mirror
            .rows
            .lock()
            .expect("lock")
            .insert(id.as_str().to_owned(), document);
        Arc::new(mirror)
    }

    fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl DeclarationMirror for CountingMirror {
    async fn upsert(
        &self,
        id: &MeterTypeId,
        _type_uuid: Uuid,
        document: &Value,
    ) -> Result<(), MirrorError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.rows
            .lock()
            .expect("lock")
            .insert(id.as_str().to_owned(), document.clone());
        Ok(())
    }

    async fn read(&self, id: &MeterTypeId) -> Result<Option<Value>, MirrorError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.rows.lock().expect("lock").get(id.as_str()).cloned())
    }

    async fn resolve_id(&self, _type_uuid: Uuid) -> Result<Option<MeterTypeId>, MirrorError> {
        Ok(None)
    }
}

#[tokio::test]
async fn build_default_resolvers_wires_the_real_mirror_not_the_noop_default() {
    // The mirror leg: a successful resolution must write the declaration
    // back through the INJECTED mirror, not silently through a no-op one.
    let hub = hub_with(MockTypesRegistryClient::new().with_type_schemas([registered_schema()]));
    let mirror = CountingMirror::arc();

    let (resolver, _reverse) = build_default_resolvers(
        hub,
        Arc::clone(&mirror) as Arc<dyn DeclarationMirror>,
        300,
        10_000,
        Arc::new(NoopMetrics),
    );

    resolver.resolve(&meter_id()).await.expect("resolves");

    assert_eq!(
        mirror.writes(),
        1,
        "a successful resolution must mirror the declaration exactly once \
         through the mirror build_default_resolvers was given; `TypeResolver::new`'s \
         no-op default would leave this at 0 while still resolving successfully"
    );
}

/// A fake `TypesRegistryClient` whose `get_type_schema` answers a definite
/// not-found on the FIRST call and `schema` on every call after — standing
/// in for "the registry just lost the declaration", so a restore (which
/// registers the mirrored document back, then re-fetches per spec ruling
/// J12) finds it again on its second try. Used only by the one test below
/// that exercises `build_default_resolvers`'s restore leg end to end; every
/// other `TypesRegistryClient` method is unreached by that path and panics
/// if it ever is.
struct FlakyThenFoundRegistry {
    schema: GtsTypeSchema,
    get_calls: AtomicUsize,
    register_calls: AtomicUsize,
}

impl FlakyThenFoundRegistry {
    fn new(schema: GtsTypeSchema) -> Self {
        Self {
            schema,
            get_calls: AtomicUsize::new(0),
            register_calls: AtomicUsize::new(0),
        }
    }

    fn register_calls(&self) -> usize {
        self.register_calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl TypesRegistryClient for FlakyThenFoundRegistry {
    async fn register(&self, _entities: Vec<Value>) -> Result<Vec<RegisterResult>, CanonicalError> {
        unimplemented!("not reached by the restore path under test")
    }

    async fn register_type_schemas(
        &self,
        type_schemas: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        self.register_calls.fetch_add(1, Ordering::SeqCst);
        Ok(type_schemas
            .iter()
            .map(|_| RegisterResult::Ok {
                gts_id: METER.to_owned(),
            })
            .collect())
    }

    async fn get_type_schema(&self, type_id: &str) -> Result<GtsTypeSchema, CanonicalError> {
        if self.get_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(types_registry_sdk::testing::not_found(type_id))
        } else {
            Ok(self.schema.clone())
        }
    }

    async fn get_type_schema_by_uuid(&self, _: Uuid) -> Result<GtsTypeSchema, CanonicalError> {
        unimplemented!("not reached by the restore path under test")
    }

    async fn get_type_schemas(
        &self,
        _: Vec<String>,
    ) -> HashMap<String, Result<GtsTypeSchema, CanonicalError>> {
        unimplemented!("not reached by the restore path under test")
    }

    async fn get_type_schemas_by_uuid(
        &self,
        _: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsTypeSchema, CanonicalError>> {
        unimplemented!("not reached by the restore path under test")
    }

    async fn list_type_schemas(
        &self,
        _: TypeSchemaQuery,
    ) -> Result<Vec<GtsTypeSchema>, CanonicalError> {
        unimplemented!("not reached by the restore path under test")
    }

    async fn register_instances(
        &self,
        _: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        unimplemented!("not reached by the restore path under test")
    }

    async fn get_instance(&self, _: &str) -> Result<GtsInstance, CanonicalError> {
        unimplemented!("not reached by the restore path under test")
    }

    async fn get_instance_by_uuid(&self, _: Uuid) -> Result<GtsInstance, CanonicalError> {
        unimplemented!("not reached by the restore path under test")
    }

    async fn get_instances(
        &self,
        _: Vec<String>,
    ) -> HashMap<String, Result<GtsInstance, CanonicalError>> {
        unimplemented!("not reached by the restore path under test")
    }

    async fn get_instances_by_uuid(
        &self,
        _: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsInstance, CanonicalError>> {
        unimplemented!("not reached by the restore path under test")
    }

    async fn list_instances(&self, _: InstanceQuery) -> Result<Vec<GtsInstance>, CanonicalError> {
        unimplemented!("not reached by the restore path under test")
    }
}

#[tokio::test]
async fn build_default_resolvers_wires_the_real_registrar_and_restores_a_lost_declaration() {
    // The registrar leg: a definite not-found from the registry, with the
    // declaration seeded in the mirror, must restore through the INJECTED
    // registrar (register, then re-fetch) rather than failing closed with
    // nothing to fall back on.
    let registry = Arc::new(FlakyThenFoundRegistry::new(registered_schema()));
    let hub = Arc::new(ClientHub::new());
    hub.register::<dyn TypesRegistryClient>(registry.clone());

    let seed_document = json!({ "$id": format!("gts://{METER}") });
    let mirror = CountingMirror::seeded(&meter_id(), seed_document);

    let (resolver, _reverse) = build_default_resolvers(
        hub,
        mirror as Arc<dyn DeclarationMirror>,
        300,
        10_000,
        Arc::new(NoopMetrics),
    );

    let declaration = resolver
        .resolve(&meter_id())
        .await
        .expect("a seeded mirror row must restore the declaration, not fail closed");
    assert_eq!(declaration.gts_type_id.as_str(), METER);
    assert_eq!(
        registry.register_calls(),
        1,
        "the restore must register the mirrored document back to \
         types-registry through the INJECTED registrar exactly once; \
         `TypeResolver::new`'s `UnavailableDeclarationRegistrar` default \
         would fail this resolution closed instead"
    );
}

/// A fake `TypesRegistryClient` that wraps a `MockTypesRegistryClient`,
/// answering a scripted `register_type_schemas` result, counting calls, and
/// recording every batch it was actually called with.
///
/// Hand-rolled rather than reusing `MockTypesRegistryClient`: that mock's
/// `register_type_schemas` opens with
/// `assert!(type_schemas.is_empty(), "…is not implemented; …")`, so it
/// **panics** on any real registration. Extending it would be an edit to
/// another gear's SDK — the shared-platform shape ruling E1 settled when the
/// owner declined one to `toolkit-odata` — so the fake lives here, over this
/// gear's own port.
struct ScriptedRegistry {
    result: std::sync::Mutex<Option<Result<Vec<RegisterResult>, CanonicalError>>>,
    calls: std::sync::atomic::AtomicUsize,
    // What `register_type_schemas` was actually called with, in call order.
    // Load-bearing, not decoration: without this, nothing in this file pins
    // what `register()` sends — a mutation that swaps in an unrelated
    // document, or an empty batch, or skips the call entirely, left every
    // test green before this field existed. An empty batch is the serious
    // case: the real `register_type_schemas` answers `Ok(vec![])` for an
    // empty input, so the per-item loop below would find no
    // `RegisterResult::Err` among zero items and report success having
    // registered nothing.
    received: std::sync::Mutex<Vec<Vec<Value>>>,
    // Every method but `register_type_schemas` delegates here so this fake
    // stays small. Not in the task brief's sketch of this struct — an empty
    // `MockTypesRegistryClient::new()` has nothing to delegate *to* without
    // it, so this field is load-bearing, not decoration.
    inner: MockTypesRegistryClient,
}

impl ScriptedRegistry {
    fn arc(result: Result<Vec<RegisterResult>, CanonicalError>) -> Arc<Self> {
        Arc::new(Self {
            result: std::sync::Mutex::new(Some(result)),
            calls: std::sync::atomic::AtomicUsize::new(0),
            received: std::sync::Mutex::new(Vec::new()),
            inner: MockTypesRegistryClient::new(),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Every batch `register_type_schemas` was actually called with, in call
    /// order.
    fn received(&self) -> Vec<Vec<Value>> {
        self.received
            .lock()
            .expect("ScriptedRegistry: received-batches lock poisoned")
            .clone()
    }
}

// Only `register_type_schemas` is scripted; every other method delegates to
// `self.inner` (a `MockTypesRegistryClient`) so this fake stays small.
//
// `TypesRegistryClient` has exactly 13 methods — counted from
// `types-registry-sdk/src/api.rs`. These 12 delegate; four of them
// (`get_type_schemas`, `get_type_schemas_by_uuid`, `get_instances`,
// `get_instances_by_uuid`) return a bare `HashMap`, not a `Result`.
#[async_trait::async_trait]
impl TypesRegistryClient for ScriptedRegistry {
    async fn register(&self, entities: Vec<Value>) -> Result<Vec<RegisterResult>, CanonicalError> {
        self.inner.register(entities).await
    }

    async fn register_type_schemas(
        &self,
        type_schemas: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.received
            .lock()
            .expect("ScriptedRegistry: received-batches lock poisoned")
            .push(type_schemas);
        self.result
            .lock()
            .expect("ScriptedRegistry: result lock poisoned")
            .take()
            .expect("ScriptedRegistry::register_type_schemas scripted for exactly one call")
    }

    async fn get_type_schema(&self, type_id: &str) -> Result<GtsTypeSchema, CanonicalError> {
        self.inner.get_type_schema(type_id).await
    }

    async fn get_type_schema_by_uuid(
        &self,
        type_uuid: Uuid,
    ) -> Result<GtsTypeSchema, CanonicalError> {
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

#[tokio::test]
async fn a_successful_registration_is_ok_and_calls_the_registry_once() {
    let registry = ScriptedRegistry::arc(Ok(vec![RegisterResult::Ok {
        gts_id: METER.to_owned(),
    }]));
    let hub = Arc::new(ClientHub::new());
    hub.register::<dyn TypesRegistryClient>(registry.clone());
    let document = json!({ "$id": "gts://x" });

    TypesRegistryDeclarationSource::new(hub)
        .register(&document)
        .await
        .expect("a RegisterResult::Ok must be Ok");

    assert_eq!(
        registry.calls(),
        1,
        "the restore must make exactly one registration call"
    );
    assert_eq!(
        registry.received(),
        vec![vec![document]],
        "register() must send exactly the caller's own document, in a \
         singleton batch - not an unrelated document, and not an empty \
         batch (the real `register_type_schemas` answers `Ok(vec![])` for \
         an empty input, which would let a restore report success having \
         registered nothing)"
    );
}

/// Review Focus item 1: a per-item refusal arrives INSIDE an `Ok`.
///
/// `register_type_schemas` is documented *"Returns `Err` only for
/// catastrophic failures"*; its sibling `register`'s doc (`api.rs`, the
/// generic batch-register method, not this one) is what says *"Per-item
/// failures are reported via `RegisterResult::Err`"* — the behaviour is
/// shared across both methods, but that exact sentence lives on `register`,
/// not here.
///
/// Asserts the refused item's own `gts_id` and the category `Display`
/// prefixes onto every message, not the field violation's own description
/// ("parent not registered"). Ruling J24b: `CanonicalError`'s `Display`
/// deliberately withholds a field-violation's description for
/// `InvalidArgument` — `#[resource_error]`'s generated `invalid_argument()`
/// constructor seeds `detail` with the fixed string `"Request validation
/// failed"` (`toolkit-canonical-errors-macro/src/lib.rs`), and `create()`'s
/// detail-override step never replaces it with the per-violation
/// description. So the description never reaches `Display`; the `gts_id`
/// (from this code's own format string) and the category name do.
#[tokio::test]
async fn a_per_item_registration_refusal_inside_ok_is_an_error() {
    let registry = ScriptedRegistry::arc(Ok(vec![RegisterResult::Err {
        gts_id: Some(METER.to_owned()),
        error: types_registry_sdk::testing::invalid_gts_id("parent not registered"),
    }]));
    let hub = Arc::new(ClientHub::new());
    hub.register::<dyn TypesRegistryClient>(registry.clone());

    let err = TypesRegistryDeclarationSource::new(hub)
        .register(&json!({ "$id": "gts://x" }))
        .await
        .expect_err("a RegisterResult::Err must not be read as success");

    let message = err.to_string();
    assert!(
        message.contains(METER),
        "the refused item's own gts_id must reach the message: {err}"
    );
    assert!(
        message.contains("invalid_argument"),
        "the per-item CanonicalError's category must reach the message: {err}"
    );
}

/// Review Focus item 8: a `RegisterResult::Err` whose `gts_id` could not be
/// extracted from the document at all — a real registry path
/// (`types-registry`'s `local_client.rs` emits exactly this for a document
/// whose id cannot be read), not a hypothetical.
#[tokio::test]
async fn a_per_item_refusal_with_no_extractable_gts_id_uses_the_placeholder() {
    let registry = ScriptedRegistry::arc(Ok(vec![RegisterResult::Err {
        gts_id: None,
        error: types_registry_sdk::testing::invalid_gts_id("no gts id field found"),
    }]));
    let hub = Arc::new(ClientHub::new());
    hub.register::<dyn TypesRegistryClient>(registry.clone());

    let err = TypesRegistryDeclarationSource::new(hub)
        .register(&json!({ "not-a-gts-id-field": true }))
        .await
        .expect_err("a RegisterResult::Err must not be read as success");

    assert!(
        err.to_string().contains("<no id in the document>"),
        "a refusal with no extractable gts_id must render the placeholder \
         identity, not an empty or missing one: {err}"
    );
}

#[tokio::test]
async fn a_catastrophic_registration_failure_is_an_error() {
    let registry =
        ScriptedRegistry::arc(Err(types_registry_sdk::testing::internal("backend down")));
    let hub = Arc::new(ClientHub::new());
    hub.register::<dyn TypesRegistryClient>(registry.clone());

    let err = TypesRegistryDeclarationSource::new(hub)
        .register(&json!({ "$id": "gts://x" }))
        .await
        .expect_err("an outer Err must be an error");
    // The category reaches the message; the `Internal` description
    // ("backend down") deliberately does not — see
    // [`an_internal_registry_failure_s_diagnostic_never_reaches_the_wire_problem_detail`].
    assert!(
        err.to_string().contains("internal"),
        "the registry failure's own category must reach the message: {err}"
    );
}

#[tokio::test]
async fn a_hub_without_a_registry_client_fails_as_unavailable() {
    let err = TypesRegistryDeclarationSource::new(Arc::new(ClientHub::new()))
        .register(&json!({ "$id": "gts://x" }))
        .await
        .expect_err("no client on the hub is a failure");
    assert!(
        matches!(err, DomainError::TypesRegistryUnavailable(_)),
        "a hub miss is an availability fact, not a statement about the type: {err:?}"
    );
}

/// Own mutation beyond the brief's: a catastrophic outer `Err` must still be
/// classified `TypesRegistryUnavailable` (not just "any `Err`"), and the
/// message must carry the registry's own failure text rather than a fixed
/// placeholder string the implementation could satisfy without looking at
/// `e` at all.
///
/// **The fixture is a `ServiceUnavailable`, not an `Internal`.** It used to
/// be `testing::internal("warehouse offline")`, which worked only because
/// the code under test read `diagnostic()` — the accessor that returns
/// `Internal`'s `#[serde(skip)]` description. Now that it reads `Display`,
/// `Internal` renders a fixed placeholder by construction, so an `Internal`
/// fixture could no longer tell "looked at `e`" from "ignored `e`". A
/// category whose `Display` carries real text is what keeps this
/// anti-placeholder property alive after that change; the disclosure half
/// is pinned separately below.
#[tokio::test]
async fn a_catastrophic_failure_is_specifically_unavailable_with_the_backend_detail() {
    let registry = ScriptedRegistry::arc(Err(CanonicalError::service_unavailable()
        .with_detail("warehouse offline")
        .create()));
    let hub = Arc::new(ClientHub::new());
    hub.register::<dyn TypesRegistryClient>(registry.clone());

    let err = TypesRegistryDeclarationSource::new(hub)
        .register(&json!({ "$id": "gts://x" }))
        .await
        .expect_err("an outer Err must be an error");

    assert!(
        matches!(err, DomainError::TypesRegistryUnavailable(_)),
        "a catastrophic registry failure must be TypesRegistryUnavailable: {err:?}"
    );
    assert!(
        err.to_string().contains("warehouse offline"),
        "the catastrophic failure's own detail must reach the message, not a \
         fixed placeholder: {err}"
    );
}

#[tokio::test]
async fn fetch_by_uuid_resolves_a_registered_schema() {
    let hub = hub_with(MockTypesRegistryClient::new().with_type_schemas([registered_schema()]));
    let source = TypesRegistryDeclarationSource::new(hub);
    let reference = registered_schema().type_uuid;

    let schema = source
        .fetch_by_uuid(reference)
        .await
        .expect("a registered reference resolves");

    assert_eq!(schema.type_id.as_ref(), METER);
}

#[tokio::test]
async fn fetch_by_uuid_reports_an_unknown_reference_as_a_definite_not_found() {
    let hub = hub_with(MockTypesRegistryClient::new().with_type_schemas([registered_schema()]));
    let source = TypesRegistryDeclarationSource::new(hub);

    let err = source
        .fetch_by_uuid(Uuid::from_u128(0xfeed))
        .await
        .expect_err("an unregistered reference does not resolve");

    assert!(
        err.is_declaration_not_found(),
        "an unknown reference is a definite answer, not an availability problem: {err:?}"
    );
}

#[tokio::test]
async fn fetch_by_uuid_reports_a_missing_client_as_unavailable() {
    let source = TypesRegistryDeclarationSource::new(Arc::new(ClientHub::default()));

    let err = source
        .fetch_by_uuid(Uuid::from_u128(1))
        .await
        .expect_err("no client means no answer");

    assert!(
        matches!(err, DomainError::TypesRegistryUnavailable(_)),
        "a hub miss says nothing about whether the type exists: {err:?}"
    );
}

/// A `CanonicalError::Internal`'s description must not reach the 503's wire
/// body.
///
/// `diagnostic()` returns `Some(&ctx.description)` for `Internal` and
/// `Unknown`, and that field is `#[serde(skip)]` in
/// `toolkit-canonical-errors`' `InternalV1`/`UnknownV1` — the platform
/// stating, with compiler enforcement, that the text is server-side only.
/// `register()` used to copy it into a plain `String`, which defeats the
/// attribute: a `String` in `DomainError::TypesRegistryUnavailable` is no
/// longer protected by anything.
///
/// This walks the whole chain the gear actually ships rather than asserting
/// at one hop, because the hop that made the copy dangerous is in a
/// different file: `From<DomainError> for UsageCollectorError`'s
/// `TypesRegistryUnavailable` arm carries the detail through
/// instead of replacing it with a fixed string,
/// and `infra::sdk_error_mapping` then hands it to
/// `CanonicalError::service_unavailable().with_detail(...)`, whose `detail`
/// **is** serialized as `Problem.detail`.
///
/// Not vacuous: the fixture is asserted to really carry the secret in the
/// place `diagnostic()` reads, so a run in which the planting silently
/// failed reds here rather than passing the containment checks below for
/// the wrong reason.
#[tokio::test]
async fn an_internal_registry_failure_s_diagnostic_never_reaches_the_wire_problem_detail() {
    const SECRET: &str = "connection to pg-primary.internal:5432 refused; password=hunter2";

    let planted = types_registry_sdk::testing::internal(SECRET);
    assert_eq!(
        planted.diagnostic(),
        Some(SECRET),
        "fixture guard: the secret must really sit where `diagnostic()` reads it, or the \
         absence assertions below prove nothing"
    );

    let registry = ScriptedRegistry::arc(Err(planted));
    let hub = Arc::new(ClientHub::new());
    hub.register::<dyn TypesRegistryClient>(registry.clone());

    let domain_err = TypesRegistryDeclarationSource::new(hub)
        .register(&json!({ "$id": "gts://x" }))
        .await
        .expect_err("an outer Err must be an error");

    assert!(
        !domain_err.to_string().contains(SECRET),
        "the Internal description must not be copied into the DomainError at all: {domain_err}"
    );

    let lifted = usage_collector_sdk::UsageCollectorError::from(domain_err);
    let problem = crate::infra::sdk_error_mapping::usage_record_error_to_problem(lifted);

    assert_eq!(
        problem.status,
        Some(503),
        "a types-registry availability failure lifts to a 503"
    );
    assert!(
        !problem.detail.contains(SECRET),
        "Problem.detail is serialized onto the wire; an Internal error's #[serde(skip)] \
         description must never appear there: {:?}",
        problem.detail
    );
    let serialized = serde_json::to_string(&problem).expect("Problem serializes");
    assert!(
        !serialized.contains(SECRET),
        "no part of the serialized Problem may carry the Internal description: {serialized}"
    );
}
