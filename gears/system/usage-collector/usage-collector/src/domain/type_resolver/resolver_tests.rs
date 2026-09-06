//! Cache-policy tests against a scripted `DeclarationSource` fake.
//!
//! Cache policy is the whole point of this component (DESIGN §3.2, §3.5), so
//! each behaviour below gets a dedicated test rather than being folded into
//! `declaration_tests` / `metadata_tests`, which exercise the parsing and
//! validation halves this module also holds.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;
use types_registry_sdk::{GtsTypeId, GtsTypeSchema};
use usage_collector_sdk::{AggregationFold, MeterTypeId};

use crate::domain::error::DomainError;
use crate::domain::ports::declarations::DeclarationSource;
use crate::domain::ports::metrics::{TypeResolutionOutcome, UsageCollectorMetrics};

use super::{TypeResolver, TypeResolverConfig};

/// Counts `record_type_resolution` calls per [`TypeResolutionOutcome`], so
/// these cache-policy tests can pin that each of the resolver's real
/// branches reports the outcome DESIGN §3.11.5's `uc_type_resolution_total`
/// expects — not just that `resolve` itself returns the right `Result`.
#[derive(Default)]
struct RecordingMetrics {
    cache_hit: AtomicUsize,
    cache_miss: AtomicUsize,
    served_stale: AtomicUsize,
    unresolved: AtomicUsize,
    registry_error: AtomicUsize,
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
    fn observe_ingestion_duration(&self, _: f64) {}
    fn observe_record_metadata_bytes(&self, _: u64) {}
    fn record_ingestion_record(
        &self,
        _: crate::domain::ports::metrics::RecordOutcome,
        _: crate::domain::ports::metrics::RecordKind,
        _: crate::domain::ports::metrics::RecordErrorCategory,
    ) {
    }
    fn record_ingestion_request(
        &self,
        _: crate::domain::ports::metrics::IngestRequestOutcome,
        _: crate::domain::ports::metrics::IngestRequestErrorCategory,
    ) {
    }
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
    fn record_deactivation_request(
        &self,
        _: crate::domain::ports::metrics::RequestOutcome,
        _: crate::domain::ports::metrics::DeactivationErrorCategory,
        _: f64,
    ) {
    }
    fn record_type_resolution(&self, outcome: TypeResolutionOutcome) {
        let counter = match outcome {
            TypeResolutionOutcome::CacheHit => &self.cache_hit,
            TypeResolutionOutcome::CacheMiss => &self.cache_miss,
            TypeResolutionOutcome::ServedStale => &self.served_stale,
            TypeResolutionOutcome::Unresolved => &self.unresolved,
            TypeResolutionOutcome::RegistryError => &self.registry_error,
        };
        counter.fetch_add(1, Ordering::SeqCst);
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

/// Builds a base + derived schema pair declaring `unit` as the canonical
/// unit, the way `declaration_tests::schema_with_traits` does.
///
/// `x-gts-traits` is placed at the **top level** of the derived schema's raw
/// JSON, not nested inside an `allOf` branch: `GtsTypeSchema::extract_traits`
/// (which `effective_traits()` reads) only ever reads
/// `schema.get("x-gts-traits")` at the top of the raw value, so a trait
/// block nested inside `allOf[1]` is invisible to it and every trait lookup
/// would come back "not declared" — see `declaration_tests.rs`'s fixture
/// comment for the history of this exact mistake.
fn schema(unit: &str) -> GtsTypeSchema {
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
                "canonical_unit": unit,
                "retention": "P125D"
            }
        }),
        None,
        Some(Arc::new(base)),
    )
    .expect("derived schema")
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
}

fn cfg(ttl: Duration) -> TypeResolverConfig {
    TypeResolverConfig { ttl, capacity: 64 }
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
    assert!(
        matches!(err, DomainError::TypesRegistryUnavailable(ref msg) if msg == "connect refused"),
        "unexpected error variant: {err:?}"
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
    let resolver = TypeResolver::new(
        source.clone(),
        TypeResolverConfig {
            ttl: Duration::from_mins(5),
            capacity: 2,
        },
        RecordingMetrics::arc(),
    );

    resolver.resolve(&meter_id()).await.expect("resolves a"); // oldest
    resolver.resolve(&meter_id_b()).await.expect("resolves b");
    resolver.resolve(&meter_id_c()).await.expect("resolves c"); // over capacity, evicts a
    assert_eq!(source.calls(), 3);

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
