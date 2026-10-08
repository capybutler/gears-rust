//! Cache-policy tests against a scripted `DeclarationSource` fake.
//!
//! Cache policy is the whole point of this component (DESIGN §3.2, §3.5), so
//! each behaviour below gets a dedicated test rather than being folded into
//! `declaration_tests` / `metadata_tests`, which exercise the parsing and
//! validation halves this module also holds.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;
use types_registry_sdk::{GtsTypeId, GtsTypeSchema};
use usage_collector_sdk::{AggregationFold, MeterTypeId};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::declaration_mirror::{
    DeclarationMirror, MirrorError, NoopDeclarationMirror,
};
use crate::domain::ports::declarations::{DeclarationRegistrar, DeclarationSource};
use crate::domain::ports::metrics::{TypeResolutionOutcome, UsageCollectorMetrics};

use super::{TypeResolver, TypeResolverConfig};

/// Counts `record_type_resolution` calls per [`TypeResolutionOutcome`], so
/// these cache-policy tests can pin that each of the resolver's real
/// branches reports the outcome DESIGN §3.11.5's `uc_type_resolution_total`
/// expects — not just that `resolve` itself returns the right `Result`.
///
/// Also records the last `uc_resolved_types` / `uc_declaration_cache_age_seconds`
/// gauge value, for the Step 6/9 cache-instrument tests. `resolved_types`
/// starts `None` (never set) rather than `Some(0)`, and `cache_age_seconds`
/// starts `None` (never set) rather than `Some(None)`, so a test can tell
/// "the resolver never touched this gauge" apart from "the resolver set it
/// to empty" — the same distinction [`UsageCollectorMetrics::set_declaration_cache_age_seconds`]'s
/// own `None` arm exists to preserve on the real adapter.
///
/// Also records every `uc_type_resolution_duration_seconds` observation, in
/// call order, so a test can assert which outcomes were and were not
/// observed — `resolve`'s two `CacheHit` sites and `populate`'s `CacheMiss`
/// one.
#[derive(Default)]
struct RecordingMetrics {
    cache_hit: AtomicUsize,
    cache_miss: AtomicUsize,
    served_stale: AtomicUsize,
    restored: AtomicUsize,
    unresolved: AtomicUsize,
    registry_error: AtomicUsize,
    mirror_write_failures: AtomicUsize,
    resolved_types: StdMutex<Option<u64>>,
    // Genuinely three states (never called / called with None / called with
    // Some(n)), which is exactly what the outer `Option` is here for — see
    // the doc above.
    #[allow(clippy::option_option)]
    cache_age_seconds: StdMutex<Option<Option<u64>>>,
    duration_observations: StdMutex<Vec<(TypeResolutionOutcome, f64)>>,
}

impl RecordingMetrics {
    fn arc() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

impl UsageCollectorMetrics for RecordingMetrics {
    fn set_pdp_ready(&self, _: bool) {}
    fn record_pdp_decision(
        &self,
        _: crate::domain::ports::metrics::PdpOp,
        _: crate::domain::ports::metrics::AuthzDecision,
        _: f64,
    ) {
    }
    fn record_pdp_failure(
        &self,
        _: crate::domain::ports::metrics::PdpOp,
        _: crate::domain::ports::metrics::PdpFailureCause,
        _: f64,
    ) {
    }
    fn set_plugin_ready(&self, _: bool) {}
    fn record_plugin_call(&self, _: crate::domain::ports::metrics::PluginOp, _: f64) {}
    fn record_plugin_accept_error(
        &self,
        _: crate::domain::ports::metrics::PluginOp,
        _: crate::domain::ports::metrics::PluginErrorCategory,
    ) {
    }
    fn observe_ingestion_batch_size(&self, _: u64) {}
    fn observe_ingestion_duration(&self, _: f64, _: usage_collector_sdk::RecordOrigin) {}
    fn observe_record_metadata_bytes(&self, _: u64) {}
    fn record_ingestion_record(
        &self,
        _: crate::domain::ports::metrics::RecordOutcome,
        _: usage_collector_sdk::EntryType,
        _: usage_collector_sdk::RecordOrigin,
        _: crate::domain::ports::metrics::RecordErrorCategory,
    ) {
    }
    fn record_ingestion_request(
        &self,
        _: crate::domain::ports::metrics::IngestRequestOutcome,
        _: crate::domain::ports::metrics::IngestRequestErrorCategory,
    ) {
    }
    fn record_quota_rejection(&self, _: u64) {}
    fn set_quota_buckets_active(&self, _: u64) {}
    fn query_inflight_inc(&self, _: crate::domain::ports::metrics::QueryKind) {}
    fn query_inflight_dec(&self, _: crate::domain::ports::metrics::QueryKind) {}
    fn observe_query_result_rows(&self, _: crate::domain::ports::metrics::QueryKind, _: u64) {}
    fn record_query_request(
        &self,
        _: crate::domain::ports::metrics::QueryKind,
        _: crate::domain::ports::metrics::RequestOutcome,
        _: crate::domain::ports::metrics::QueryErrorCategory,
        _: f64,
    ) {
    }
    fn record_feed_request(
        &self,
        _: crate::domain::ports::metrics::RequestOutcome,
        _: crate::domain::ports::metrics::FeedErrorCategory,
        _: f64,
    ) {
    }
    fn observe_feed_page_entries(&self, _: u64) {}
    fn record_type_resolution(&self, outcome: TypeResolutionOutcome) {
        let counter = match outcome {
            TypeResolutionOutcome::CacheHit => &self.cache_hit,
            TypeResolutionOutcome::CacheMiss => &self.cache_miss,
            TypeResolutionOutcome::ServedStale => &self.served_stale,
            TypeResolutionOutcome::Restored => &self.restored,
            TypeResolutionOutcome::Unresolved => &self.unresolved,
            TypeResolutionOutcome::RegistryError => &self.registry_error,
        };
        counter.fetch_add(1, Ordering::SeqCst);
    }
    fn set_resolved_types(&self, count: u64) {
        *self.resolved_types.lock().expect("lock") = Some(count);
    }
    fn set_declaration_cache_age_seconds(&self, age: Option<u64>) {
        *self.cache_age_seconds.lock().expect("lock") = Some(age);
    }
    fn observe_type_resolution_duration(&self, outcome: TypeResolutionOutcome, seconds: f64) {
        self.duration_observations
            .lock()
            .expect("lock")
            .push((outcome, seconds));
    }
    fn record_declaration_mirror_write_failure(&self) {
        self.mirror_write_failures.fetch_add(1, Ordering::SeqCst);
    }
}

const METER: &str = "gts.cf.core.uc.usage_record.v1~example.metering._.stored_volume.v1~";
const BASE: &str = "gts.cf.core.uc.usage_record.v1~";

fn meter_id() -> MeterTypeId {
    MeterTypeId::new(METER).expect("valid meter id")
}

/// A second meter id, distinct from [`meter_id`], for tests that need more
/// than one cached key (capacity eviction).
fn meter_id_b() -> MeterTypeId {
    MeterTypeId::new("gts.cf.core.uc.usage_record.v1~example.metering._.network_egress.v1~")
        .expect("valid meter id")
}

/// A third meter id, distinct from both [`meter_id`] and [`meter_id_b`], for
/// the same purpose.
fn meter_id_c() -> MeterTypeId {
    MeterTypeId::new("gts.cf.core.uc.usage_record.v1~example.metering._.api_calls.v1~")
        .expect("valid meter id")
}

/// Builds a base + derived schema pair declaring `unit` as the canonical unit
/// and `retention` as the declared retention trait, the way
/// `declaration_tests::schema_with_traits` does. [`schema`] delegates here
/// with retention fixed at `"P125D"`, so the shape below has one home.
///
/// `x-gts-traits` is placed at the **top level** of the derived schema's raw
/// JSON, not nested inside an `allOf` branch: `GtsTypeSchema::extract_traits`
/// (which `effective_traits()` reads) only reads `schema.get("x-gts-traits")`
/// at the top of the raw value, so a nested trait block is invisible to it and
/// every lookup comes back "not declared".
fn schema_with_retention(unit: &str, retention: &str) -> GtsTypeSchema {
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
            // A restore test using this fixture as a mirrored document needs
            // it to carry `$id` itself, or it hits `document_names`'s refusal.
            // Harmless elsewhere: `GtsTypeSchema::try_new` never cross-checks
            // `$id` against the `type_id` it is passed.
            "$id": format!("gts://{METER}"),
            "allOf": [
                { "$ref": format!("gts://{BASE}") }
            ],
            "x-gts-traits": {
                "aggregation_fold": "SUM",
                "canonical_unit": unit,
                "retention": retention
            }
        }),
        None,
        Some(Arc::new(base)),
    )
    .expect("derived schema")
}

/// [`schema_with_retention`] with retention fixed at `"P125D"` — every
/// pre-Task-5 call site, and every Task 5 test that does not itself need
/// retention to vary, goes through this name.
fn schema(unit: &str) -> GtsTypeSchema {
    schema_with_retention(unit, "P125D")
}

/// [`schema`] with the meter identifier threaded through rather than
/// hardcoded to [`METER`]: a test resolving more than one meter needs each
/// document to self-report its own identity rather than all sharing one.
fn schema_for(id: &MeterTypeId) -> GtsTypeSchema {
    let base = GtsTypeSchema::try_new(
        GtsTypeId::try_new(BASE).expect("base type id"),
        json!({ "type": "object" }),
        None,
        None,
    )
    .expect("base schema");
    GtsTypeSchema::try_new(
        GtsTypeId::try_new(id.as_str()).expect("meter type id"),
        json!({
            "$id": format!("gts://{id}"),
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

/// A schema whose body carries [`meter_id`]'s own `$id` — so a restore's
/// `document_names` check accepts it — but declares no `x-gts-traits`, so
/// `ResolvedDeclaration::from_schema` fails on the missing `aggregation_fold`.
///
/// Built as a **root** type-schema (`type_id = BASE`, no parent):
/// `GtsTypeSchema::try_new` never cross-checks the raw body's `$id` against
/// the `type_id` it is passed, and `ResolvedDeclaration::from_schema` reads
/// `effective_traits()` only, so a root construction suffices to exercise the
/// missing-trait failure.
fn bare_schema_without_traits() -> GtsTypeSchema {
    GtsTypeSchema::try_new(
        GtsTypeId::try_new(BASE).expect("base type id"),
        json!({
            "$id": format!("gts://{METER}"),
            "type": "object"
        }),
        None,
        None,
    )
    .expect("schema with no declared traits")
}

/// Scripted source: each call pops the next outcome, and the last one
/// repeats for any further calls.
struct FakeSource {
    outcomes: Mutex<Vec<Result<GtsTypeSchema, DomainError>>>,
    calls: AtomicUsize,
    delay: Option<Duration>,
}

impl FakeSource {
    fn new(outcomes: Vec<Result<GtsTypeSchema, DomainError>>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes),
            calls: AtomicUsize::new(0),
            delay: None,
        })
    }

    /// A source whose `fetch` takes `delay` to resolve, widening the race
    /// window a broken single-flight implementation would fall into.
    fn slow(outcomes: Vec<Result<GtsTypeSchema, DomainError>>, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes),
            calls: AtomicUsize::new(0),
            delay: Some(delay),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl DeclarationSource for FakeSource {
    async fn fetch(&self, _id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(d) = self.delay {
            tokio::time::sleep(d).await;
        }
        let mut outcomes = self.outcomes.lock().await;
        if outcomes.len() > 1 {
            outcomes.remove(0)
        } else {
            match outcomes.first() {
                Some(Ok(s)) => Ok(s.clone()),
                Some(Err(e)) => Err(e.clone()),
                None => Err(DomainError::Internal("FakeSource exhausted".to_owned())),
            }
        }
    }

    async fn fetch_by_uuid(&self, type_uuid: Uuid) -> Result<GtsTypeSchema, DomainError> {
        // This module's cache-policy tests drive the forward direction only;
        // `MeterReverseResolver` has its own fixtures in `meter_reverse_tests`.
        unimplemented!("fetch_by_uuid({type_uuid}) is not reached by this fake")
    }
}

/// A scripted [`DeclarationMirror`] that counts reads and writes separately.
///
/// Counting both is what makes the warm-path obligation assertable: §6's
/// first acceptance criterion requires *"no `types-registry` call, no
/// mirror-table read, and no mirror-table write, verified by instrumenting
/// all three and asserting zero calls on the second reference"*.
#[derive(Default)]
struct FakeMirror {
    rows: StdMutex<std::collections::HashMap<String, serde_json::Value>>,
    /// The `type_uuid` each successful `upsert` was called with, alongside
    /// `rows`'s document. Without it nothing here could see the argument
    /// `TypeResolver::populate` threads through (`schema.type_uuid`), so a
    /// mutation of that call site to a constant would go unnoticed.
    type_uuids: StdMutex<std::collections::HashMap<String, Uuid>>,
    reads: AtomicUsize,
    writes: AtomicUsize,
    write_fails: bool,
    read_fails: bool,
    /// Delays `upsert` by this long before it writes — the mirror-side
    /// counterpart of `FakeSource::slow`'s delayed `fetch`, for pinning the
    /// exposure `Self::rehydrate`'s (the real resolver's) doc on
    /// `mirror_document` names: the mirror write is awaited on the request
    /// path, inside the single-flight gate.
    delay: Option<Duration>,
}

impl FakeMirror {
    fn arc() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A mirror whose writes always fail — ADR Confirmation 3.
    fn failing_writes() -> Arc<Self> {
        Arc::new(Self {
            write_fails: true,
            ..Self::default()
        })
    }

    /// A mirror whose `upsert` takes `delay` to return, otherwise behaving
    /// normally — for proving a slow write still completes the resolution
    /// and is not ALSO counted as a failed one.
    fn slow(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            delay: Some(delay),
            ..Self::default()
        })
    }

    /// A mirror whose reads always fail — spec ruling J6.
    fn failing_reads() -> Arc<Self> {
        Arc::new(Self {
            read_fails: true,
            ..Self::default()
        })
    }

    /// Seeds a row directly, standing in for "this meter was resolved before
    /// the registry lost it".
    fn seeded(id: &MeterTypeId, document: serde_json::Value) -> Arc<Self> {
        let mirror = Self::default();
        mirror
            .rows
            .lock()
            .expect("lock")
            .insert(id.as_str().to_owned(), document);
        Arc::new(mirror)
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }

    fn row(&self, id: &MeterTypeId) -> Option<serde_json::Value> {
        self.rows.lock().expect("lock").get(id.as_str()).cloned()
    }

    /// The `type_uuid` the most recent successful `upsert` for `id` was
    /// called with, or `None` if `id` was never written.
    fn type_uuid(&self, id: &MeterTypeId) -> Option<Uuid> {
        self.type_uuids
            .lock()
            .expect("lock")
            .get(id.as_str())
            .copied()
    }
}

#[async_trait]
impl DeclarationMirror for FakeMirror {
    async fn upsert(
        &self,
        id: &MeterTypeId,
        type_uuid: Uuid,
        document: &serde_json::Value,
    ) -> Result<(), MirrorError> {
        if let Some(d) = self.delay {
            tokio::time::sleep(d).await;
        }
        self.writes.fetch_add(1, Ordering::SeqCst);
        if self.write_fails {
            return Err(MirrorError::new("scripted write failure"));
        }
        self.rows
            .lock()
            .expect("lock")
            .insert(id.as_str().to_owned(), document.clone());
        self.type_uuids
            .lock()
            .expect("lock")
            .insert(id.as_str().to_owned(), type_uuid);
        Ok(())
    }

    async fn read(&self, id: &MeterTypeId) -> Result<Option<serde_json::Value>, MirrorError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.read_fails {
            return Err(MirrorError::new("scripted read failure"));
        }
        Ok(self.rows.lock().expect("lock").get(id.as_str()).cloned())
    }

    async fn resolve_id(&self, _type_uuid: Uuid) -> Result<Option<MeterTypeId>, MirrorError> {
        Ok(None)
    }
}

/// A scripted [`DeclarationRegistrar`] that counts calls and records the
/// documents it was handed.
#[derive(Default)]
struct FakeRegistrar {
    calls: AtomicUsize,
    documents: StdMutex<Vec<serde_json::Value>>,
    fails: bool,
}

impl FakeRegistrar {
    fn arc() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn failing() -> Arc<Self> {
        Arc::new(Self {
            fails: true,
            ..Self::default()
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn documents(&self) -> Vec<serde_json::Value> {
        self.documents.lock().expect("lock").clone()
    }
}

#[async_trait]
impl DeclarationRegistrar for FakeRegistrar {
    async fn register(&self, document: &serde_json::Value) -> Result<(), DomainError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.documents.lock().expect("lock").push(document.clone());
        if self.fails {
            return Err(DomainError::TypesRegistryUnavailable(
                "scripted registration failure".to_owned(),
            ));
        }
        Ok(())
    }
}

fn cfg(ttl: Duration) -> TypeResolverConfig {
    TypeResolverConfig { ttl, capacity: 64 }
}

/// Builds a [`TypeResolver`] with every collaborator wired, the one call
/// shape the ~40 pre-existing [`TypeResolver::new`]-based tests do not need
/// (their arity was not widened — ruling J18) and the dozen new
/// mirror/restore tests below do.
fn resolver_with(
    source: Arc<dyn DeclarationSource>,
    mirror: Arc<dyn DeclarationMirror>,
    registrar: Arc<dyn DeclarationRegistrar>,
    config: TypeResolverConfig,
    metrics: Arc<dyn UsageCollectorMetrics>,
) -> TypeResolver {
    TypeResolver::with_rehydration(source, mirror, registrar, config, metrics)
}

#[tokio::test]
async fn a_miss_populates_and_a_hit_serves_from_cache() {
    let source = FakeSource::new(vec![Ok(schema("bytes"))]);
    let metrics = RecordingMetrics::arc();
    let resolver = TypeResolver::new(source.clone(), cfg(Duration::from_mins(5)), metrics.clone());

    let first = resolver.resolve(&meter_id()).await.expect("resolves");
    assert_eq!(first.aggregation_fold, AggregationFold::Sum);
    assert_eq!(first.canonical_unit, "bytes");

    let second = resolver.resolve(&meter_id()).await.expect("resolves");
    assert_eq!(second.canonical_unit, "bytes");

    assert_eq!(source.calls(), 1, "a cache hit must not reach the registry");
    assert_eq!(
        metrics.cache_miss.load(Ordering::SeqCst),
        1,
        "the cold miss must record CacheMiss exactly once"
    );
    assert_eq!(
        metrics.cache_hit.load(Ordering::SeqCst),
        1,
        "the warm hit must record CacheHit exactly once"
    );
    // Pins `TypeResolver::new`'s DEFAULTED mirror (`NoopDeclarationMirror`,
    // ruling J18), the one combination no other test here exercises — every
    // mirror-write-failure assertion elsewhere wires an explicit `FakeMirror`.
    // Measured: swapping the default for a failing mirror reds this assertion
    // while the rest of the suite and the §3.11.5 inventory pin stay green.
    assert_eq!(
        metrics.mirror_write_failures.load(Ordering::SeqCst),
        0,
        "TypeResolver::new's defaulted mirror is NoopDeclarationMirror, whose \
         upsert always succeeds -- it must never increment this counter"
    );
}

#[tokio::test(start_paused = true)]
async fn concurrent_misses_make_one_source_call() {
    // Without single-flight a cold key under load fans a burst of identical
    // reads at types-registry, which is exactly the hot-path coupling the
    // cache exists to prevent. The 50ms fetch delay opens a race window
    // that the async gate must close; time is paused and auto-advances past
    // the delay once every task is parked, so the test is instant and
    // deterministic rather than depending on real scheduling.
    let source = FakeSource::slow(vec![Ok(schema("bytes"))], Duration::from_millis(50));
    let resolver = Arc::new(TypeResolver::new(
        source.clone(),
        cfg(Duration::from_mins(5)),
        RecordingMetrics::arc(),
    ));

    let mut handles = Vec::new();
    for _ in 0..8 {
        let r = resolver.clone();
        handles.push(tokio::spawn(async move { r.resolve(&meter_id()).await }));
    }
    for h in handles {
        h.await.expect("task joins").expect("resolves");
    }

    assert_eq!(
        source.calls(),
        1,
        "single-flight must collapse concurrent misses"
    );
}

#[tokio::test(start_paused = true)]
async fn an_entry_refreshes_past_the_ttl() {
    let source = FakeSource::new(vec![Ok(schema("bytes")), Ok(schema("count"))]);
    let resolver = TypeResolver::new(
        source.clone(),
        cfg(Duration::from_millis(20)),
        RecordingMetrics::arc(),
    );

    let first = resolver.resolve(&meter_id()).await.expect("first resolve");
    assert_eq!(first.canonical_unit, "bytes");

    // Paused time auto-advances past this sleep (nothing else is runnable),
    // so this clears the 20ms TTL instantly rather than via a real delay.
    tokio::time::sleep(Duration::from_millis(40)).await;

    let second = resolver.resolve(&meter_id()).await.expect("second resolve");
    assert_eq!(
        second.canonical_unit, "count",
        "past the TTL the entry refreshes"
    );
    assert_eq!(source.calls(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_registry_error_past_the_ttl_serves_the_stale_entry() {
    // DESIGN 3.5: cached declarations stay usable while the registry is
    // unreachable, so an outage degrades new-type introduction rather than
    // ingestion of existing types.
    let source = FakeSource::new(vec![
        Ok(schema("bytes")),
        Err(DomainError::TypesRegistryUnavailable(
            "connect refused".to_owned(),
        )),
    ]);
    let metrics = RecordingMetrics::arc();
    let resolver = TypeResolver::new(
        source.clone(),
        cfg(Duration::from_millis(20)),
        metrics.clone(),
    );

    resolver.resolve(&meter_id()).await.expect("first resolve");

    tokio::time::sleep(Duration::from_millis(40)).await;

    let stale = resolver
        .resolve(&meter_id())
        .await
        .expect("a stale entry must still serve while the registry is down");
    assert_eq!(stale.canonical_unit, "bytes");
    assert_eq!(
        source.calls(),
        2,
        "the refresh attempt still reaches the registry once"
    );
    assert_eq!(metrics.cache_miss.load(Ordering::SeqCst), 1);
    assert_eq!(
        metrics.served_stale.load(Ordering::SeqCst),
        1,
        "the registry-unavailable-past-TTL refresh must record ServedStale"
    );
}

#[tokio::test]
async fn a_registry_error_with_nothing_cached_fails_closed() {
    let source = FakeSource::new(vec![Err(DomainError::TypesRegistryUnavailable(
        "connect refused".to_owned(),
    ))]);
    let metrics = RecordingMetrics::arc();
    let resolver = TypeResolver::new(source, cfg(Duration::from_mins(5)), metrics.clone());

    let err = resolver
        .resolve(&meter_id())
        .await
        .expect_err("with nothing cached the resolver must fail closed, never admit unvalidated");
    // Ruling K25 (entry 46): the cold-cache registry error now names the
    // meter it failed to resolve, ahead of the registry's own message, not
    // just that message verbatim.
    assert!(
        matches!(err, DomainError::TypesRegistryUnavailable(ref msg)
            if msg == &format!("{}: connect refused", meter_id())),
        "unexpected error variant: {err:?}"
    );
    assert!(
        format!("{err}").contains(meter_id().as_str()),
        "a fail-closed rejection must name the identifier it refused; got: {err}"
    );
    assert_eq!(
        metrics.registry_error.load(Ordering::SeqCst),
        1,
        "a registry error with nothing cached must record RegistryError, not Unresolved: \
         the registry itself failed to answer, no verdict on the type was reached"
    );
    assert_eq!(
        metrics.unresolved.load(Ordering::SeqCst),
        0,
        "a registry-unavailable failure must never record Unresolved"
    );
}

#[tokio::test]
async fn a_not_found_fails_closed_and_is_not_cached() {
    // Caching a negative answer would make a type declared a moment later
    // unusable until the entry expired.
    let source = FakeSource::new(vec![
        Err(DomainError::declaration_not_found(&meter_id())),
        Ok(schema("bytes")),
    ]);
    let metrics = RecordingMetrics::arc();
    let resolver = TypeResolver::new(source.clone(), cfg(Duration::from_mins(5)), metrics.clone());

    let err = resolver
        .resolve(&meter_id())
        .await
        .expect_err("a definite not-found must fail closed");
    assert!(
        err.is_declaration_not_found(),
        "unexpected error variant: {err:?}"
    );

    let after = resolver
        .resolve(&meter_id())
        .await
        .expect("a freshly declared type resolves without waiting out a negative TTL");
    assert_eq!(after.canonical_unit, "bytes");
    assert_eq!(source.calls(), 2);
    assert_eq!(
        metrics.unresolved.load(Ordering::SeqCst),
        1,
        "a definite not-found must record Unresolved, not RegistryError: the registry \
         answered fine, the type is simply not there"
    );
    assert_eq!(
        metrics.registry_error.load(Ordering::SeqCst),
        0,
        "a definite not-found must never record RegistryError"
    );
    assert_eq!(
        metrics.cache_miss.load(Ordering::SeqCst),
        1,
        "the following successful resolve must record CacheMiss"
    );
}

#[tokio::test]
async fn an_incomplete_declaration_fails_closed_and_is_not_cached_as_success() {
    // A schema that resolves at the registry but carries none of the traits
    // a meter needs (no `x-gts-traits` at all) fails `from_schema` parsing
    // rather than `fetch` itself. That failure must fail closed exactly like
    // a not-found, and must not poison the cache with a bogus success.
    let bad = GtsTypeSchema::try_new(
        GtsTypeId::try_new(BASE).expect("base type id"),
        json!({ "type": "object" }),
        None,
        None,
    )
    .expect("schema with no declared traits");
    let source = FakeSource::new(vec![Ok(bad), Ok(schema("bytes"))]);
    let metrics = RecordingMetrics::arc();
    let resolver = TypeResolver::new(source.clone(), cfg(Duration::from_mins(5)), metrics.clone());

    let err = resolver
        .resolve(&meter_id())
        .await
        .expect_err("an incomplete declaration must fail closed");
    assert!(
        err.is_declaration_not_found(),
        "an incomplete declaration must route through the same fail-closed \
         variant as a genuine not-found: {err:?}"
    );

    let after = resolver
        .resolve(&meter_id())
        .await
        .expect("a corrected declaration resolves on the very next call");
    assert_eq!(after.canonical_unit, "bytes");
    assert_eq!(
        source.calls(),
        2,
        "the failed parse must not have been cached as a success"
    );
    assert_eq!(
        metrics.unresolved.load(Ordering::SeqCst),
        1,
        "an incomplete declaration must record Unresolved, not RegistryError: \
         types-registry answered fine, the declaration itself is unusable"
    );
    assert_eq!(
        metrics.registry_error.load(Ordering::SeqCst),
        0,
        "an incomplete declaration must never record RegistryError"
    );
}

#[tokio::test]
async fn capacity_evicts_the_oldest_meter_when_a_new_one_arrives() {
    // A long TTL keeps every entry fresh throughout, so cache hits/misses
    // below are driven purely by eviction, not by staleness.
    let source = FakeSource::new(vec![Ok(schema("bytes"))]);
    let metrics = RecordingMetrics::arc();
    let resolver = TypeResolver::new(
        source.clone(),
        TypeResolverConfig {
            ttl: Duration::from_mins(5),
            capacity: 2,
        },
        metrics.clone(),
    );

    resolver.resolve(&meter_id()).await.expect("resolves a"); // oldest
    resolver.resolve(&meter_id_b()).await.expect("resolves b");
    resolver.resolve(&meter_id_c()).await.expect("resolves c"); // over capacity, evicts a
    assert_eq!(source.calls(), 3);
    // The one fixture where stores completed and cache size diverge: three
    // meters stored, capacity 2. A gauge wrongly set to "stores completed"
    // instead of `entries.len()` reads 3 here while still passing
    // `resolving_sets_the_resolved_type_count_to_the_cache_size`, where no
    // eviction happens.
    let resolved_types = *metrics.resolved_types.lock().expect("lock");
    assert_eq!(
        resolved_types,
        Some(2),
        "the gauge must read the capacity-bounded cache SIZE (2), not the \
         number of stores that have happened (3)",
    );

    // The two most recently fetched meters remain cached: re-resolving them
    // must not reach the source again. Checked before touching `a` again,
    // since refreshing an evicted `a` would itself perform its own
    // eviction (covered by the next test) and muddy this assertion.
    resolver
        .resolve(&meter_id_b())
        .await
        .expect("b still cached");
    resolver
        .resolve(&meter_id_c())
        .await
        .expect("c still cached");
    assert_eq!(
        source.calls(),
        3,
        "the two most recently fetched meters must remain cached"
    );

    // The oldest meter was evicted to make room for the third: resolving it
    // again must reach the source.
    resolver.resolve(&meter_id()).await.expect("a refetched");
    assert_eq!(
        source.calls(),
        4,
        "the evicted meter must be refetched, not served from a stale slot"
    );
}

/// Ruling K26: the cache must evict the least recently *resolved* meter, not
/// the one with the oldest *fetch*.
///
/// `a` is fetched first, then referenced again (a cache HIT) just before
/// capacity is reached; `b` is fetched later and never touched again. The two
/// policies disagree on the victim: oldest-fetch evicts `a`, whose
/// `fetched_at` the hit never updated, while least-recently-resolved evicts
/// the genuinely cold `b`.
/// `capacity_evicts_the_oldest_meter_when_a_new_one_arrives` never
/// re-references an entry before its eviction, so both policies name the same
/// victim there.
#[tokio::test(start_paused = true)]
async fn a_continuously_referenced_meter_survives_eviction_ahead_of_a_newer_but_untouched_one() {
    let source = FakeSource::new(vec![Ok(schema("bytes"))]);
    let resolver = TypeResolver::new(
        source.clone(),
        TypeResolverConfig {
            ttl: Duration::from_mins(5),
            capacity: 2,
        },
        RecordingMetrics::arc(),
    );

    resolver.resolve(&meter_id()).await.expect("a: cold fetch"); // oldest fetch
    tokio::time::advance(Duration::from_secs(1)).await;
    resolver
        .resolve(&meter_id_b())
        .await
        .expect("b: cold fetch");
    tokio::time::advance(Duration::from_secs(1)).await;

    // `a` is referenced again -- a cache HIT, not a fetch -- marking it the
    // more recently USED of the two even though its fetch is still the
    // older one.
    resolver.resolve(&meter_id()).await.expect("a: cache hit");
    assert_eq!(
        source.calls(),
        2,
        "the re-reference of a must be a cache hit, not a refetch"
    );

    // A third, distinct meter arrives at capacity: one of a/b must be
    // evicted to make room.
    resolver
        .resolve(&meter_id_c())
        .await
        .expect("c: cold fetch, forces eviction");

    assert!(
        resolver.stale_entry(&meter_id()).await.is_some(),
        "the continuously referenced meter (a) must survive eviction -- true \
         least-recently-resolved protects it even though its own fetch is the \
         older of the two"
    );
    assert!(
        resolver.stale_entry(&meter_id_b()).await.is_none(),
        "the meter fetched later but never referenced again (b) must be the \
         one evicted, not a"
    );
}

/// Ruling K26 has two recency-update sites: a cache hit (`fresh_entry`,
/// pinned above) and a stale serve (`handle_registry_error`'s `ServedStale`
/// arm, via `touch_last_used`). This is the stale-serve limb's discriminator:
/// `a` is fetched first, then served STALE just before capacity is reached,
/// and `b` is fetched later and never referenced again. A stale serve counts
/// as "resolved" for eviction even though it must not touch `fetched_at`, so
/// without `touch_last_used` the oldest-fetch victim `a` is evicted instead of
/// the genuinely cold `b`.
#[tokio::test(start_paused = true)]
async fn a_meter_served_stale_survives_eviction_ahead_of_a_newer_but_untouched_one() {
    let a = meter_id();
    let b = meter_id_b();
    let c = meter_id_c();
    let source = FakeSource::new(vec![
        Ok(schema_for(&a)),
        Ok(schema_for(&b)),
        // The third call -- re-referencing `a` past its TTL -- answers a
        // registry error, so `a` is served STALE rather than refreshed.
        Err(DomainError::TypesRegistryUnavailable("down".to_owned())),
        Ok(schema_for(&c)),
    ]);
    let resolver = TypeResolver::new(
        source.clone(),
        TypeResolverConfig {
            ttl: Duration::from_secs(2),
            capacity: 2,
        },
        RecordingMetrics::arc(),
    );

    resolver.resolve(&a).await.expect("a: cold fetch"); // t=0
    tokio::time::advance(Duration::from_secs(1)).await;
    resolver.resolve(&b).await.expect("b: cold fetch"); // t=1
    // `a`'s age is now 2.5s (past the 2s TTL); `b`'s is 1.5s (still fresh).
    tokio::time::advance(Duration::from_millis(1500)).await;

    resolver.resolve(&a).await.expect("a: served stale"); // t=2.5
    assert_eq!(
        source.calls(),
        3,
        "the third call must be the scripted registry error, i.e. a stale \
         serve, not a fourth fetch"
    );

    // A third, distinct meter arrives at capacity: one of a/b must be
    // evicted to make room.
    resolver
        .resolve(&c)
        .await
        .expect("c: cold fetch, forces eviction");

    assert!(
        resolver.stale_entry(&a).await.is_some(),
        "the meter just served stale (a) must survive eviction -- a stale \
         serve counts as a resolution for recency purposes, even though its \
         own fetch is the older of the two"
    );
    assert!(
        resolver.stale_entry(&b).await.is_none(),
        "the meter fetched later but never referenced again (b) must be the \
         one evicted, not a"
    );
}

#[tokio::test(start_paused = true)]
async fn refreshing_an_existing_key_at_capacity_does_not_evict_another_entry() {
    // Regression coverage for the `store` eviction guard: with the cache at
    // capacity, refreshing a key it already holds must update that key in
    // place, not misread its own refresh as "a new key needs room" and
    // evict a different entry.
    let source = FakeSource::new(vec![Ok(schema("bytes"))]);
    let resolver = TypeResolver::new(
        source.clone(),
        TypeResolverConfig {
            ttl: Duration::from_millis(20),
            capacity: 2,
        },
        RecordingMetrics::arc(),
    );

    resolver.resolve(&meter_id()).await.expect("resolves a"); // oldest
    // Paused time is frozen except when explicitly advanced or a sleep
    // auto-advances it: without this, "a" and "b" would be fetched at the
    // *identical* virtual instant and eviction's oldest-first tiebreak would
    // be undefined, which would make this test's falsification check below
    // unreliable. One millisecond keeps both entries well inside the 20ms
    // TTL while still ordering them strictly.
    tokio::time::advance(Duration::from_millis(1)).await;
    resolver.resolve(&meter_id_b()).await.expect("resolves b");
    assert_eq!(source.calls(), 2);

    // Paused time auto-advances past this sleep, clearing the TTL for both
    // entries instantly rather than via a real delay.
    tokio::time::sleep(Duration::from_millis(40)).await;

    // Refresh "b" — the *newer* of the two, not the one eviction would pick
    // if it (wrongly) ran here. If the guard were missing, `store` would
    // see the cache at capacity and evict the oldest entry ("a") to make
    // room for an insert that was actually just overwriting b's own slot.
    resolver.resolve(&meter_id_b()).await.expect("refreshes b");
    assert_eq!(source.calls(), 3);

    // `stale_entry` (not `resolve`) is the probe here so that checking on
    // "a" does not itself trigger a — now also past-TTL — refresh of "a",
    // which would confound whether it survived the refresh of "b" above.
    assert!(
        resolver.stale_entry(&meter_id()).await.is_some(),
        "refreshing an existing key at capacity must not evict another entry"
    );
}

// ── The three absent type-resolver instruments ──────────────

#[tokio::test]
async fn resolving_sets_the_resolved_type_count_to_the_cache_size() {
    // Two distinct meters resolve, so the gauge reads 2 rather than 1 — a
    // single resolution cannot distinguish "the cache size" from "one".
    let source = FakeSource::new(vec![Ok(schema("bytes"))]);
    let metrics = RecordingMetrics::arc();
    let resolver = TypeResolver::new(source, cfg(Duration::from_mins(5)), metrics.clone());

    resolver.resolve(&meter_id()).await.expect("resolves a");
    resolver.resolve(&meter_id_b()).await.expect("resolves b");

    let resolved_types = *metrics.resolved_types.lock().expect("lock");
    assert_eq!(
        resolved_types,
        Some(2),
        "two distinct meters are now cached, so the gauge must read the cache size \
         (2), not merely confirm that a resolution happened",
    );
}

#[tokio::test(start_paused = true)]
async fn the_cache_age_is_the_oldest_entrys_age_not_the_newest() {
    // Resolve meter A, let time pass, resolve meter B, assert the recorded
    // age tracks A (the oldest), not B (the newest, which just resolved and
    // would read as ~0s). Asserting only "an age was recorded" would pass
    // against a resolver reporting the newest entry's age, which is the
    // inverse of the staleness signal §3.11.6 alerts on.
    let source = FakeSource::new(vec![Ok(schema("bytes"))]);
    let metrics = RecordingMetrics::arc();
    let resolver = TypeResolver::new(source, cfg(Duration::from_mins(5)), metrics.clone());

    resolver.resolve(&meter_id()).await.expect("resolves a"); // oldest

    // Paused time auto-advances past this sleep (nothing else is runnable),
    // so this elapses 10 virtual seconds instantly rather than via a real
    // delay — and `fetched_at` is a monotonic `Instant`, so the measurement
    // is exact regardless.
    tokio::time::sleep(Duration::from_secs(10)).await;

    resolver.resolve(&meter_id_b()).await.expect("resolves b"); // newest

    let age = metrics
        .cache_age_seconds
        .lock()
        .expect("lock")
        .expect("the age gauge was set on this resolve")
        .expect("the cache is non-empty, so an age must be recorded, not None");
    assert_eq!(
        age, 10,
        "the age must track the OLDEST entry (a, resolved 10s before b), not the \
         newest (b, resolved just now, which would read as 0)",
    );
}

#[tokio::test]
async fn a_resolver_that_has_resolved_nothing_never_touches_the_age_gauge() {
    // Review Focus item 3. This pins the resolver's OWN call sites, not the
    // `None` arm itself: both of them (`store`, and the `ServedStale`
    // fallback via `sample_cache_gauges`) only ever run once something is
    // already cached, so a resolver that has resolved nothing must never
    // have called the setter at all -- not "called it with zero", which a
    // fix-round finding showed this test's previous name and body could not
    // tell apart from. See `publish_cache_gauges_reports_none_for_an_empty_map`
    // below for the `None` arm itself, which this resolver can never reach.
    let source = FakeSource::new(vec![Ok(schema("bytes"))]);
    let metrics = RecordingMetrics::arc();
    let _resolver = TypeResolver::new(source, cfg(Duration::from_mins(5)), metrics.clone());

    let cache_age_seconds = *metrics.cache_age_seconds.lock().expect("lock");
    assert_eq!(
        cache_age_seconds, None,
        "before anything is cached, the age gauge must never have been touched at \
         all -- not even with a zero reading",
    );
}

#[test]
fn publish_cache_gauges_reports_none_for_an_empty_map() {
    // The `None` arm of `set_declaration_cache_age_seconds` that
    // `TypeResolver` itself can never reach (both of its real call sites
    // only run once something is already cached -- see that method's own
    // doc). A resolver-driven test cannot exercise this arm at all, so this
    // calls the private `publish_cache_gauges` directly over an empty map,
    // which `resolver_tests` can do since it is a child module of
    // `type_resolver`.
    let metrics = RecordingMetrics::arc();
    let resolver = TypeResolver::new(
        FakeSource::new(vec![Ok(schema("bytes"))]),
        cfg(Duration::from_mins(5)),
        metrics.clone(),
    );

    resolver.publish_cache_gauges(&std::collections::HashMap::new());

    let resolved_types = *metrics.resolved_types.lock().expect("lock");
    assert_eq!(
        resolved_types,
        Some(0),
        "an empty map's resolved-types count is 0",
    );
    let cache_age_seconds = *metrics.cache_age_seconds.lock().expect("lock");
    assert_eq!(
        cache_age_seconds,
        Some(None),
        "an empty map's age must be published as None, not a defaulted zero",
    );
}

#[tokio::test(start_paused = true)]
async fn a_stale_serve_does_not_refresh_the_cache_age() {
    // Resolve a meter, then make the source fail so the next resolution
    // past the TTL takes the ServedStale path, then assert the recorded age
    // still measures from the ORIGINAL successful fetch.
    //
    // Without this, an edit that refreshed `fetched_at` on the stale path
    // would make a cache that has not reached the registry in hours report
    // as freshly refreshed, and §3.11.6's declaration-cache-staleness alert
    // would never fire again. DESIGN §3.11.5 says the age is "since its
    // last successful refresh", and a stale serve is not one.
    let source = FakeSource::new(vec![
        Ok(schema("bytes")),
        Err(DomainError::TypesRegistryUnavailable(
            "connect refused".to_owned(),
        )),
    ]);
    let metrics = RecordingMetrics::arc();
    let resolver = TypeResolver::new(source, cfg(Duration::from_secs(20)), metrics.clone());

    resolver.resolve(&meter_id()).await.expect("first resolve");

    // Captured directly from `entries`, independently of the gauge: a
    // fix-round finding showed the gauge-only assertion below cannot tell
    // "fetched_at was wrongly refreshed" apart from "the gauge was simply
    // never sampled on this path" -- both give the identical `age == 0`
    // failure. This assertion targets the first fault specifically, and
    // `resolver_tests` can reach the private `entries` field directly, the
    // same way it already reaches `stale_entry`.
    let fetched_at_before = {
        let entries = resolver.entries.read().await;
        entries
            .get(&meter_id())
            .expect("cached by the first resolve")
            .fetched_at
    };

    // Paused time auto-advances past this sleep, clearing the 20s TTL
    // instantly rather than via a real delay.
    tokio::time::sleep(Duration::from_secs(40)).await;

    resolver
        .resolve(&meter_id())
        .await
        .expect("a stale entry must still serve while the registry is down");

    let fetched_at_after = {
        let entries = resolver.entries.read().await;
        entries
            .get(&meter_id())
            .expect("still cached after the stale serve")
            .fetched_at
    };
    assert_eq!(
        fetched_at_before, fetched_at_after,
        "a stale serve must never write `fetched_at` -- only a successful fetch \
         does; this is the fetched_at-level pin, independent of whatever the \
         age gauge reports",
    );

    let age = metrics
        .cache_age_seconds
        .lock()
        .expect("lock")
        .expect("the ServedStale path samples the age gauge")
        .expect("the cache is non-empty");
    assert_eq!(
        age, 40,
        "the age must still measure from the original successful fetch (~40s ago), \
         not from this stale serve (~0s ago)",
    );
}

#[tokio::test]
async fn resolving_the_same_meter_twice_records_one_miss_duration_and_one_hit_duration() {
    // `populate`'s `CacheMiss` site and `resolve`'s FIRST `CacheHit` site
    // (the immediate `fresh_entry` check, before the single-flight gate) --
    // the simplest of `resolve`/`populate`'s three duration call sites to
    // reach. The SECOND `CacheHit` site (after the gate) needs a
    // concurrent caller; see the test below.
    let source = FakeSource::new(vec![Ok(schema("bytes"))]);
    let metrics = RecordingMetrics::arc();
    let resolver = TypeResolver::new(source, cfg(Duration::from_mins(5)), metrics.clone());

    resolver
        .resolve(&meter_id())
        .await
        .expect("first resolve (miss)");
    resolver
        .resolve(&meter_id())
        .await
        .expect("second resolve (immediate hit)");

    let durations = metrics.duration_observations.lock().expect("lock");
    let miss_count = durations
        .iter()
        .filter(|(o, _)| *o == TypeResolutionOutcome::CacheMiss)
        .count();
    let hit_count = durations
        .iter()
        .filter(|(o, _)| *o == TypeResolutionOutcome::CacheHit)
        .count();
    assert_eq!(
        miss_count, 1,
        "the cold fetch must record exactly one CacheMiss duration observation",
    );
    assert_eq!(
        hit_count, 1,
        "the immediate (non-gate-queued) cache hit must record exactly one \
         CacheHit duration observation too",
    );
}

#[tokio::test(start_paused = true)]
async fn a_concurrent_waiter_records_a_duration_on_the_post_gate_cache_hit() {
    // `resolve` has TWO `CacheHit` duration sites: the immediate check
    // above the single-flight gate, and the second check taken once a
    // waiter acquires the gate after the winner already populated the
    // cache. Nothing else reaches that second site: it requires a
    // concurrent caller that misses on its own first check, then finds the
    // entry fresh once it gets the gate -- the same race
    // `concurrent_misses_make_one_source_call` opens, reused here to
    // assert what each caller recorded rather than only the source call
    // count.
    let source = FakeSource::slow(vec![Ok(schema("bytes"))], Duration::from_millis(50));
    let metrics = RecordingMetrics::arc();
    let resolver = Arc::new(TypeResolver::new(
        source,
        cfg(Duration::from_mins(5)),
        metrics.clone(),
    ));

    let mut handles = Vec::new();
    for _ in 0..2 {
        let r = resolver.clone();
        handles.push(tokio::spawn(async move { r.resolve(&meter_id()).await }));
    }
    for h in handles {
        h.await.expect("task joins").expect("resolves");
    }

    let durations = metrics.duration_observations.lock().expect("lock");
    let miss_count = durations
        .iter()
        .filter(|(o, _)| *o == TypeResolutionOutcome::CacheMiss)
        .count();
    let hit_count = durations
        .iter()
        .filter(|(o, _)| *o == TypeResolutionOutcome::CacheHit)
        .count();
    assert_eq!(
        miss_count, 1,
        "exactly one of the two callers wins the gate and fetches",
    );
    assert_eq!(
        hit_count, 1,
        "the other caller's post-gate-wait cache hit (resolve's SECOND \
         fresh_entry check) must record a CacheHit duration too -- the site \
         the brief named as needing coverage and that nothing else reaches",
    );
}

// ── Task 5: the DESIGN §3.7 declaration mirror and restore bridge ───────

#[tokio::test]
async fn a_cold_miss_mirrors_the_document_the_registry_returned() {
    let id = meter_id();
    let schema = schema("bytes");
    let source = FakeSource::new(vec![Ok(schema.clone())]);
    let mirror = FakeMirror::arc();
    let registrar = FakeRegistrar::arc();
    let metrics = RecordingMetrics::arc();
    let resolver = resolver_with(
        source.clone(),
        mirror.clone(),
        registrar.clone(),
        cfg(Duration::from_mins(5)),
        metrics.clone(),
    );

    resolver.resolve(&id).await.expect("the meter resolves");

    assert_eq!(mirror.writes(), 1, "a cold miss must write the mirror once");
    assert_eq!(
        mirror.row(&id),
        Some(schema.raw_schema.clone()),
        "the mirror must hold the document as the registry returned it"
    );
    // Fix round 1, Important 2: the task's central wiring
    // (`populate`'s `type_uuid: schema.type_uuid`) had no test -- every
    // other double discarded the argument. This pins that the resolver
    // passes the SCHEMA's own registry reference through to the mirror, not
    // a placeholder: `schema.type_uuid` is derived deterministically from
    // the meter's own `GtsTypeId` (`GtsId::to_uuid`'s injective UUIDv5
    // branch), so a mutation of the call site to a constant (e.g.
    // `Uuid::nil()`) fails this assertion.
    assert_eq!(
        mirror.type_uuid(&id),
        Some(schema.type_uuid),
        "the mirror write must carry the resolved schema's own registry \
         reference, not a placeholder"
    );
    assert_eq!(
        registrar.calls(),
        0,
        "a successful registry read must not register anything back"
    );
    assert_eq!(metrics.mirror_write_failures.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_failed_mirror_write_is_counted_and_the_entry_is_still_served() {
    let id = meter_id();
    let source = FakeSource::new(vec![Ok(schema("bytes"))]);
    let mirror = FakeMirror::failing_writes();
    let metrics = RecordingMetrics::arc();
    let resolver = resolver_with(
        source,
        mirror.clone(),
        FakeRegistrar::arc(),
        cfg(Duration::from_mins(5)),
        metrics.clone(),
    );

    // The positive half: the operation SUCCEEDS. ADR statement 4 — "A failed
    // mirror write does not reject the entry. It costs only the ability to
    // restore that type later."
    resolver
        .resolve(&id)
        .await
        .expect("a failed mirror write must not reject the operation");

    assert_eq!(mirror.writes(), 1, "the write was attempted");
    assert_eq!(
        metrics.mirror_write_failures.load(Ordering::SeqCst),
        1,
        "the failure must be counted on uc_declaration_mirror_write_failures_total"
    );
    assert_eq!(
        metrics.cache_miss.load(Ordering::SeqCst),
        1,
        "the resolution itself is still a cache_miss"
    );
}

/// A stalled mirror write is bounded, counted, swallowed, and does not reject
/// the resolved declaration.
#[tokio::test(start_paused = true)]
async fn a_stalled_mirror_write_is_counted_and_the_entry_is_still_served() {
    let id = meter_id();
    let source = FakeSource::new(vec![Ok(schema("bytes"))]);
    let mirror = FakeMirror::slow(Duration::from_secs(5));
    let metrics = RecordingMetrics::arc();
    let resolver = resolver_with(
        source,
        mirror.clone(),
        FakeRegistrar::arc(),
        cfg(Duration::from_mins(5)),
        metrics.clone(),
    );

    let declaration = tokio::time::timeout(Duration::from_millis(100), resolver.resolve(&id))
        .await
        .expect("the resolver must bound mirror writes")
        .expect("a stalled mirror write must not reject the operation");
    assert_eq!(declaration.canonical_unit, "bytes");

    assert_eq!(mirror.writes(), 0, "the timed-out write was cancelled");
    assert_eq!(
        metrics.mirror_write_failures.load(Ordering::SeqCst),
        1,
        "a mirror write timeout is counted like a mirror write error"
    );
}

#[tokio::test]
async fn a_definite_not_found_with_a_mirror_row_restores_registers_and_serves() {
    let id = meter_id();
    let schema = schema("bytes");
    let mirror = FakeMirror::seeded(&id, schema.raw_schema.clone());
    let registrar = FakeRegistrar::arc();
    // Not-found first (the registry lost it), then the re-fetch the restore
    // makes after registering the document back — spec ruling J12.
    let source = FakeSource::new(vec![
        Err(DomainError::declaration_not_found(&id)),
        Ok(schema.clone()),
    ]);
    let metrics = RecordingMetrics::arc();
    let resolver = resolver_with(
        source.clone(),
        mirror.clone(),
        registrar.clone(),
        cfg(Duration::from_mins(5)),
        metrics.clone(),
    );

    let declaration = resolver
        .resolve(&id)
        .await
        .expect("a mirrored declaration must be restored and served");

    assert_eq!(declaration.gts_type_id, id);
    assert_eq!(
        metrics.restored.load(Ordering::SeqCst),
        1,
        "the resolution must count result = restored"
    );
    assert_eq!(
        metrics.cache_miss.load(Ordering::SeqCst),
        0,
        "a restore is not a cache_miss"
    );
    assert_eq!(metrics.unresolved.load(Ordering::SeqCst), 0);
    assert_eq!(mirror.reads(), 1, "the row must be read exactly once");
    assert_eq!(
        registrar.documents(),
        vec![schema.raw_schema.clone()],
        "the restore must replay the mirrored document verbatim"
    );
    assert_eq!(
        source.calls(),
        2,
        "the restore re-fetches after registering: a locally rebuilt schema \
         cannot carry the inheritance chain effective_traits walks (J12)"
    );
}

#[tokio::test]
async fn a_successful_restore_writes_nothing_to_the_mirror() {
    let id = meter_id();
    let schema = schema("bytes");
    let mirror = FakeMirror::seeded(&id, schema.raw_schema.clone());
    let source = FakeSource::new(vec![
        Err(DomainError::declaration_not_found(&id)),
        Ok(schema.clone()),
    ]);
    let resolver = resolver_with(
        source,
        mirror.clone(),
        FakeRegistrar::arc(),
        cfg(Duration::from_mins(5)),
        RecordingMetrics::arc(),
    );

    resolver.resolve(&id).await.expect("the restore serves");

    // The positive half: the row was READ, so the absence below is about the
    // write and not about the mirror never being touched at all.
    assert_eq!(mirror.reads(), 1, "the restore read the row");
    assert_eq!(
        mirror.writes(),
        0,
        "a restore is not a registry read: ADR statement 2 rewrites the row \
         'on every successful registry read', and last_seen_at must not report \
         a replay as a fresh confirmation (J7)"
    );
}

#[tokio::test]
async fn a_failed_registration_rejects_the_operation_fail_closed() {
    let id = meter_id();
    let schema = schema("bytes");
    let mirror = FakeMirror::seeded(&id, schema.raw_schema.clone());
    let registrar = FakeRegistrar::failing();
    let source = FakeSource::new(vec![Err(DomainError::declaration_not_found(&id))]);
    let metrics = RecordingMetrics::arc();
    let resolver = resolver_with(
        source,
        mirror,
        registrar.clone(),
        cfg(Duration::from_mins(5)),
        metrics.clone(),
    );

    resolver
        .resolve(&id)
        .await
        .expect_err("a failed restore must reject fail-closed");

    assert_eq!(registrar.calls(), 1, "the registration was attempted");
    assert_eq!(
        metrics.restored.load(Ordering::SeqCst),
        0,
        "a failed restore must not count as restored"
    );
    assert_eq!(
        metrics.unresolved.load(Ordering::SeqCst),
        1,
        "it counts unresolved, per algo-resolve-declaration step 7"
    );
}

/// Fix round 1, Important: a non-not-found failure on the restore's
/// RE-FETCH (registration succeeded, but `types-registry` then fails
/// transiently) must reach the caller as that same transient error, not get
/// reported as a definite not-found. `rehydrate`'s own `# Errors` section
/// documents this distinction; before this test nothing held code to it.
///
/// A real consequence, not a bookkeeping nicety: with the distinction lost,
/// a transient outage mid-restore would reach the ingesting client as a
/// definite "your meter does not exist" instead of a retryable
/// unavailability -- on a charging path.
#[tokio::test]
async fn a_non_not_found_re_fetch_failure_during_restore_is_not_reported_as_not_found() {
    let id = meter_id();
    let schema = schema("bytes");
    let mirror = FakeMirror::seeded(&id, schema.raw_schema.clone());
    let registrar = FakeRegistrar::arc();
    // Not-found first (triggers the restore), then the re-fetch answers a
    // TRANSIENT failure rather than a second not-found -- the registration
    // itself (scripted to succeed via the default `FakeRegistrar`) already
    // completed by the time this one is consulted.
    let source = FakeSource::new(vec![
        Err(DomainError::declaration_not_found(&id)),
        Err(DomainError::TypesRegistryUnavailable(
            "down mid-restore".to_owned(),
        )),
    ]);
    let metrics = RecordingMetrics::arc();
    let resolver = resolver_with(
        source,
        mirror,
        registrar.clone(),
        cfg(Duration::from_mins(5)),
        metrics.clone(),
    );

    let err = resolver
        .resolve(&id)
        .await
        .expect_err("a transient re-fetch failure must still reject");

    assert_eq!(
        registrar.calls(),
        1,
        "the registration was attempted and succeeded"
    );
    assert!(
        !err.is_declaration_not_found(),
        "a transient registry outage mid-restore must surface as that \
         failure, not as a definite not-found: {err:?}"
    );
    // Ruling K25 (entry 46): the re-fetch's own message still propagates
    // verbatim, but now with the identifier it was restoring wrapped ahead
    // of it, not the registry's message alone.
    assert!(
        matches!(err, DomainError::TypesRegistryUnavailable(ref msg)
            if msg == &format!("{id}: down mid-restore")),
        "the re-fetch's own error must propagate verbatim (plus the wrapped \
         identifier), not the original not-found: {err:?}"
    );
    assert!(
        format!("{err}").contains(id.as_str()),
        "a fail-closed rejection must name the identifier it refused; got: {err}"
    );
}

#[tokio::test]
async fn a_definite_not_found_with_no_mirror_row_is_rejected_fail_closed() {
    let id = meter_id();
    let mirror = FakeMirror::arc();
    let registrar = FakeRegistrar::arc();
    let source = FakeSource::new(vec![Err(DomainError::declaration_not_found(&id))]);
    let metrics = RecordingMetrics::arc();
    let resolver = resolver_with(
        source,
        mirror.clone(),
        registrar.clone(),
        cfg(Duration::from_mins(5)),
        metrics.clone(),
    );

    resolver.resolve(&id).await.expect_err("fails closed");

    assert_eq!(mirror.reads(), 1, "the row was looked for");
    assert_eq!(
        registrar.calls(),
        0,
        "with no row there is nothing to register back"
    );
    assert_eq!(metrics.unresolved.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_declaration_whose_mirror_write_failed_is_not_restored_later() {
    let id = meter_id();
    let schema = schema("bytes");
    let mirror = FakeMirror::failing_writes();
    let metrics = RecordingMetrics::arc();
    // Resolve once (the write fails), then the registry loses it.
    let source = FakeSource::new(vec![
        Ok(schema.clone()),
        Err(DomainError::declaration_not_found(&id)),
    ]);
    let resolver = resolver_with(
        source,
        mirror.clone(),
        FakeRegistrar::arc(),
        // A zero TTL so the second reference takes the cold path without a
        // sleep: `fetched_at.elapsed() < ttl` is false immediately.
        TypeResolverConfig {
            ttl: Duration::ZERO,
            capacity: 10,
        },
        metrics.clone(),
    );

    resolver
        .resolve(&id)
        .await
        .expect("the first call is served");
    assert_eq!(metrics.mirror_write_failures.load(Ordering::SeqCst), 1);

    resolver
        .resolve(&id)
        .await
        .expect_err("with no row, the lost declaration is not restorable");
    assert_eq!(
        metrics.restored.load(Ordering::SeqCst),
        0,
        "nothing to restore: the mirror write failed, so the row never existed"
    );
}

#[tokio::test]
async fn a_registry_error_with_a_cold_cache_fails_closed_and_does_not_read_the_row() {
    let id = meter_id();
    // Fix round 1, Important: a SECOND meter, resolved successfully first,
    // so the positive half of this test's absence assertions lives IN this
    // test rather than relying on a sibling. The gap was demonstrated
    // directly: a `resolver_with` that silently discards its `mirror`
    // argument entirely still passes this test's old form -- `mirror.reads()
    // == 0` cannot tell "correctly refused on an error answer" apart from
    // "the mirror was never wired to anything" without a prior positive
    // interaction to rule the second case out.
    let other = meter_id_b();
    let schema = schema("bytes");
    let mirror = FakeMirror::seeded(&id, schema.raw_schema.clone());
    let registrar = FakeRegistrar::arc();
    let source = FakeSource::new(vec![
        Ok(schema_for(&other)),
        Err(DomainError::TypesRegistryUnavailable("down".to_owned())),
    ]);
    let metrics = RecordingMetrics::arc();
    let resolver = resolver_with(
        source,
        mirror.clone(),
        registrar.clone(),
        cfg(Duration::from_mins(5)),
        metrics.clone(),
    );

    // The positive half: resolving a DIFFERENT meter successfully must
    // write the SAME mirror this test's absence assertions are about,
    // proving it is wired to this resolver before the error-path absence
    // below is ever asserted.
    resolver
        .resolve(&other)
        .await
        .expect("the positive half: a working resolution uses the mirror");
    assert_eq!(
        mirror.writes(),
        1,
        "the prior resolve of a different meter demonstrably used this mirror"
    );

    resolver
        .resolve(&id)
        .await
        .expect_err("an error answer with a cold cache fails closed");

    assert_eq!(
        metrics.registry_error.load(Ordering::SeqCst),
        1,
        "it counts registry_error, not unresolved"
    );
    // The absences, now proven against a mirror the test itself already
    // showed is live, not merely unwired.
    assert_eq!(
        mirror.reads(),
        0,
        "ADR statement 3: the restore needs a definite not-found. An error \
         answer cannot tell loss from unavailability, so the row is not served"
    );
    assert_eq!(registrar.calls(), 0, "and nothing is registered back");
}

#[tokio::test]
async fn a_cache_hit_makes_no_registry_call_no_mirror_read_and_no_mirror_write() {
    let id = meter_id();
    let source = FakeSource::new(vec![Ok(schema("bytes"))]);
    let mirror = FakeMirror::arc();
    let registrar = FakeRegistrar::arc();
    let metrics = RecordingMetrics::arc();
    let resolver = resolver_with(
        source.clone(),
        mirror.clone(),
        registrar.clone(),
        cfg(Duration::from_mins(5)),
        metrics.clone(),
    );

    // The POSITIVE half, in this test rather than a sibling: the first
    // reference must move all three counters. Without it, a fake that was
    // never wired to the resolver at all would satisfy the zeros below —
    // which is the shape ruling I51 names, an absence assertion that cannot
    // distinguish correct behaviour from a defect.
    resolver
        .resolve(&id)
        .await
        .expect("first reference resolves");
    assert_eq!(source.calls(), 1, "the first reference reads the registry");
    assert_eq!(mirror.writes(), 1, "and writes the mirror");

    resolver
        .resolve(&id)
        .await
        .expect("second reference resolves");

    assert_eq!(
        metrics.cache_hit.load(Ordering::SeqCst),
        1,
        "the second reference is a cache hit"
    );
    assert_eq!(source.calls(), 1, "no second registry call");
    assert_eq!(mirror.reads(), 0, "no mirror read on the warm path");
    assert_eq!(mirror.writes(), 1, "no second mirror write");
    assert_eq!(registrar.calls(), 0);
}

#[tokio::test]
async fn a_refreshed_row_restores_the_amended_declaration() {
    let id = meter_id();
    // Fix round 1: retention now varies WITH the unit, via the licensed
    // `schema_with_retention` helper, rather than both fixtures sharing
    // `schema`'s hardcoded "P125D" -- see that helper's own doc.
    let before = schema_with_retention("bytes", "P125D");
    let after = schema_with_retention("gibibytes", "P400D");
    let mirror = FakeMirror::arc();
    let registrar = FakeRegistrar::arc();
    let metrics = RecordingMetrics::arc();
    // Cold miss (mirrors `before`), refresh past the TTL (rewrites the row
    // with `after`), then the registry loses it and the restore replays.
    let source = FakeSource::new(vec![
        Ok(before.clone()),
        Ok(after.clone()),
        Err(DomainError::declaration_not_found(&id)),
        Ok(after.clone()),
    ]);
    let resolver = resolver_with(
        source,
        mirror.clone(),
        registrar.clone(),
        TypeResolverConfig {
            ttl: Duration::ZERO,
            capacity: 10,
        },
        metrics.clone(),
    );

    resolver.resolve(&id).await.expect("cold miss");
    resolver.resolve(&id).await.expect("refresh");
    assert_eq!(
        mirror.row(&id),
        Some(after.raw_schema.clone()),
        "ADR statement 2: the row is rewritten on every successful registry \
         read, refresh included, so it tracks the registry"
    );

    let restored = resolver.resolve(&id).await.expect("restore");
    assert_eq!(
        restored.canonical_unit, "gibibytes",
        "the restore must carry the AMENDED declaration, not the first one"
    );
    assert_eq!(metrics.restored.load(Ordering::SeqCst), 1);

    // J19 fact 3, closed per fix round 1 and corrected per fix round 2
    // (the original comment here overstated what these two assertions add).
    // `ResolvedDeclaration` exposes no retention by design (`declaration.rs`:
    // "retention is deliberately absent"), so the document is the only place
    // the ADR's "A restore reinstates the retention of the last successful
    // resolution" is observable at all. Varying retention between `before`
    // ("P125D") and `after` ("P400D") is what makes the whole-document
    // assertion ABOVE (`mirror.row(&id) == Some(after.raw_schema.clone())`)
    // retention-discriminating in the first place: before that fixture
    // change, `before` and `after` agreed on retention, so that same
    // comparison would have passed even if retention had leaked from the
    // wrong fixture into the served document. The two assertions below do
    // NOT add a stronger check of their own -- a restore writes nothing to
    // the mirror, so `mirror.row(&id)` has not changed since the
    // assertion above already read it -- they name the retention value
    // directly, for a reader who would otherwise have to decode it out of a
    // `raw_schema` blob.
    assert_eq!(
        mirror
            .row(&id)
            .and_then(|doc| doc["x-gts-traits"]["retention"].as_str().map(str::to_owned)),
        Some("P400D".to_owned()),
        "the mirrored document must carry the AMENDED retention (P400D), \
         not the pre-amendment P125D"
    );
    assert_eq!(
        registrar.documents(),
        vec![after.raw_schema.clone()],
        "the restore must replay the AMENDED document verbatim, retention included"
    );
}

#[tokio::test]
async fn a_row_amended_after_eviction_restores_the_pre_amendment_declaration() {
    let id = meter_id();
    // Fix round 1: a distinct retention value from the sibling test's pair
    // (P90D, not P125D or P400D) via the licensed `schema_with_retention`
    // helper, so this assertion cannot be satisfied by a value this file
    // happens to hardcode everywhere else.
    let before = schema_with_retention("bytes", "P90D");
    let mirror = FakeMirror::arc();
    let registrar = FakeRegistrar::arc();
    let metrics = RecordingMetrics::arc();
    // Cold miss mirrors `before`. The meter then falls out of the cache and
    // the amendment happens where this gear cannot see it, so the row still
    // holds `before` when the registry loses the declaration.
    let source = FakeSource::new(vec![
        Ok(before.clone()),
        Err(DomainError::declaration_not_found(&id)),
        Ok(before.clone()),
    ]);
    let resolver = resolver_with(
        source,
        mirror.clone(),
        registrar.clone(),
        TypeResolverConfig {
            ttl: Duration::ZERO,
            capacity: 10,
        },
        metrics.clone(),
    );

    resolver.resolve(&id).await.expect("cold miss");
    let restored = resolver.resolve(&id).await.expect("restore");

    assert_eq!(
        restored.canonical_unit, "bytes",
        "ADR Consequences: 'A restore reinstates the retention of the last \
         successful resolution' -- a stale row puts back what it holds, and the \
         `restored` result is the operator's signal to revalidate"
    );
    assert_eq!(metrics.restored.load(Ordering::SeqCst), 1);

    // Fix round 1, closing J19 fact 3; comment corrected per fix round 2
    // (see the sibling test's identical correction). The whole-document
    // assertion right below is the one doing the real work here, and it is
    // `before.raw_schema` specifically -- a value distinct from the sibling
    // test's pair (P90D, not P125D or P400D) -- that keeps the comparison
    // from being satisfiable by a retention constant this file happens to
    // reuse everywhere. The retention-only assertion that follows adds no
    // independent coverage of its own: it reads the identical first
    // document the line above already compared in full; it names the value
    // directly rather than leaving a reader to decode it out of the
    // `raw_schema` the equality check already pins.
    assert_eq!(
        registrar.documents(),
        vec![before.raw_schema.clone()],
        "the restore must replay the pre-amendment document the mirror held, verbatim"
    );
    assert_eq!(
        registrar
            .documents()
            .first()
            .and_then(|doc| doc["x-gts-traits"]["retention"].as_str().map(str::to_owned)),
        Some("P90D".to_owned()),
        "the replayed document must still carry the declared retention"
    );
}

#[tokio::test]
async fn a_restore_observes_no_duration_sample_while_a_miss_does() {
    // Named `seed_schema`, not `schema`: the brief's draft shadowed the
    // `schema(unit)` fixture function with a same-named local binding, which
    // broke the later `schema("bytes")` call below (`E0618`, "expected
    // function, found `GtsTypeSchema`").
    let id = meter_id();
    let seed_schema = schema("bytes");
    let mirror = FakeMirror::seeded(&id, seed_schema.raw_schema.clone());
    let source = FakeSource::new(vec![
        Err(DomainError::declaration_not_found(&id)),
        Ok(seed_schema.clone()),
    ]);
    let metrics = RecordingMetrics::arc();
    let resolver = resolver_with(
        source,
        mirror,
        FakeRegistrar::arc(),
        cfg(Duration::from_mins(5)),
        metrics.clone(),
    );

    resolver.resolve(&id).await.expect("the restore serves");

    let observed: Vec<TypeResolutionOutcome> = metrics
        .duration_observations
        .lock()
        .expect("lock")
        .iter()
        .map(|(outcome, _)| *outcome)
        .collect();
    assert!(
        !observed.contains(&TypeResolutionOutcome::Restored),
        "uc_type_resolution_duration_seconds declares result as (cache_hit, \
         cache_miss) only — DESIGN §3.11.5 and metrics_inventory.rs agree — so \
         a Restored sample would ship an undeclared label value. Observed: {observed:?}"
    );

    // The positive half, so the absence above has a subject: the SAME
    // histogram does receive a sample on a path that declares one.
    let miss_resolver = resolver_with(
        FakeSource::new(vec![Ok(schema("bytes"))]),
        FakeMirror::arc(),
        FakeRegistrar::arc(),
        cfg(Duration::from_mins(5)),
        metrics.clone(),
    );
    miss_resolver
        .resolve(&meter_id_b())
        .await
        .expect("the miss serves");
    let after: Vec<TypeResolutionOutcome> = metrics
        .duration_observations
        .lock()
        .expect("lock")
        .iter()
        .map(|(outcome, _)| *outcome)
        .collect();
    assert!(
        after.contains(&TypeResolutionOutcome::CacheMiss),
        "a cache_miss DOES observe the histogram, so the assertion above is \
         about the restore and not about an unwired recorder: {after:?}"
    );
}

/// Fix round 1, Important: renamed from
/// `a_mirror_row_that_does_not_parse_rejects_fail_closed_without_panicking`,
/// which named a mechanism that does not exist. Review Focus item 2's input
/// class ("a row whose document will not parse") has **no production step
/// to break**: `read_restorable_document` runs `document_names` on the
/// mirrored document and hands it to the registrar VERBATIM — the row's
/// document is never parsed into a declaration at all on the restore path.
/// Its only two fates are "names the right meter" (handled by
/// `document_names`, covered by the sibling different-meter test) or
/// "does not"; parseability never enters it. Both directions were measured
/// directly: seeding a row that WOULD parse while keeping an
/// unparseable re-fetch still reds; keeping this row while making the
/// re-fetch succeed makes the test pass. So the real subject is always the
/// **restored declaration failing to parse after the re-fetch** — the
/// ordinary `ResolvedDeclaration::from_schema` call every cache miss also
/// goes through, now reached via the restore path instead.
///
/// Seeded with [`bare_schema_without_traits`]'s own raw body so the row
/// itself carries this meter's own `$id` (`document_names` accepts it and
/// registration is attempted) but no `x-gts-traits`; the re-fetch then
/// answers that same traitless body, which fails to parse for the ordinary
/// reason (`ResolvedDeclaration::from_schema` cannot find
/// `aggregation_fold`). The `registrar.calls()` assertion below pins that
/// registration WAS attempted — i.e. that this exercises the re-fetch's
/// parse failure, not the name guard.
#[tokio::test]
async fn a_restore_whose_re_fetched_schema_fails_to_parse_rejects_fail_closed_without_panicking() {
    let id = meter_id();
    let mirror = FakeMirror::seeded(&id, bare_schema_without_traits().raw_schema);
    let registrar = FakeRegistrar::arc();
    let source = FakeSource::new(vec![
        Err(DomainError::declaration_not_found(&id)),
        // The re-fetch answers the same useless document the restore just
        // registered.
        Ok(bare_schema_without_traits()),
    ]);
    let metrics = RecordingMetrics::arc();
    let resolver = resolver_with(
        source,
        mirror,
        registrar.clone(),
        cfg(Duration::from_mins(5)),
        metrics.clone(),
    );

    let err = resolver
        .resolve(&id)
        .await
        .expect_err("an unusable restored declaration must fail closed");
    assert!(
        err.is_declaration_not_found(),
        "an incomplete declaration is as unresolvable as an absent one: {err:?}"
    );
    assert_eq!(
        metrics.restored.load(Ordering::SeqCst),
        0,
        "a declaration that will not parse is not a restore"
    );
    assert_eq!(
        registrar.calls(),
        1,
        "document_names must have passed first -- the seeded document DOES \
         name this meter -- so the parse failure, not the name guard, is \
         what this test exercises"
    );
}

/// Designed beyond the brief's list: the counterpart ruling J6 pins — a
/// mirror READ failure during a restore must fail closed exactly like "no
/// row", and must NOT be counted on `uc_declaration_mirror_write_failures_total`,
/// whose published meaning is "resolved but not mirrored", not "could not be
/// read back". Also the only call site that exercises
/// `FakeMirror::failing_reads`, which the brief's own fixture declares but
/// never drives.
#[tokio::test]
async fn a_failed_mirror_read_during_restore_is_treated_as_no_row_and_is_not_a_write_failure() {
    let id = meter_id();
    let mirror = FakeMirror::failing_reads();
    let registrar = FakeRegistrar::arc();
    let source = FakeSource::new(vec![Err(DomainError::declaration_not_found(&id))]);
    let metrics = RecordingMetrics::arc();
    let resolver = resolver_with(
        source,
        mirror.clone(),
        registrar.clone(),
        cfg(Duration::from_mins(5)),
        metrics.clone(),
    );

    resolver
        .resolve(&id)
        .await
        .expect_err("an unreadable mirror row must fail closed, same as no row");

    assert_eq!(mirror.reads(), 1, "the read was attempted");
    assert_eq!(
        registrar.calls(),
        0,
        "nothing is registered when the row cannot even be read"
    );
    assert_eq!(
        metrics.mirror_write_failures.load(Ordering::SeqCst),
        0,
        "a read failure is NOT a write failure (ruling J6): the row may be \
         intact and the reader broken"
    );
    assert_eq!(metrics.unresolved.load(Ordering::SeqCst), 1);
}

/// Routed finding: `NoopDeclarationMirror`'s behaviour was pinned by
/// nothing — inverting both its methods left the whole suite green. It is
/// `TypeResolver::new`'s default, so this is where that
/// ruling's reasoning starts being load-bearing: a failing default would
/// turn every pre-8b `TypeResolver::new` call site into a silent
/// mirror-write-failure emitter.
#[tokio::test]
async fn noop_declaration_mirror_writes_nowhere_and_reads_nothing() {
    let mirror = NoopDeclarationMirror;

    mirror
        .upsert(
            &meter_id(),
            Uuid::from_u128(1),
            &json!({ "irrelevant": true }),
        )
        .await
        .expect("a no-op write always succeeds");
    assert_eq!(
        mirror
            .read(&meter_id())
            .await
            .expect("a no-op read never fails"),
        None,
        "a no-op mirror holds no row, by construction -- this is the \
         fail-closed answer a restore against TypeResolver::new's default \
         correctly gets"
    );
}

/// Review Focus item 3: a row whose document names a different meter.
///
/// Nothing in the documents forbids such a row, and replaying it would
/// re-register ANOTHER declaration — the one thing ADR statement 1 bounds
/// this write to prevent.
#[tokio::test]
async fn a_mirror_row_whose_document_names_another_meter_is_not_restored() {
    let id = meter_id();
    let other = meter_id_b();
    let mirror = FakeMirror::seeded(&id, schema_for(&other).raw_schema);
    let registrar = FakeRegistrar::arc();
    let source = FakeSource::new(vec![Err(DomainError::declaration_not_found(&id))]);
    let metrics = RecordingMetrics::arc();
    let resolver = resolver_with(
        source,
        mirror.clone(),
        registrar.clone(),
        cfg(Duration::from_mins(5)),
        metrics.clone(),
    );

    resolver
        .resolve(&id)
        .await
        .expect_err("a row naming another meter must not be restored");

    assert_eq!(mirror.reads(), 1, "the row was read");
    assert_eq!(
        registrar.calls(),
        0,
        "and refused before any registration: replaying it would re-register \
         a declaration for {other} under a resolution of {id}"
    );
    assert_eq!(metrics.unresolved.load(Ordering::SeqCst), 1);
}

/// Review Focus item 4: N concurrent cold resolutions of one lost meter must
/// produce ONE registration, not N.
///
/// The restore sits inside `populate`, behind the existing per-key
/// single-flight gate, so the collapse is inherited rather than new — which
/// is exactly why it needs a test: nothing else in this slice would notice if
/// the restore were moved outside the gate, and hammering the registry during
/// an outage recovery is the worst possible time to do it.
#[tokio::test]
async fn concurrent_cold_resolutions_of_a_lost_meter_register_once() {
    let id = meter_id();
    let schema = schema("bytes");
    let mirror = FakeMirror::seeded(&id, schema.raw_schema.clone());
    let registrar = FakeRegistrar::arc();
    // A delay widens the window a broken collapse would fall into, the same
    // way `FakeSource::slow` does for the existing single-flight tests.
    let source = FakeSource::slow(
        vec![
            Err(DomainError::declaration_not_found(&id)),
            Ok(schema.clone()),
        ],
        Duration::from_millis(50),
    );
    let metrics = RecordingMetrics::arc();
    let resolver = Arc::new(resolver_with(
        source.clone(),
        mirror.clone(),
        registrar.clone(),
        cfg(Duration::from_mins(5)),
        metrics.clone(),
    ));

    let mut handles = Vec::new();
    for _ in 0..8 {
        let resolver = Arc::clone(&resolver);
        let id = id.clone();
        handles.push(tokio::spawn(async move { resolver.resolve(&id).await }));
    }
    for handle in handles {
        handle
            .await
            .expect("task joins")
            .expect("each caller is served");
    }

    assert_eq!(
        registrar.calls(),
        1,
        "eight concurrent resolutions of one lost meter must make ONE \
         registration: the restore runs inside populate, behind the per-key gate"
    );
    // Fix round 1, Important: without this, the test reds only through
    // `cache_hit == 7` below -- incidental bookkeeping, not the stated
    // subject ("must produce ONE registration, not N"). Measured directly:
    // defeating the single-flight gate entirely (giving every caller its
    // own private `Mutex` instead of the shared, per-key one `gate_for`
    // hands out) leaves `registrar.calls()` and `mirror.reads()` BOTH still
    // at 1 -- the 7 extra callers each race the FakeSource directly and win
    // its *second* scripted outcome (a plain success), never reaching
    // restore mode at all, since only the caller unlucky enough to be
    // first ever sees the single scripted not-found. `source.calls()` rises
    // to 9 under that mutation (one not-found plus eight successes) and
    // `cache_hit` drops to 0 (no caller ever finds a fresh entry on its
    // second check, since none is ever queued behind another). A real
    // collapse failure need not take this exact shape, but every shape
    // that lets more than one caller reach `populate` moves `source.calls()`
    // off 2, which is this assertion's subject and why it is placed here,
    // before `cache_hit`, rather than after it.
    assert_eq!(
        source.calls(),
        2,
        "one call for the initial not-found, one for the restore's \
         re-fetch, regardless of how many of the eight callers reach the \
         gate -- hammering the registry during an outage recovery is the \
         worst possible time for this to grow"
    );
    assert_eq!(mirror.reads(), 1, "and read the row once");
    assert_eq!(
        metrics.restored.load(Ordering::SeqCst),
        1,
        "one restore; the other seven are post-gate cache hits"
    );
    assert_eq!(
        metrics.cache_hit.load(Ordering::SeqCst),
        7,
        "the other seven collapse onto the post-gate cache-hit site"
    );
}

/// Review Focus item 5: a restore caches through `store`, so it must evict
/// and republish the gauges exactly as a cold miss does.
///
/// **One line dropped from the brief's draft**: an unused `let schema =
/// schema("bytes");` binding this test never reads (every fixture it
/// actually resolves comes from `schema_for`) — `cargo clippy --all-targets`
/// would flag it under this workspace's `unused_mut`/default `unused_variables`
/// lints.
///
/// **Slice 8b final fix round, item 5.2: the republication half of this
/// test's own name is now pinned, rather than the name being narrowed to
/// eviction.** The previous form could not see it. `resolved_types` already
/// read `Some(2)` after the SECOND resolve, before the restore ran at all,
/// and the age-gauge assertion was a bare `is_some()` whose own comment
/// conceded as much. Both are satisfied by values left over from an earlier
/// resolve, so a restore that inserted straight into `entries` and never
/// called `store` would have reded neither.
///
/// What makes republication observable is making the gauge's correct value
/// **change** across the restore, which needs two things: paused virtual
/// time (so the ages are exact integers rather than a race with the clock)
/// and the eviction this test already forces. With `first` resolved at t=0,
/// `second` at t=5 and the restore at t=12: the second resolve publishes
/// age = 5 (oldest is `first`, resolved 5s earlier), and the restore evicts
/// `first`, leaving `second` as the oldest and publishing age = 7. A restore
/// that bypassed `store` would leave the gauge reading 5.
#[tokio::test(start_paused = true)]
async fn a_restore_at_capacity_evicts_and_republishes_the_cache_gauges() {
    let first = meter_id();
    let second = meter_id_b();
    let restored_id = meter_id_c();
    let mirror = FakeMirror::seeded(&restored_id, schema_for(&restored_id).raw_schema);
    let metrics = RecordingMetrics::arc();
    let source = FakeSource::new(vec![
        Ok(schema_for(&first)),
        Ok(schema_for(&second)),
        Err(DomainError::declaration_not_found(&restored_id)),
        Ok(schema_for(&restored_id)),
    ]);
    let resolver = resolver_with(
        source.clone(),
        mirror,
        FakeRegistrar::arc(),
        TypeResolverConfig {
            ttl: Duration::from_mins(5),
            capacity: 2,
        },
        metrics.clone(),
    );

    resolver.resolve(&first).await.expect("first");
    // Paused time is frozen except when explicitly advanced, so without these
    // the three resolutions would land on the IDENTICAL virtual instant,
    // every published age would be 0, and the republication assertions below
    // could not tell a republished gauge from a left-over one. Five seconds
    // then seven keeps both entries inside the 5-minute TTL while ordering
    // them strictly and making the two expected ages (5, then 7) distinct.
    tokio::time::advance(Duration::from_secs(5)).await;
    resolver.resolve(&second).await.expect("second");
    tokio::time::advance(Duration::from_secs(7)).await;

    // The republication half's "before" reading, captured while `first` is
    // still cached and therefore still the oldest entry: the second resolve
    // published `first`'s age, which was 5s at that moment.
    let cache_age_seconds = *metrics.cache_age_seconds.lock().expect("lock");
    assert_eq!(
        cache_age_seconds,
        Some(Some(5)),
        "before the restore the gauge holds what the SECOND resolve published: \
         `first`'s age at that instant (5s)"
    );

    resolver.resolve(&restored_id).await.expect("restore");

    // The republication itself. The restore evicts `first`, so the oldest
    // SURVIVING entry is `second` (resolved 7s ago) and `store`'s
    // `publish_cache_gauges` must publish 7 -- a value that did not exist
    // anywhere before the restore ran, so no left-over reading can satisfy
    // it. A restore that inserted straight into `entries`, bypassing `store`,
    // leaves this at the 5 asserted above.
    let cache_age_seconds = *metrics.cache_age_seconds.lock().expect("lock");
    assert_eq!(
        cache_age_seconds,
        Some(Some(7)),
        "the restore must go through `store`, which republishes the age from \
         the POST-eviction map: `first` is gone, so the oldest survivor is \
         `second` at 7s -- not the 5 the previous publish left behind"
    );

    // Designed beyond the brief's draft: that version's two assertions below
    // pass even against a mutation that inserts the restored entry directly
    // into `entries`, bypassing `store`'s eviction AND its gauge republish
    // entirely -- verified by running that exact mutation, which reds
    // NOTHING, because both gauges already happen to hold the right-looking
    // values from the SECOND resolve (before the restore ever runs) and a
    // direct insert never touches them again. These two assertions close
    // that gap: a direct map-size read (this module is `type_resolver`'s own
    // child, so it can reach the private `entries` field the way
    // `a_stale_serve_does_not_refresh_the_cache_age` already does) and a
    // re-resolution of the evicted `first`, which must reach the source
    // again -- neither can be satisfied by a value left over from an
    // earlier resolve.
    let entries = resolver.entries.read().await;
    assert_eq!(
        entries.len(),
        2,
        "the restore's insert must still leave the map itself (not just the \
         gauge) bounded at capacity"
    );
    drop(entries);
    assert_eq!(
        source.calls(),
        4,
        "sanity: exactly the four calls the fixture scripted so far"
    );
    resolver
        .resolve(&first)
        .await
        .expect("first, evicted, must be refetchable");
    assert_eq!(
        source.calls(),
        5,
        "`first` was evicted when the restore was stored, so resolving it \
         again must reach the source rather than serve a stale survivor"
    );

    let resolved_types = *metrics.resolved_types.lock().expect("lock");
    assert_eq!(
        resolved_types,
        Some(2),
        "the restore must go through `store`, so capacity still bounds the map \
         at 2 rather than growing to 3"
    );
    // Fix round 1, Minor recorded a bare `is_some()` here whose message had
    // to be walked back to "the gauge remains set", because the prior two
    // resolves had already set it and `is_some()` could not see the restore
    // at all. Slice 8b's final fix round (item 5.2) replaced it with the two
    // age-value assertions ABOVE, which bracket the restore and require the
    // published value to MOVE (5 -> 7) across it. This assertion is not
    // repeated here because the republication is already pinned where it can
    // be seen -- on either side of the call that performs it.
    assert_eq!(metrics.restored.load(Ordering::SeqCst), 1);
}

/// Slice 8b final fix round, item 1.2. Spec §5.4 row 3 states the
/// obligation — *"unchanged — and no mirror write: nothing was resolved, so
/// §3.11.5's 'a declaration resolved but not mirrored' has no subject"* —
/// and nothing asserted it. The only test driving `populate`'s parse-failure
/// arm (`an_incomplete_declaration_fails_closed_and_is_not_cached_as_success`)
/// builds through `TypeResolver::new`, whose defaulted
/// `NoopDeclarationMirror` counts no writes at all, so hoisting the
/// `rehydrate(Mirror)` call above `populate`'s `from_schema` match left the
/// whole `--lib` lane green.
///
/// The positive half is in this same test and goes through the SAME mirror: a
/// resolvable meter writes exactly once first, so the unchanged count below
/// is about the parse failure rather than about a mirror that was never wired.
#[tokio::test]
async fn an_incomplete_declaration_performs_no_mirror_write() {
    let id = meter_id();
    let other = meter_id_b();
    let mirror = FakeMirror::arc();
    let metrics = RecordingMetrics::arc();
    // A resolvable schema for `other` first (the positive half), then a
    // schema that fetches fine but declares no `x-gts-traits` at all, so
    // `ResolvedDeclaration::from_schema` fails and `populate` takes its
    // parse-failure arm rather than its success arm.
    let source = FakeSource::new(vec![
        Ok(schema_for(&other)),
        Ok(bare_schema_without_traits()),
    ]);
    let resolver = resolver_with(
        source,
        mirror.clone(),
        FakeRegistrar::arc(),
        cfg(Duration::from_mins(5)),
        metrics.clone(),
    );

    resolver
        .resolve(&other)
        .await
        .expect("the positive half: a resolvable meter resolves");
    assert_eq!(
        mirror.writes(),
        1,
        "the positive half: a successful resolution writes THIS mirror exactly once"
    );

    resolver
        .resolve(&id)
        .await
        .expect_err("an incomplete declaration must fail closed");

    assert_eq!(
        metrics.unresolved.load(Ordering::SeqCst),
        1,
        "the parse failure counts Unresolved"
    );
    assert_eq!(
        mirror.writes(),
        1,
        "spec \u{a7}5.4 row 3: an incomplete declaration performs NO mirror write. \
         `uc_declaration_mirror_write_failures_total` publishes 'a declaration \
         resolved but not mirrored', and nothing was resolved here, so that \
         instrument has no subject -- the count must still be the ONE the \
         positive half above made"
    );
    assert_eq!(
        mirror.row(&id),
        None,
        "and no row exists for the meter that never resolved"
    );
}

/// Slice 8b final fix round, item 1.3. Spec §5.4 row 5 and §6 criterion 7
/// (*"the row is not served"*) were pinned only for the **cold-cache** half,
/// by `a_registry_error_with_a_cold_cache_fails_closed_and_does_not_read_the_row`
/// — where there is nothing cached to serve in the first place. No
/// `resolver_with`-based test reached `ServedStale` at all (both driving
/// tests build through `TypeResolver::new`, whose mirror counts nothing), so
/// adding a `self.mirror.read(id)` to `populate`'s `ServedStale` arm left the
/// lane green.
///
/// Why the absence matters rather than being bookkeeping: a registry *error*
/// is not loss. The registry never said the type is gone, so there is nothing
/// for the mirror to repair, and a row read here would spend a database round
/// trip on the one path whose whole purpose is to keep ingestion off a
/// failing dependency.
///
/// The positive half is in this same test: the cold miss writes the very
/// mirror the absence below is asserted against.
#[tokio::test(start_paused = true)]
async fn a_stale_serve_reads_no_mirror_row() {
    let id = meter_id();
    let mirror = FakeMirror::arc();
    let metrics = RecordingMetrics::arc();
    let source = FakeSource::new(vec![
        Ok(schema("bytes")),
        Err(DomainError::TypesRegistryUnavailable(
            "connect refused".to_owned(),
        )),
    ]);
    let resolver = resolver_with(
        source.clone(),
        mirror.clone(),
        FakeRegistrar::arc(),
        cfg(Duration::from_millis(20)),
        metrics.clone(),
    );

    resolver.resolve(&id).await.expect("the cold miss resolves");
    assert_eq!(
        mirror.writes(),
        1,
        "the positive half: the cold miss demonstrably used THIS mirror"
    );

    // Paused time auto-advances past this sleep (nothing else is runnable),
    // clearing the 20ms TTL instantly rather than via a real delay.
    tokio::time::sleep(Duration::from_millis(40)).await;

    let stale = resolver
        .resolve(&id)
        .await
        .expect("a stale entry must still serve while the registry is down");
    assert_eq!(
        stale.canonical_unit, "bytes",
        "it is the cached declaration that is served"
    );
    assert_eq!(
        metrics.served_stale.load(Ordering::SeqCst),
        1,
        "the resolution counts ServedStale, so this test is on the stale-serve arm"
    );
    assert_eq!(
        source.calls(),
        2,
        "the refresh attempt still reached the registry and failed"
    );
    assert_eq!(
        mirror.reads(),
        0,
        "\u{a7}6 criterion 7: the row is NOT served. A registry error cannot tell \
         loss from unavailability, so there is nothing for the mirror to \
         repair -- the cached entry stands in instead"
    );
    assert_eq!(
        mirror.writes(),
        1,
        "and a stale serve is not a successful registry read, so ADR statement \
         2's rewrite does not apply either: still the ONE write from the cold miss"
    );
}

/// Slice 8b final fix round, item 1.6: the single-flight gate's **per-key**
/// scope, which this slice newly made load-bearing and which nothing pinned.
///
/// `mirror_document`'s own doc claims a stalled mirror write costs *"every
/// OTHER concurrent caller queued behind it **on the same key**"* — a claim
/// about the gate's scope. Every other concurrency test in this file drives
/// exactly **one** meter, so collapsing `inflight` from
/// `Mutex<HashMap<MeterTypeId, Arc<Mutex<()>>>>` to a single
/// `Mutex<Option<Arc<Mutex<()>>>>` — one gate for all keys — left the whole
/// lane green. Under that collapse a cold burst across N distinct meters
/// serializes into N sequential registry round trips, which is exactly the
/// hot-path coupling on a second gear's latency the cache exists to prevent.
///
/// Measured in **paused virtual** time, not wall clock: each fetch takes 10
/// virtual seconds, so two meters resolved in parallel elapse ~10s and two
/// serialized behind one global gate elapse ~20s. The 15s threshold sits
/// between them with margin on both sides and costs no real time at all.
#[tokio::test(start_paused = true)]
async fn two_meters_resolve_concurrently_rather_than_serializing_on_one_gate() {
    let source = FakeSource::slow(vec![Ok(schema("bytes"))], Duration::from_secs(10));
    let resolver = Arc::new(resolver_with(
        source.clone(),
        FakeMirror::arc(),
        FakeRegistrar::arc(),
        cfg(Duration::from_mins(5)),
        RecordingMetrics::arc(),
    ));

    let started = tokio::time::Instant::now();
    let mut handles = Vec::new();
    for id in [meter_id(), meter_id_b()] {
        let resolver = Arc::clone(&resolver);
        handles.push(tokio::spawn(async move { resolver.resolve(&id).await }));
    }
    for handle in handles {
        handle
            .await
            .expect("task joins")
            .expect("each meter resolves");
    }
    let elapsed = started.elapsed();

    assert_eq!(
        source.calls(),
        2,
        "two DISTINCT meters are two cold keys: each owes its own fetch, and \
         neither may be collapsed into the other's"
    );
    assert!(
        elapsed < Duration::from_secs(15),
        "the single-flight gate is PER KEY: two distinct meters must not \
         serialize behind one gate. Two 10s fetches in parallel elapse ~10s; \
         serialized behind a single global gate they elapse ~20s. Measured: \
         {elapsed:?}"
    );
}

/// Slice 8b final fix round, item 1.5: the one composition no point in the
/// slice made — a **real** document in a **real** table, read back through
/// the real adapter, into the real resolver.
///
/// Measured across five surfaces, every one of which substitutes something:
/// `infra/declaration_mirror/mirror_tests.rs` drives a real migrated table
/// with hand-built documents; every mirror/restore test above drives real
/// documents through a `HashMap`-backed `FakeMirror`;
/// `infra/types_registry_source_tests.rs`'s construction-point tests drive a
/// stub document through a `HashMap`-backed `CountingMirror`; and
/// `module_tests`' provider is never
/// migrated. So a defect living in the seam — the `document` column being
/// free text that has to survive a `serde_json` round trip, `read` keying on
/// `MeterTypeId::as_str`, `document_names` finding `$id` in what came back
/// out of the database rather than in what a fake handed straight over — had
/// no test anywhere in the slice that could see it.
///
/// **A `cfg(test)`-only reach from `domain` into `infra`.** The production
/// direction is one-way (`infra::types_registry_source::build_default_resolvers`
/// builds the resolver; the domain layer never names an adapter), and this
/// test stands in for that composition root, so it necessarily names both
/// halves. The harness is `mirror_tests.rs`'s own
/// (`connect_db("sqlite::memory:")` + `run_migrations_for_testing`); see the
/// comment on its `migrated_sqlite` for why `sqlite::memory:` isolates per
/// `connect_db` call rather than per pool, and why `max_conns: Some(1)` is
/// belt-and-braces rather than the isolation mechanism.
#[tokio::test]
async fn a_real_mirror_table_round_trips_a_real_document_through_a_restore() {
    use toolkit_db::migration_runner::run_migrations_for_testing;
    use toolkit_db::{ConnectOpts, DBProvider, connect_db};

    use crate::infra::declaration_mirror::{DbDeclarationMirror, migrations};

    let id = meter_id();
    let schema = schema("bytes");

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
    run_migrations_for_testing(&db, migrations())
        .await
        .expect("apply the declaration-mirror migration");
    let mirror = Arc::new(DbDeclarationMirror::new(DBProvider::new(db)));

    let registrar = FakeRegistrar::arc();
    let metrics = RecordingMetrics::arc();
    // Cold miss (the resolver mirrors into the real table), then the registry
    // loses the type, then the restore's re-fetch.
    let source = FakeSource::new(vec![
        Ok(schema.clone()),
        Err(DomainError::declaration_not_found(&id)),
        Ok(schema.clone()),
    ]);
    let resolver = resolver_with(
        source,
        mirror,
        registrar.clone(),
        // A zero TTL so the second reference takes the cold path without a
        // sleep, the same way the sibling row-amendment tests do.
        TypeResolverConfig {
            ttl: Duration::ZERO,
            capacity: 10,
        },
        metrics.clone(),
    );

    resolver
        .resolve(&id)
        .await
        .expect("the cold miss resolves and mirrors into the real table");

    let restored = resolver
        .resolve(&id)
        .await
        .expect("a row in the REAL table must restore the lost declaration");

    assert_eq!(
        metrics.restored.load(Ordering::SeqCst),
        1,
        "the resolution must count result = restored, driven by a row that \
         actually went through the migration's `document` text column"
    );
    assert_eq!(
        restored.canonical_unit, "bytes",
        "and the served declaration is the mirrored one, re-parsed after the \
         restore's re-fetch"
    );
    assert_eq!(
        registrar.documents(),
        vec![schema.raw_schema.clone()],
        "the document the real adapter read back out of the real table must be \
         byte-identical to the one the registry returned -- the text column, \
         its serde_json round trip and `document_names`' `$id` lookup all on \
         the real path, with nothing substituted"
    );
}

/// Designed beyond the brief's list: direct coverage of `document_names`'s
/// `gtsId` / `id` fallback fields. Every resolver-level test above seeds a
/// document carrying `$id` alone, so a mutation narrowing the field list to
/// `["$id"]` only -- dropping the `gtsId`/`id` fallback `types-registry`'s
/// own `entity_id_fields` default also carries -- reds NOTHING through any
/// of them; verified by running that exact mutation. `resolver_tests` can
/// reach `document_names` directly: it is a private item of `type_resolver`,
/// and privacy in Rust extends to descendant modules, the same access this
/// file already uses for `publish_cache_gauges` and `stale_entry`.
///
/// **Fix round 1, Critical.** The `disagreeing` case below is the one that
/// mattered: a prior version of `document_names` used `.any(...)` across the
/// three fields and stripped `gts://` from all three, which agreed with
/// `types-registry`'s own `extract_gts_id` on every single-field document
/// but diverged on a multi-field one. `types-registry` returns on the
/// **first present** field (`$id` before `gtsId` before `id`) and strips the
/// URI prefix **only** for `$id`
/// (`types-registry/src/domain/service.rs:160-174`); the old `.any()`
/// ignored field order entirely. A row carrying `{"$id": "<other meter,
/// gts:// prefixed>", "gtsId": "<this meter, unprefixed>"}` made the old
/// version accept (the `gtsId` field matches `id`) while `types-registry`
/// would register the document under `$id`'s *other* meter — the guard
/// passing exactly the row it exists to catch. Nothing in a JSON body
/// forbids such a row, and the mirror table is durable, so this is reachable
/// in principle even though no production call site builds one today.
#[test]
fn document_names_checks_id_gts_id_and_id_fields_with_the_gts_prefix_stripped() {
    let id = meter_id();
    let other = meter_id_b();

    assert!(
        super::document_names(&json!({ "$id": format!("gts://{id}") }), &id),
        "the production `$id` form, URI-prefixed"
    );
    assert!(
        super::document_names(&json!({ "gtsId": id.as_str() }), &id),
        "the `gtsId` fallback field, unprefixed"
    );
    assert!(
        super::document_names(&json!({ "id": id.as_str() }), &id),
        "the `id` fallback field, unprefixed"
    );
    assert!(
        !super::document_names(&json!({ "$id": format!("gts://{other}") }), &id),
        "a document naming a different meter must not match"
    );
    assert!(
        !super::document_names(&json!({ "unrelated": "field" }), &id),
        "a document with none of the three fields must not match"
    );

    // The disagreeing case (fix round 1, Critical): `$id` names `other`,
    // `gtsId` names `id` itself. `types-registry`'s `extract_gts_id` returns
    // on the first present field (`$id`) and never consults `gtsId` at all,
    // so the registry would register this document under `other` -- this
    // guard must refuse it, not accept on `gtsId`'s match.
    assert!(
        !super::document_names(
            &json!({ "$id": format!("gts://{other}"), "gtsId": id.as_str() }),
            &id
        ),
        "when $id and gtsId disagree, types-registry's extract_gts_id acts on \
         the FIRST present field ($id, naming `other`) and never reaches \
         gtsId -- this guard must agree with that, not accept on gtsId's \
         match to `id`"
    );
    // The mirrored disagreement, one precedence step down: with `$id`
    // absent, `gtsId` is the first field this function's own fixed
    // `["$id", "gtsId", "id"]` priority list reaches (the precedence is
    // that fixed list, not the JSON object's own key order, which
    // `document.as_object()` does not even expose a position for). `gtsId`
    // names `id` itself; `id` disagrees, naming `other`. `types-registry`'s
    // `extract_gts_id` would stop at `gtsId` and never reach `id` either, so
    // this guard must agree and accept, not fall through to `id`'s mismatch.
    assert!(
        super::document_names(&json!({ "gtsId": id.as_str(), "id": other.as_str() }), &id),
        "with $id absent, gtsId decides this on its own and must not fall \
         through to check `id`, which disagrees (naming `other`)"
    );

    // Fix round 2, Critical's second clause: `types-registry`'s
    // `extract_gts_id` strips `gts://` ONLY when the winning field is
    // `$id` -- `gtsId` and `id` are compared raw. A `gtsId` value that
    // itself carries the `gts://` prefix is therefore a LITERAL string
    // that does not equal `id.as_str()` (which never carries the prefix),
    // and this guard must refuse it rather than strip the prefix from
    // every field and accept. Before this assertion, a mutation that
    // stripped `gts://` from all three fields (restoring the field order
    // fix round 1 closed, while silently reopening the stripping-scope
    // half) left all 874 tests green.
    assert!(
        !super::document_names(&json!({ "gtsId": format!("gts://{id}") }), &id),
        "gtsId is compared raw, never gts://-stripped -- a gtsId value that \
         happens to carry the URI prefix literally is not this meter's bare \
         id and must not match"
    );

    // Slice 8b final fix round, item 1.1: the `document.as_object()` early
    // return. Every `Value` above is an OBJECT, so nothing above reaches
    // that branch at all, and mutating its `return false` to `return true`
    // left the whole --lib lane green and clippy clean -- under which a row
    // whose `document` text is valid JSON but not an object would be
    // accepted and handed VERBATIM to `DeclarationRegistrar::register`. Not
    // hypothetical input: `document` is a free-text column
    // (`infra/declaration_mirror/entity.rs`), which is exactly why
    // `mirror_tests.rs::a_row_whose_stored_text_is_not_json_surfaces_as_an_error_not_a_panic`
    // exists -- an older schema or a hand edit can leave a scalar, an array
    // or a bare string there, and a string carrying another meter's URI is
    // the worst of them.
    for non_object in [
        json!(format!("gts://{other}")),
        json!([{ "$id": format!("gts://{id}") }]),
        json!(42),
        json!(null),
    ] {
        assert!(
            !super::document_names(&non_object, &id),
            "a mirrored document that is valid JSON but not an OBJECT names no \
             meter and must never be accepted for restore: {non_object}"
        );
    }
}

// ── Entry 46: every rejection lifting through
// `TypesRegistryUnavailable` must name the meter ──────────────────────────
//
// Three sites in this module raise or propagate
// `DomainError::TypesRegistryUnavailable`, and before ruling K25 none of
// them carried the identifier being resolved -- only the registry's own
// message (`name_registry_failure`'s own doc names all three). One test per
// site: a test over a single path would stay green while the other two
// still discarded the identifier -- the partial-coverage shape this suite
// exists to refuse.

/// Path 1 of 3: the cold-cache registry error -- `populate`'s final `else`,
/// reached when the fetch fails and nothing is cached to fall back on.
#[tokio::test]
async fn cold_cache_registry_error_names_the_meter_it_failed_to_resolve() {
    let source = FakeSource::new(vec![Err(DomainError::TypesRegistryUnavailable(
        "connect refused".to_owned(),
    ))]);
    let resolver = TypeResolver::new(source, cfg(Duration::from_mins(5)), RecordingMetrics::arc());

    let err = resolver
        .resolve(&meter_id())
        .await
        .expect_err("a registry error with nothing cached must not resolve");

    let rendered = format!("{err}");
    assert!(
        rendered.contains(meter_id().as_str()),
        "a fail-closed rejection must name the identifier it refused; got: {rendered}"
    );
}

/// Path 2 of 3: the restore's registration refusal --
/// `TypeResolver::restore_document`'s `self.registrar.register` arm.
#[tokio::test]
async fn restore_registration_refusal_names_the_meter_it_failed_to_restore() {
    let id = meter_id();
    let schema = schema("bytes");
    let mirror = FakeMirror::seeded(&id, schema.raw_schema.clone());
    let registrar = FakeRegistrar::failing();
    let source = FakeSource::new(vec![Err(DomainError::declaration_not_found(&id))]);
    let resolver = resolver_with(
        source,
        mirror,
        registrar.clone(),
        cfg(Duration::from_mins(5)),
        RecordingMetrics::arc(),
    );

    let err = resolver
        .resolve(&id)
        .await
        .expect_err("a refused registration must not resolve");

    assert_eq!(registrar.calls(), 1, "the registration was attempted");
    let rendered = format!("{err}");
    assert!(
        rendered.contains(id.as_str()),
        "a fail-closed rejection must name the identifier it refused; got: {rendered}"
    );
}

/// Path 3 of 3: the restore's post-registration re-fetch failure --
/// `TypeResolver::restore_document`'s `self.source.fetch(id)` re-fetch,
/// reached only once registration itself has already succeeded.
#[tokio::test]
async fn restore_re_fetch_failure_names_the_meter_it_failed_to_restore() {
    let id = meter_id();
    let schema = schema("bytes");
    let mirror = FakeMirror::seeded(&id, schema.raw_schema.clone());
    let registrar = FakeRegistrar::arc();
    let source = FakeSource::new(vec![
        Err(DomainError::declaration_not_found(&id)),
        Err(DomainError::TypesRegistryUnavailable(
            "down mid-restore".to_owned(),
        )),
    ]);
    let resolver = resolver_with(
        source,
        mirror,
        registrar.clone(),
        cfg(Duration::from_mins(5)),
        RecordingMetrics::arc(),
    );

    let err = resolver
        .resolve(&id)
        .await
        .expect_err("a transient re-fetch failure must not resolve");

    assert_eq!(
        registrar.calls(),
        1,
        "the registration succeeded before the re-fetch failed"
    );
    let rendered = format!("{err}");
    assert!(
        rendered.contains(id.as_str()),
        "a fail-closed rejection must name the identifier it refused; got: {rendered}"
    );
}
