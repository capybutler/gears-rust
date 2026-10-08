#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use types_registry_sdk::{GtsTypeId, GtsTypeSchema};
use usage_collector_sdk::MeterTypeId;
use uuid::Uuid;

use super::MeterReverseResolver;
use crate::domain::error::DomainError;
use crate::domain::ports::declaration_mirror::{DeclarationMirror, MirrorError};
use crate::domain::ports::declarations::DeclarationSource;
use crate::domain::ports::metrics::{NoopMetrics, UsageCollectorMetrics};

const METER: &str = "gts.cf.core.uc.usage_record.v1~example.metering._.stored_volume.v1~";
const BASE: &str = "gts.cf.core.uc.usage_record.v1~";

fn meter_id() -> MeterTypeId {
    MeterTypeId::new(METER).expect("valid meter id")
}

fn meter_schema() -> GtsTypeSchema {
    let base = GtsTypeSchema::try_new(
        GtsTypeId::try_new(BASE).expect("base id"),
        json!({"type": "object", "x-gts-abstract": true}),
        None,
        None,
    )
    .expect("base schema");
    GtsTypeSchema::try_new(
        GtsTypeId::try_new(METER).expect("meter id"),
        json!({
            "allOf": [{ "$ref": format!("gts://{BASE}") }],
            "x-gts-traits": { "aggregation_fold": "SUM", "canonical_unit": "bytes" }
        }),
        None,
        Some(Arc::new(base)),
    )
    .expect("meter schema")
}

/// A source that answers one schema by reference and counts its calls.
struct ScriptedSource {
    schema: Option<GtsTypeSchema>,
    error: Option<DomainError>,
    calls: AtomicUsize,
}

impl ScriptedSource {
    fn answering(schema: GtsTypeSchema) -> Arc<Self> {
        Arc::new(Self {
            schema: Some(schema),
            error: None,
            calls: AtomicUsize::new(0),
        })
    }
    fn failing(error: DomainError) -> Arc<Self> {
        Arc::new(Self {
            schema: None,
            error: Some(error),
            calls: AtomicUsize::new(0),
        })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl DeclarationSource for ScriptedSource {
    async fn fetch(&self, _id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError> {
        unimplemented!("the forward direction is not used here")
    }
    async fn fetch_by_uuid(&self, _type_uuid: Uuid) -> Result<GtsTypeSchema, DomainError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match (&self.schema, &self.error) {
            (Some(s), _) => Ok(s.clone()),
            (None, Some(e)) => Err(e.clone()),
            (None, None) => unreachable!("ScriptedSource needs a schema or an error"),
        }
    }
}

/// A mirror whose three behaviours are scripted independently.
struct ScriptedMirror {
    reverse: Option<MeterTypeId>,
    reverse_fails: bool,
    writes: AtomicUsize,
    reverse_reads: AtomicUsize,
    write_fails: bool,
    rows: StdMutex<HashMap<String, Uuid>>,
}

impl ScriptedMirror {
    fn build(reverse: Option<MeterTypeId>, reverse_fails: bool, write_fails: bool) -> Arc<Self> {
        Arc::new(Self {
            reverse,
            reverse_fails,
            writes: AtomicUsize::new(0),
            reverse_reads: AtomicUsize::new(0),
            write_fails,
            rows: StdMutex::new(HashMap::new()),
        })
    }
    fn empty() -> Arc<Self> {
        Self::build(None, false, false)
    }
    fn answering(id: MeterTypeId) -> Arc<Self> {
        Self::build(Some(id), false, false)
    }
    fn failing_reads() -> Arc<Self> {
        Self::build(None, true, false)
    }
    fn failing_writes() -> Arc<Self> {
        Self::build(None, false, true)
    }
    fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }
    /// How many times `resolve_id` (the tier-2 reverse read) was called.
    ///
    /// Exists so a test can prove a resolve was served from tier 1 (memory)
    /// without ever reaching the mirror at all, and so it can prove a
    /// tier-2 hit populated tier 1 — neither of which `writes()` or `row()`
    /// can show, since both are silent about *reads*.
    fn reverse_reads(&self) -> usize {
        self.reverse_reads.load(Ordering::SeqCst)
    }
    /// The registry reference most recently upserted under `id`, if any.
    ///
    /// Exists so a test can check which `(id, type_uuid)` PAIR a write
    /// carried, not merely that a write happened: `writes()` alone cannot
    /// distinguish a correctly-keyed upsert from one that stored the right
    /// identifier against the wrong reference (or vice versa).
    fn row(&self, id: &MeterTypeId) -> Option<Uuid> {
        self.rows.lock().expect("lock").get(id.as_str()).copied()
    }
}

#[async_trait]
impl DeclarationMirror for ScriptedMirror {
    async fn upsert(
        &self,
        id: &MeterTypeId,
        type_uuid: Uuid,
        _document: &Value,
    ) -> Result<(), MirrorError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        if self.write_fails {
            return Err(MirrorError::new("scripted write failure"));
        }
        self.rows
            .lock()
            .expect("lock")
            .insert(id.as_str().to_owned(), type_uuid);
        Ok(())
    }
    async fn read(&self, _id: &MeterTypeId) -> Result<Option<Value>, MirrorError> {
        Ok(None)
    }
    async fn resolve_id(&self, _type_uuid: Uuid) -> Result<Option<MeterTypeId>, MirrorError> {
        self.reverse_reads.fetch_add(1, Ordering::SeqCst);
        if self.reverse_fails {
            return Err(MirrorError::new("scripted reverse read failure"));
        }
        Ok(self.reverse.clone())
    }
}

fn resolver(
    source: Arc<dyn DeclarationSource>,
    mirror: Arc<dyn DeclarationMirror>,
) -> MeterReverseResolver {
    MeterReverseResolver::new(source, mirror, Arc::new(NoopMetrics))
}

/// Counts `record_declaration_mirror_write_failure` calls, so a test can pin
/// the deliberate asymmetry `meter_reverse.rs` documents: a mirror *read*
/// failure is logged only, a mirror *write* failure is logged AND counted
/// here. Every other test in this file builds the resolver with
/// [`NoopMetrics`], whose every method (including this one) is an empty
/// body — which proves `resolve` still returns the right `Result` under
/// either failure, but cannot tell the two failure arms apart. This fixture
/// is what lets the two tests that care about the asymmetry actually assert
/// it, instead of only asserting around it.
///
/// Mirrors `domain::type_resolver::resolver_tests::RecordingMetrics`'s own
/// `mirror_write_failures` counter and stub shape. A new, smaller copy
/// rather than a shared import: that fixture is private to its own module
/// and carries a dozen other counters/gauges this file has no use for.
#[derive(Default)]
struct RecordingMetrics {
    mirror_write_failures: AtomicUsize,
}

impl RecordingMetrics {
    fn arc() -> Arc<Self> {
        Arc::new(Self::default())
    }
    fn mirror_write_failure_count(&self) -> usize {
        self.mirror_write_failures.load(Ordering::SeqCst)
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
    fn record_type_resolution(&self, _: crate::domain::ports::metrics::TypeResolutionOutcome) {}
    fn set_resolved_types(&self, _: u64) {}
    fn set_declaration_cache_age_seconds(&self, _: Option<u64>) {}
    fn observe_type_resolution_duration(
        &self,
        _: crate::domain::ports::metrics::TypeResolutionOutcome,
        _: f64,
    ) {
    }
    fn record_declaration_mirror_write_failure(&self) {
        self.mirror_write_failures.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn a_mirror_hit_never_reaches_the_registry() {
    let source = ScriptedSource::answering(meter_schema());
    let mirror = ScriptedMirror::answering(meter_id());
    let r = resolver(source.clone(), mirror.clone());
    let type_uuid = meter_schema().type_uuid;

    let first = r.resolve(type_uuid).await.expect("resolves");
    // A second resolve against the same reference, so a test can tell a
    // tier-1 (in-memory) hit apart from a tier-2 (mirror) hit that merely
    // happens to answer the same way every time. If `resolve` consulted the
    // mirror before memory, this second call would still succeed —
    // `reverse_reads()` is what catches that reordering.
    let second = r.resolve(type_uuid).await.expect("resolves again");

    assert_eq!(first, meter_id());
    assert_eq!(second, meter_id());
    assert_eq!(source.calls(), 0, "tier 2 answered; tier 3 must not run");
    assert_eq!(mirror.writes(), 0, "a mirror hit writes nothing back");
    assert_eq!(
        mirror.reverse_reads(),
        1,
        "the second resolve must be served from tier 1 (memory), not the mirror again"
    );
}

#[tokio::test]
async fn a_registry_hit_writes_the_mirror_and_is_cached() {
    let schema = meter_schema();
    let source = ScriptedSource::answering(schema.clone());
    let mirror = ScriptedMirror::empty();
    let r = resolver(source.clone(), mirror.clone());

    assert_eq!(
        r.resolve(schema.type_uuid).await.expect("first"),
        meter_id()
    );
    assert_eq!(
        r.resolve(schema.type_uuid).await.expect("second"),
        meter_id()
    );

    assert_eq!(
        source.calls(),
        1,
        "the second resolve must be served from memory"
    );
    assert_eq!(
        mirror.writes(),
        1,
        "only the registry hit writes the mirror"
    );
    assert_eq!(
        mirror.row(&meter_id()),
        Some(schema.type_uuid),
        "the mirror row must carry the schema's own registry reference, not merely some row"
    );
    assert_eq!(
        mirror.reverse_reads(),
        1,
        "only the first resolve's cold miss may reach the mirror's reverse read; the \
         second resolve must be served from tier 1 (memory), which requires the \
         registry hit to have populated it via `remember`"
    );
}

#[tokio::test]
async fn a_mirror_read_failure_degrades_to_the_registry() {
    let schema = meter_schema();
    let source = ScriptedSource::answering(schema.clone());
    let metrics = RecordingMetrics::arc();
    let r = MeterReverseResolver::new(
        source.clone(),
        ScriptedMirror::failing_reads(),
        metrics.clone(),
    );

    let got = r
        .resolve(schema.type_uuid)
        .await
        .expect("a cache failure is not fatal");

    assert_eq!(got, meter_id());
    assert_eq!(source.calls(), 1);
    assert_eq!(
        metrics.mirror_write_failure_count(),
        0,
        "a mirror READ failure must not be counted on \
         uc_declaration_mirror_write_failures_total (spec ruling J6): that instrument's \
         published meaning is a declaration resolved but not mirrored, which a read failure is \
         not"
    );
}

#[tokio::test]
async fn a_mirror_write_failure_does_not_fail_the_resolve() {
    let schema = meter_schema();
    let source = ScriptedSource::answering(schema.clone());
    let mirror = ScriptedMirror::failing_writes();
    let metrics = RecordingMetrics::arc();
    let r = MeterReverseResolver::new(source.clone(), mirror.clone(), metrics.clone());

    let got = r
        .resolve(schema.type_uuid)
        .await
        .expect("a failed cache write is not fatal");

    assert_eq!(got, meter_id());
    assert_eq!(mirror.writes(), 1, "the write was attempted");
    assert_eq!(
        metrics.mirror_write_failure_count(),
        1,
        "a mirror WRITE failure must be counted on uc_declaration_mirror_write_failures_total"
    );
}

#[tokio::test]
async fn a_definite_not_found_is_reported_and_not_cached() {
    let source = ScriptedSource::failing(DomainError::DeclarationNotFound {
        gts_type_id: Uuid::from_u128(3).to_string(),
        reason: "is not declared under this registry reference".to_owned(),
    });
    let r = resolver(source.clone(), ScriptedMirror::empty());

    for _ in 0..2 {
        let err = r.resolve(Uuid::from_u128(3)).await.expect_err("not found");
        assert!(err.is_declaration_not_found(), "{err:?}");
    }
    assert_eq!(source.calls(), 2, "a failure must not be cached");
}

#[tokio::test]
async fn an_unavailable_registry_is_reported_and_not_cached() {
    let source = ScriptedSource::failing(DomainError::TypesRegistryUnavailable("down".to_owned()));
    let r = resolver(source.clone(), ScriptedMirror::empty());

    for _ in 0..2 {
        let err = r
            .resolve(Uuid::from_u128(4))
            .await
            .expect_err("unavailable");
        assert!(
            matches!(err, DomainError::TypesRegistryUnavailable(_)),
            "{err:?}"
        );
    }
    assert_eq!(source.calls(), 2, "a failure must not be cached");
}

#[tokio::test]
async fn a_reference_naming_a_non_meter_type_is_a_definite_not_found() {
    // Review Focus 2. The registry answers with a perfectly valid type schema
    // that is not a meter: it does not derive from the usage-record base type,
    // so `MeterTypeId::new` refuses it.
    let other = GtsTypeSchema::try_new(
        GtsTypeId::try_new("gts.cf.core.am.tenant_metadata.v1~").expect("other id"),
        json!({"type": "object"}),
        None,
        None,
    )
    .expect("other schema");
    let reference = other.type_uuid;
    let source = ScriptedSource::answering(other);
    let r = resolver(source.clone(), ScriptedMirror::empty());

    for _ in 0..2 {
        let err = r.resolve(reference).await.expect_err("not a meter");
        assert!(
            err.is_declaration_not_found(),
            "a non-meter type under a reference is a definite answer: {err:?}"
        );
    }
    assert_eq!(source.calls(), 2, "a failure must not be cached");
}
