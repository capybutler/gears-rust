//! Bootstrap tests for generation 2, and the coexistence tests that pin what
//! neither generation's own suite reaches.
//!
//! Generation 1's `init` / `register_rest` guards are covered by
//! `crate::module::module_tests`; the three tests whose names would have
//! duplicated them are deliberately absent, because both files would
//! otherwise drive the same gear's `init`. What is left here is what only
//! generation 2 introduces — the `db_required()` fail-closed branch, the
//! widened capability set and the mirror's migration roster — plus the three
//! coexistence tests.
//!
//! The whole file is attached by `#[path]` from the `#[cfg(feature = "v2")]`
//! gated [`crate::v2::module`], so it is gated as a unit and needs no
//! per-test attribute.

use std::sync::Arc;

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverApi;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use toolkit::config::ConfigProvider;
use toolkit::contracts::DatabaseCapability;
use toolkit::registry::{Registrator, RegistryBuilder};
use toolkit::{ClientHub, Gear, GearCtx, RestApiCapability};
use toolkit_db::{ConnectOpts, DBProvider, connect_db};
use uuid::Uuid;

use crate::module::UsageCollectorModule;
use crate::v2::domain::test_support::CountingAllowAllResolver;

struct StaticConfigProvider {
    root: serde_json::Value,
}

impl ConfigProvider for StaticConfigProvider {
    fn get_gear_config(&self, gear: &str) -> Option<&serde_json::Value> {
        self.root.get(gear)
    }
}

fn make_ctx(hub: Arc<ClientHub>) -> GearCtx {
    let cfg = json!({
        "usage-collector": { "vendor": "test-vendor" }
    });
    GearCtx::new(
        UsageCollectorModule::MODULE_NAME,
        Uuid::new_v4(),
        Arc::new(StaticConfigProvider { root: cfg }),
        hub,
        CancellationToken::new(),
    )
}

/// `make_ctx`, plus an in-memory `SQLite` database attached — `init_v2` calls
/// `ctx.db_required()` for the declaration mirror, so any test that drives a
/// full, successful `init` needs a database on the context or it fails closed
/// before ever reaching the assertions it actually wants to make. No
/// migration is applied: `init_v2` only stores the `DBProvider` inside a
/// `DbDeclarationMirror`, it never queries it, so the mirror table does not
/// need to exist for `init` itself to succeed.
async fn make_ctx_with_db(hub: Arc<ClientHub>) -> GearCtx {
    let db = connect_db(
        "sqlite::memory:",
        ConnectOpts {
            max_conns: Some(1),
            min_conns: Some(1),
            ..Default::default()
        },
    )
    .await
    .expect("connect in-memory SQLite");
    make_ctx(hub).with_db(DBProvider::new(db))
}

/// `ctx.db_required()` is required, not `ctx.db()` — a configured database
/// is a deployment obligation now, not an optional one. No other test would
/// catch a silent drop of that call: every test that drives a successful
/// `init` carries a database precisely so it can succeed, so a mutation
/// removing `ctx.db_required()?` from `init_v2` reds nothing there (the call
/// becomes dead code, not a missing check) — verified below.
#[tokio::test]
async fn init_fails_when_database_is_not_configured() {
    let hub = Arc::new(ClientHub::new());
    let resolver: Arc<dyn AuthZResolverApi> = CountingAllowAllResolver::new();
    hub.register::<dyn AuthZResolverApi>(resolver);

    // Deliberately `make_ctx`, NOT `make_ctx_with_db`: authz is present, but
    // no database is attached.
    let ctx = make_ctx(hub);
    let module = UsageCollectorModule::default();

    let err = module
        .init(&ctx)
        .await
        .expect_err("init must fail closed when no database is configured for the gear");

    let msg = format!("{err}");
    assert!(
        msg.contains("usage-collector") && msg.contains("Database is not configured"),
        "error should name the gear and the missing database, got: {msg}"
    );
}

/// Dummy core for the two gears `UsageCollectorModule` declares as
/// dependencies — this test only needs them to exist as registry entries so
/// `RegistryBuilder::build_topo_sorted` can resolve `usage-collector`'s
/// `deps` edges; their own capabilities are not under test here.
#[derive(Default)]
struct DummyDepCore;

#[async_trait]
impl Gear for DummyDepCore {
    async fn init(&self, _ctx: &GearCtx) -> anyhow::Result<()> {
        Ok(())
    }
}

/// The mirror is generation 2's first durable table
/// (`cpt-cf-usage-collector-adr-declaration-rehydration`), so the gear now
/// needs the `db` capability its generation-1 module doc used to say it did
/// not have — and it keeps `stateful`, because generation 1's
/// `uc_usage_types` gauge-refresh loop is still the gear's lifecycle entry.
///
/// Pinned indirectly, NOT through a `UsageCollectorModule::CAPABILITIES`
/// constant: reading `#[toolkit::gear(...)]`'s expansion
/// (`libs/toolkit-macros/src/lib.rs`) shows it generates `MODULE_NAME` and a
/// hidden `inventory::submit!` registrator whose body calls
/// `RegistryBuilder::register_db_with_meta` / `register_rest_with_meta` /
/// `register_stateful_with_meta` for each declared capability — it emits no
/// public capability-list constant on the struct itself, so
/// `UsageCollectorModule::CAPABILITIES` does not exist and would not
/// compile. `GearCtx` (`libs/toolkit/src/context.rs`) exposes no capability
/// query either — only config/db/client-hub accessors.
///
/// Also NOT `GearRegistry::discover_and_build()` (the mechanism `cluster`'s
/// and `gear-orchestrator`'s own test suites use for this exact purpose,
/// e.g. `gears/system/cluster/cluster/tests/consumer_wiring.rs`): it fails
/// here with `UnknownDependency { depends_on: "types-registry" }`. The
/// macro's `dep_reexports` — the `pub use ::#crate_ident as
/// _gear_dep_#crate_ident;` that force-links a declared dependency's own
/// `inventory::submit!` registrator when this gear is "pulled in
/// transitively" — is generated `#[cfg(not(test))]`
/// (`libs/toolkit-macros/src/lib.rs`), deliberately, so that a gear's own
/// unit tests do not drag in every transitive dependency's registration.
/// That means in THIS crate's `--lib` test binary, `types-registry`'s and
/// `authz-resolver`'s own registrators are never force-kept, and nothing
/// else in this test binary references those crates either, so
/// `discover_and_build`'s inventory scan never finds them — not a defect in
/// this gear's wiring, just the wrong tool for a `--lib` unit test.
///
/// So this test runs `discover_and_build`'s own loop by hand — every
/// `toolkit::registry::Registrator` the link-time inventory carries, which
/// in this binary is exactly the one `#[toolkit::gear(...)]` submitted for
/// `UsageCollectorModule` — against a builder seeded with dummy cores for
/// the two declared `deps`, so the builder's dependency-edge validation has
/// something to resolve against. The generated registrator function itself
/// is private to `crate::module`, which this module is not a descendant of,
/// so the public `Registrator` inventory is the only way in from here.
#[test]
fn the_gear_declares_the_db_capability() {
    let mut b = RegistryBuilder::default();
    b.register_core_with_meta("types-registry", &[], Arc::new(DummyDepCore));
    b.register_core_with_meta("authz-resolver", &[], Arc::new(DummyDepCore));

    for registrator in inventory::iter::<Registrator> {
        (registrator.0)(&mut b);
    }

    let registry = b.build_topo_sorted().expect("registry builds");
    let entry = registry
        .gears()
        .iter()
        .find(|e| e.name() == UsageCollectorModule::MODULE_NAME)
        .expect("usage-collector gear must be discovered in the registry");

    // Pins the exact capability SET, not just `has_db()` — the sibling
    // migration test
    // (`the_gear_offers_exactly_the_declaration_mirror_migrations`) pins its
    // own roster exactly rather than by count/presence, so this is the same
    // discipline applied one step further. `#[toolkit::gear]` registers
    // capabilities in the ORDER the attribute lists them (`capabilities =
    // [rest, db, stateful]`), so this is also an implicit pin on that
    // declared order, not just on set membership.
    assert_eq!(
        entry.caps().labels(),
        vec!["rest", "db", "stateful"],
        "usage-collector must declare exactly rest + db + stateful -- no more, no fewer"
    );
}

#[test]
fn the_gear_offers_exactly_the_declaration_mirror_migrations() {
    let migrations = UsageCollectorModule::default().migrations();
    let names: Vec<&str> = migrations.iter().map(|m| m.name()).collect();
    assert_eq!(
        names,
        vec![
            "m20261004_000001_declaration_mirror",
            "m20261005_000002_declaration_mirror_type_uuid",
        ],
        "the gear owns ONE durable table (DESIGN \u{a7}3.8: 'The one durable table \
         the gear owns is the declaration mirror of \u{a7}3.7'). Both migrations \
         here are that one table: the second drops and recreates it to add the \
         registry reference, because SQLite will not ALTER a NOT NULL column in. \
         A migration naming any OTHER table is a second table nobody specified. \
         Generation 1 contributes none"
    );
}

/// Review Focus 1. Both generations mount on one router, and
/// `axum::Router::route` *panics* on a duplicate path+method — so a generation-2
/// route left at `/usage-collector/v1/` fails at boot, where no single-surface
/// unit test would see it. `register_rest` returning at all is half the
/// assertion; the two dispatches prove each prefix reaches a handler.
#[tokio::test]
async fn both_surfaces_mount_on_one_router_and_dispatch_independently() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use toolkit::api::OpenApiRegistryImpl;
    use toolkit_security::SecurityContext;
    use tower::ServiceExt;

    let hub = Arc::new(ClientHub::new());
    let resolver: Arc<dyn AuthZResolverApi> = CountingAllowAllResolver::new();
    hub.register::<dyn AuthZResolverApi>(resolver);
    let ctx = make_ctx_with_db(hub).await;

    let module = UsageCollectorModule::default();
    module.init(&ctx).await.expect("init succeeds");

    // Panics here if the two generations registered a colliding path.
    let router = module
        .register_rest(&ctx, axum::Router::new(), &OpenApiRegistryImpl::new())
        .expect("register_rest succeeds");

    // Each prefix must be answered by **its own** generation's handler. Three
    // failure modes this separates, where `assert_ne!(status, NOT_FOUND)`
    // separated none of them:
    //
    // - `404`: the registrar did not mount the route at all.
    // - `500`: the route is mounted but a required `Extension` is missing —
    //   what a mis-sequenced `Router::layer` in `module.rs::register_rest`
    //   produces, since `layer` reaches only the routes added before it. Axum
    //   answers that with `text/plain`, never the gear's `Problem` envelope.
    // - a `Problem` naming the *other* generation's base type: the prefixes
    //   are crossed, each surface serving the other's requests.
    //
    // The oracle is the one response both generations can produce without a
    // resolvable storage plugin: the missing-query-parameter rejection, whose
    // envelope carries the generation's own `resource_type` and its own
    // spelling of the parameter (`gts_id` in generation 1, renamed to
    // `gts_type_id` in generation 2). A `SecurityContext` is supplied because
    // without one both surfaces fail in the extractor with the same
    // harness-authored `500`, which would mask the `500` that matters.
    for (path, generation, resource_type, parameter) in [
        (
            "/usage-collector/v1/records",
            "generation 1",
            "gts.cf.core.uc.usage_record.v1~",
            "gts_id",
        ),
        (
            "/usage-collector/v2/records",
            "generation 2",
            "gts.cf.core.uc.usage_record.v2~",
            "gts_type_id",
        ),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(path)
                    .extension(SecurityContext::anonymous())
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("router responds");

        let status = response.status();
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body reads");
        let body = String::from_utf8_lossy(&body);

        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{generation}: {path} answered {status} with body {body}. A 404 means the \
             registrar did not mount it; a 500 means it is mounted without the \
             `Extension` its handlers need, which is what reordering \
             `register_rest`'s two registration blocks produces"
        );
        assert_eq!(
            content_type, "application/problem+json",
            "{generation}: {path} did not answer with the gear's own RFC-9457 \
             envelope, so the response came from the framework rather than from a \
             handler"
        );
        assert!(
            body.contains(resource_type),
            "{generation}: {path} was answered by a handler naming another \
             generation's base type; expected {resource_type} in {body}"
        );
        assert!(
            body.contains(parameter),
            "{generation}: {path} was answered by the other generation's handler; \
             expected the missing-parameter violation to name {parameter} in {body}"
        );
    }
}

/// Review Focus 2. `ClientHub` is keyed by Rust type, so the two client traits
/// cannot collide — but an `init` that forgets one leaves in-process consumers
/// of that generation broken with no REST symptom at all.
#[tokio::test]
async fn init_registers_both_client_traits() {
    use usage_collector_sdk::UsageCollectorClientV1;
    use usage_collector_sdk::v2::UsageCollectorClientV2;

    let hub = Arc::new(ClientHub::new());
    let resolver: Arc<dyn AuthZResolverApi> = CountingAllowAllResolver::new();
    hub.register::<dyn AuthZResolverApi>(resolver);
    let ctx = make_ctx_with_db(Arc::clone(&hub)).await;

    let module = UsageCollectorModule::default();
    module.init(&ctx).await.expect("init succeeds");

    assert!(
        hub.get::<dyn UsageCollectorClientV1>().is_ok(),
        "generation 1's client must stay registered"
    );
    assert!(
        hub.get::<dyn UsageCollectorClientV2>().is_ok(),
        "generation 2's client must be registered"
    );
}

/// Review Focus 3. GTS content reaches `types-registry` through a process-wide
/// link-time inventory, so a generation-2 identifier left at `.v1` collides
/// with generation 1's and fails admission at boot with an error far from the
/// cause.
#[test]
fn gts_inventory_carries_both_generations_with_no_duplicate_ids() {
    use std::collections::BTreeSet;

    use toolkit_gts::{InventoryInstance, InventoryTypeSchema};

    let ids: Vec<&'static str> = inventory::iter::<InventoryTypeSchema>
        .into_iter()
        .map(|entry| entry.type_id)
        .chain(
            inventory::iter::<InventoryInstance>
                .into_iter()
                .map(|entry| entry.instance_id),
        )
        .filter(|id| id.contains("cf.core.uc."))
        .collect();

    let unique: BTreeSet<&&str> = ids.iter().collect();
    assert_eq!(
        ids.len(),
        unique.len(),
        "duplicate usage-collector GTS id in the inventory: {ids:?}"
    );

    // `cf.core.uc.usage_record.v1~` is deliberately NOT in this list, and its
    // absence is a fact about generation 1 rather than an oversight: the
    // generation-1 base type is named by `usage_collector_sdk`'s
    // `USAGE_RECORD_RESOURCE` const and by the storage plugins' fixtures, but
    // nothing in generation 1 ever submits a `#[gts_type_schema]` for it — the
    // gateway's own `src/gts/` carries permissions only. Generation 2's
    // `src/v2/gts/usage_record.rs` is the first registration of an ingestion
    // base type in this crate. Generation 1's presence in the inventory is
    // therefore pinned through the two ids it does contribute: its plugin spec
    // type and its permission catalog.
    for expected in [
        "cf.core.uc.plugin.v1~",
        "cf.core.uc.plugin.v2~",
        "cf.core.uc.usage_record.v2~",
        "cf.core.uc.usage_record_create.v1",
        "cf.core.uc.usage_record_create.v2",
    ] {
        assert!(
            ids.iter().any(|id| id.contains(expected)),
            "{expected} missing from the inventory; found {ids:?}"
        );
    }
}
