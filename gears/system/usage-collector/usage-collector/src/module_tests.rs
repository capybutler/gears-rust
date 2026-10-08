//! Lifecycle tests for the `UsageCollectorModule` gear.
//!
//! Covers the three failure branches that previously had no direct
//! coverage:
//!
//! 1. `init` returns an error when no `AuthZResolverApi` is registered
//!    in `ClientHub` — and the `ClientHubError` is preserved as the
//!    `anyhow::Error` source (regression for RUST-ERR-001).
//! 2. A second `init` call surfaces the `OnceLock` "already initialized"
//!    guard.
//! 3. `register_rest` invoked before `init` surfaces the
//!    "Service not initialized" guard.
//!
//! The module runs no background lifecycle task (the usage-type catalog's
//! gauge-refresh loop is gone with the catalog), so there is no `serve`
//! entry point left to test here.

use std::sync::Arc;

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverApi;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistryImpl;
use toolkit::config::ConfigProvider;
use toolkit::contracts::DatabaseCapability;
use toolkit::registry::RegistryBuilder;
use toolkit::{ClientHub, Gear, GearCtx, RestApiCapability};
use toolkit_db::{ConnectOpts, DBProvider, connect_db};
use uuid::Uuid;

use super::UsageCollectorModule;
use crate::domain::test_support::CountingAllowAllResolver;

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

/// `make_ctx`, plus an in-memory `SQLite` database attached — `init` now calls
/// `ctx.db_required()` for the declaration mirror, so any test that
/// drives a full, successful `init` needs a database on the context or it
/// fails closed before ever reaching the assertions it actually wants to
/// make. No migration is applied: `init` only stores the `DBProvider` inside
/// a `DbDeclarationMirror`, it never queries it, so the mirror table does not
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

#[tokio::test]
async fn init_fails_when_authz_resolver_missing() {
    let hub = Arc::new(ClientHub::new());
    let ctx = make_ctx(hub);
    let module = UsageCollectorModule::default();

    let err = module
        .init(&ctx)
        .await
        .expect_err("init must fail when no authz-resolver client is registered");

    let top = format!("{err}");
    assert!(
        top.contains("usage-collector") && top.contains("authz-resolver"),
        "top-level message should name the gear and dependency, got: {top}"
    );

    // RUST-ERR-001 regression: the underlying `ClientHubError` MUST be
    // preserved as `source()` so the `{:#}` chain renders both the
    // contextual message and the not-found cause.
    let source = err
        .source()
        .expect("anyhow::Context::with_context must preserve the ClientHubError source");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("usage-collector") && chain.contains("not found"),
        "alternate-formatted chain should include both context and ClientHubError cause, got: {chain}"
    );
    // Source itself is a `ClientHubError::NotFound`; touch it to keep the
    // assertion hard against API-shape changes.
    let _ = source.to_string();
}

/// Own mutation beyond the brief's: ruling J1 requires `ctx.db_required()`,
/// not `ctx.db()` — a configured database is a deployment obligation now,
/// not an optional one. No test the brief sketched would catch a silent
/// drop of that call: `init_fails_when_already_initialized`'s context
/// carries a database precisely so its first `init` can succeed, so a
/// mutation removing `ctx.db_required()?` from `init` reds nothing there
/// (the call becomes dead code, not a missing check) — verified below.
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

#[tokio::test]
async fn init_fails_when_already_initialized() {
    let hub = Arc::new(ClientHub::new());
    let resolver: Arc<dyn AuthZResolverApi> = CountingAllowAllResolver::new();
    hub.register::<dyn AuthZResolverApi>(resolver);

    // The first `init` call below must succeed in full (that is the whole
    // point of this test — the SECOND call is what is expected to fail), so
    // this context needs a database wired for `ctx.db_required()`.
    let ctx = make_ctx_with_db(hub).await;
    let module = UsageCollectorModule::default();

    module.init(&ctx).await.expect("first init must succeed");

    let err = module
        .init(&ctx)
        .await
        .expect_err("second init must fail with the OnceLock guard");

    let msg = format!("{err}");
    assert!(
        msg.contains("usage-collector") && msg.contains("already initialized"),
        "second-init error should name the gear and the guard, got: {msg}"
    );
}

#[test]
fn register_rest_fails_when_service_not_initialized() {
    let hub = Arc::new(ClientHub::new());
    let ctx = make_ctx(hub);
    let module = UsageCollectorModule::default();
    let openapi = OpenApiRegistryImpl::new();

    let err = module
        .register_rest(&ctx, axum::Router::new(), &openapi)
        .expect_err("register_rest must fail when init has not run");

    let msg = format!("{err}");
    assert!(
        msg.contains("usage-collector") && msg.contains("not initialized"),
        "register_rest error should report the missing Service, got: {msg}"
    );
}

/// Dummy core for the two gears `UsageCollectorModule` declares as
/// dependencies — this test only needs them to exist as registry entries so
/// `RegistryBuilder::build_topo_sorted` can resolve `usage-collector`'s
/// `deps` edges; their own capabilities are not under test here.
#[derive(Default)]
struct DummyDepCore;

#[async_trait]
impl toolkit::Gear for DummyDepCore {
    async fn init(&self, _ctx: &GearCtx) -> anyhow::Result<()> {
        Ok(())
    }
}

/// The mirror is the gear's first durable table
/// (cpt-cf-usage-collector-adr-declaration-rehydration), so the gear now
/// needs the capability its module doc used to say it did not have.
///
/// Pinned indirectly, NOT through a `UsageCollectorModule::CAPABILITIES`
/// constant as the task brief's draft sketch assumed: reading
/// `#[toolkit::gear(...)]`'s expansion (`libs/toolkit-macros/src/lib.rs`)
/// shows it generates `MODULE_NAME` and a hidden `inventory::submit!`
/// registrator whose body calls `RegistryBuilder::register_db_with_meta` /
/// `register_rest_with_meta` for each declared capability — it emits no
/// public capability-list constant on the struct itself, so
/// `UsageCollectorModule::CAPABILITIES` does not exist and would not
/// compile. `GearCtx` (`libs/toolkit/src/context.rs`) was the brief's
/// fallback suggestion, but it exposes no capability query either — only
/// config/db/client-hub accessors.
///
/// Also NOT `GearRegistry::discover_and_build()` (the mechanism `cluster`'s
/// and `gear-orchestrator`'s own test suites use for this exact purpose,
/// e.g. `gears/system/cluster/cluster/tests/consumer_wiring.rs`): tried
/// first, and it fails here with `UnknownDependency { depends_on:
/// "types-registry" }`. The macro's `dep_reexports` — the `pub use
/// ::#crate_ident as _gear_dep_#crate_ident;` that force-links a declared
/// dependency's own `inventory::submit!` registrator when this gear is
/// "pulled in transitively" — is generated `#[cfg(not(test))]`
/// (`libs/toolkit-macros/src/lib.rs`), deliberately, so that a gear's own
/// unit tests do not drag in every transitive dependency's registration.
/// That means in THIS crate's `--lib` test binary, `types-registry`'s and
/// `authz-resolver`'s own registrators are never force-kept, and nothing
/// else in this test binary references those crates either, so
/// `discover_and_build`'s inventory scan never finds them — not a defect in
/// this gear's wiring, just the wrong tool for a `--lib` unit test.
///
/// So this test instead calls the hidden registrator
/// `#[toolkit::gear(...)]` generated for `UsageCollectorModule` directly —
/// `__usage_collector_module_registrator`, from `registrator_name =
/// format_ident!("__{}_registrator", struct_name_snake)` in the same macro
/// — against a `RegistryBuilder` seeded with dummy cores for the two
/// declared `deps` so the builder's own dependency-edge validation has
/// something to resolve against. The function has no `pub` and lives at
/// `module.rs`'s top level, i.e. in the `module` module; `module_tests` is
/// `module`'s own child (`#[path = "module_tests.rs"] mod module_tests;`),
/// and a child module can see its parent's private items, so `super::`
/// reaches it.
#[test]
fn the_gear_declares_the_db_capability() {
    let mut b = RegistryBuilder::default();
    b.register_core_with_meta("types-registry", &[], Arc::new(DummyDepCore));
    b.register_core_with_meta("authz-resolver", &[], Arc::new(DummyDepCore));

    super::__usage_collector_module_registrator(&mut b);

    let registry = b.build_topo_sorted().expect("registry builds");
    let entry = registry
        .gears()
        .iter()
        .find(|e| e.name() == UsageCollectorModule::MODULE_NAME)
        .expect("usage-collector gear must be discovered in the registry");

    // Fix round 1, item 4: pins the exact capability SET, not just `has_db()`
    // — the sibling migration test
    // (`the_gear_offers_exactly_the_declaration_mirror_migrations`) already
    // pins its own roster exactly rather than by count/presence, and
    // `labels()` was already being called for the failure message, so this
    // is the same discipline applied one step further. `#[toolkit::gear]`
    // registers capabilities in the ORDER the attribute lists them
    // (`capabilities = [rest, db]`), so `["rest", "db"]` is also an implicit
    // pin on that declared order, not just on set membership.
    assert_eq!(
        entry.caps().labels(),
        vec!["rest", "db"],
        "usage-collector must declare exactly rest + db -- no more, no fewer"
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
         A migration naming any OTHER table is a second table nobody specified"
    );
}
