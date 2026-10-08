//! Tests for [`super::log_operation_completed`]
//! (`cpt-cf-usage-collector-algo-telemetry-log-correlation`).
//!
//! ## Capture mechanism (ruling — Task 9's capture pattern is the template)
//!
//! Two halves, copied from `service_metrics_tests.rs`'s
//! `ensure_plugin_spi_dispatch_callsite_is_interesting` /
//! `every_spi_dispatch_runs_inside_a_host_opened_span`, for the same
//! documented reason: `tracing` caches each callsite's `Interest`
//! process-wide, and hundreds of sibling tests in this binary dispatch
//! through `log_operation_completed`'s one `tracing::info!` callsite with no
//! subscriber installed at all. Whichever reaches it first, on a thread with
//! no subscriber, can cache it as `Interest::never` — after which a later
//! thread-local override (`tracing::subscriber::set_default`) is consulted
//! too late, because `Interest` is resolved once per callsite and the cached
//! answer short-circuits the check before the active dispatcher is ever
//! read.
//!
//! - [`ensure_operation_log_callsite_is_interesting`] installs a **global**,
//!   once-per-binary `tracing` default at `TRACE`
//!   (`tracing::subscriber::set_global_default`), discarding everything it
//!   writes. A global `Dispatch` is leaked by `tracing-core` and never
//!   drops, so once installed it stays alive for the rest of the process —
//!   which is what keeps `Interest::and`'s combined answer unable to
//!   collapse back to `Never` no matter which sibling test's `Dispatch::new`
//!   runs after it.
//!
//!   **This reach is not scoped to this module's own callsite.** Being
//!   unconditional and process-wide, installing it also rebuilds and then
//!   holds the `Interest` cache for every *other* callsite in the binary —
//!   `service_metrics_tests.rs`'s `plugin_spi_dispatch` among them.
//!   Converting this installer to scoped-only (`set_default`) capture would
//!   therefore silently resurface a hazard in `service_metrics_tests.rs`
//!   that no test there currently catches on its own. See that file's
//!   matching note at `ensure_plugin_spi_dispatch_callsite_is_interesting`
//!   for the measured counterfactual; the mechanism is documented there in
//!   full, so it is not repeated here.
//! - Every test below additionally installs its own **scoped**
//!   [`OperationLogCaptureLayer`] via `tracing::subscriber::set_default`,
//!   whose `DefaultGuard` stops capturing the moment it drops at the end of
//!   that test. A **shared, unscoped** buffer would satisfy only "this event
//!   appeared" — never "this event did not", nor an exact count — because
//!   every other test in the same binary would be free to write into it too
//!   (`redis-cluster-plugin/tests/common/mod.rs`'s own doc on this point).
//!   Three assertions below depend on scoping: the absent-`trace_id` field
//!   in [`an_operation_with_no_inbound_traceparent_still_emits_exactly_one_entry`],
//!   the absent metadata/reason fields in
//!   [`a_rejection_log_entry_carries_no_caller_supplied_metadata_or_reason_text`],
//!   and every exact `len() == 1` count in this file.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use axum::http::HeaderMap;
use tracing::Instrument;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;

use toolkit_gts::gts_id;
use usage_collector_sdk::{
    CreateUsageRecord, EntryType, IdempotencyKey, MeterTypeId, RecordOrigin, ResourceRef,
    UsageRecord,
};
use uuid::Uuid;

use crate::config::IngestionQuotaConfig;
use crate::domain::ports::metrics::{IngestRequestErrorCategory, IngestRequestOutcome, PdpOp};
use crate::domain::test_support::{
    DenyAllResolver, HappyPathPlugin, ServiceFixture, authenticated_ctx, counter_sum_with_label,
    fake_declaration_source_with_fold, qty, recent_window_end, recent_window_start,
};

use super::LOG_TARGET;

const SAMPLE_METER_TYPE_ID: &str =
    gts_id!("cf.core.uc.usage_record.v1~example.observability._.bytes_in.v1~");

fn sample_create_record() -> CreateUsageRecord {
    CreateUsageRecord {
        entry_type: EntryType::Record,
        gts_type_id: MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid gts_type_id"),
        tenant_id: Uuid::from_u128(2),
        resource_ref: ResourceRef::new("obs-rsc-1", "compute.vm").expect("valid resource ref"),
        subject_ref: None,
        metadata: BTreeMap::new(),
        quantity: qty("1"),
        idempotency_key: Some(IdempotencyKey::new("obs-idem-1").expect("valid idempotency key")),
        invalidation: None,
        window_start: recent_window_start(),
        window_end: recent_window_end(),
    }
}

fn sample_record() -> UsageRecord {
    UsageRecord {
        id: Uuid::from_u128(0x5a51),
        gts_type_id: MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid gts_type_id"),
        tenant_id: Uuid::from_u128(2),
        resource_ref: ResourceRef::new("obs-rsc-1", "compute.vm").expect("valid resource ref"),
        subject_ref: None,
        metadata: BTreeMap::new(),
        quantity: qty("1"),
        idempotency_key: IdempotencyKey::new("obs-idem-1").expect("valid idempotency key"),
        accepted_at: recent_window_end(),
        origin: RecordOrigin::Live,
        invalidation: None,
        window_start: recent_window_start(),
        window_end: recent_window_end(),
    }
}

// ── Global interest keeper (see module doc) ─────────────────────────────

/// See the module doc's capture-mechanism section. Mirrors
/// `service_metrics_tests::ensure_plugin_spi_dispatch_callsite_is_interesting`
/// exactly, pointed at this module's own callsite family instead.
fn ensure_operation_log_callsite_is_interesting() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(std::io::sink)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let _installed = tracing::subscriber::set_global_default(subscriber);
    });
}

// ── Scoped capture layer (events + the ambient span's trace_id) ────────

/// One captured [`super::log_operation_completed`] event: its own fields,
/// plus the `trace_id` recorded on whichever span was current when the
/// event fired (`None` if no span was current, or the current span never
/// had the field declared / recorded).
#[derive(Debug, Clone, Default)]
struct CapturedEntry {
    fields: HashMap<String, String>,
    trace_id: Option<String>,
}

impl CapturedEntry {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }
}

/// Visits an event's or a span's declared fields, recording only `str`
/// values (every field this module's family ever carries is a `&str` or an
/// `Option<&str>`, the latter visited as `str` by `tracing-core`'s blanket
/// `Value` impl when `Some`, and not visited at all when `None` — which is
/// exactly the presence/absence signal several tests below assert on).
struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }
}

/// Reads just `trace_id` off a span's attributes / record delta — the one
/// field [`the_correlation_identifier_is_carried_through_unchanged`] and
/// [`an_operation_with_no_inbound_traceparent_still_emits_exactly_one_entry`]
/// need off the *span*, as opposed to the event.
struct TraceIdVisitor(Option<String>);

impl tracing::field::Visit for TraceIdVisitor {
    fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "trace_id" {
            self.0 = Some(value.to_owned());
        }
    }
}

/// Per-span extension storing the last `trace_id` value
/// [`toolkit_http::otel::set_parent_from_headers`] recorded on it, if any.
#[derive(Clone)]
struct TraceIdExt(Option<String>);

/// Captures every [`super::LOG_TARGET`] event seen while this layer is
/// installed, pairing each with the `trace_id` recorded on its enclosing
/// span (if any). Filtered to [`super::LOG_TARGET`] so a test's `len() ==
/// 1` oracle is never diluted by an unrelated `tracing::info!` /
/// `tracing::warn!` the same code path happens to also emit (ingestion's
/// own quota-rejection warning, for one).
#[derive(Clone, Default)]
struct OperationLogCaptureLayer {
    entries: Arc<Mutex<Vec<CapturedEntry>>>,
}

impl<S> tracing_subscriber::Layer<S> for OperationLogCaptureLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = TraceIdVisitor(None);
        attrs.record(&mut visitor);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(TraceIdExt(visitor.0));
        }
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = TraceIdVisitor(None);
        values.record(&mut visitor);
        if let Some(trace_id) = visitor.0
            && let Some(span) = ctx.span(id)
        {
            let mut ext = span.extensions_mut();
            // `on_new_span` already inserted a `TraceIdExt` (possibly
            // `None`) for every span this layer sees, so this is always an
            // update, never a first insert — `Extensions::insert` panics
            // on a second insert of the same type, which is exactly what a
            // bare `.insert()` here would do the moment
            // `set_parent_from_headers` records `trace_id` on a span this
            // layer already instrumented.
            if let Some(existing) = ext.get_mut::<TraceIdExt>() {
                existing.0 = Some(trace_id);
            } else {
                ext.insert(TraceIdExt(Some(trace_id)));
            }
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() != LOG_TARGET {
            return;
        }
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        event.record(&mut visitor);
        let trace_id = ctx.event_span(event).and_then(|span| {
            span.extensions()
                .get::<TraceIdExt>()
                .and_then(|ext| ext.0.clone())
        });
        self.entries
            .lock()
            .expect("mutex")
            .push(CapturedEntry { fields, trace_id });
    }
}

/// Install a scoped [`OperationLogCaptureLayer`], run `fut` inside it, and
/// hand back both `fut`'s output and whatever was captured. The
/// [`ensure_operation_log_callsite_is_interesting`] global keeps the
/// callsite interesting for the thread-local override below to actually
/// take effect (see the module doc).
async fn capture<F: std::future::Future>(fut: F) -> (F::Output, Vec<CapturedEntry>) {
    ensure_operation_log_callsite_is_interesting();
    let layer = OperationLogCaptureLayer::default();
    let entries = layer.entries.clone();
    let subscriber = tracing_subscriber::registry().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);
    let output = fut.await;
    let captured = entries.lock().expect("mutex").clone();
    (output, captured)
}

/// Builds the api-gateway's own `http_request` span shape (trace-context
/// placeholders declared, then [`toolkit_http::otel::set_parent_from_headers`]
/// records them) — the one this gear has none of its own to reuse (Step 0 /
/// this module's doc). `traceparent`, when given, is the inbound W3C header
/// value; `None` drives the absent-`traceparent` path.
///
/// **Must be called with the capturing subscriber already installed as
/// default** (inside [`capture`]'s `fut`), so the span this creates is
/// registered with — and records onto — the layer under test, not whatever
/// dispatcher happened to be default beforehand (see this file's capture
/// helper and `otel::set_parent_from_headers`'s own call-site precedent in
/// `api-gateway/src/gear.rs`).
fn ambient_request_span(traceparent: Option<&str>) -> tracing::Span {
    let span = tracing::info_span!(
        "http_request",
        trace_id = tracing::field::Empty,
        parent.trace_id = tracing::field::Empty
    );
    let mut headers = HeaderMap::new();
    if let Some(tp) = traceparent {
        headers.insert(
            "traceparent",
            tp.parse().expect("valid traceparent header value"),
        );
    }
    toolkit_http::otel::set_parent_from_headers(&span, &headers);
    span
}

const VALID_TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const VALID_TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

fn happy_path_service() -> Arc<crate::domain::Service> {
    ServiceFixture::default()
        .with_resolver(crate::domain::test_support::CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build(
            {
                let plugin = HappyPathPlugin::new();
                // The service method under test is `create_usage_records`,
                // which the plugin double answers through its own
                // `create_usage_records` SPI method — the only persist method
                // the SPI declares.
                plugin.set_create_records(vec![Ok(sample_record())]);
                plugin
            },
            "test.observability.happy.plugin.v1",
        )
}

fn denying_service() -> Arc<crate::domain::Service> {
    ServiceFixture::default()
        .with_resolver(Arc::new(DenyAllResolver))
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build(HappyPathPlugin::new(), "test.observability.deny.plugin.v1")
}

/// `happy_path_service` with a one-token bucket, so the second submission in
/// a test is throttled. `sustained_entries_per_sec: 0` means the bucket never
/// refills within a test's lifetime.
fn quota_limited_service() -> Arc<crate::domain::Service> {
    ServiceFixture::default()
        .with_resolver(crate::domain::test_support::CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_fold("SUM"))
        .with_ingestion_quota(IngestionQuotaConfig {
            sustained_entries_per_sec: 0,
            burst_entries: 1,
            idle_eviction_secs: 900,
        })
        .build(
            {
                let plugin = HappyPathPlugin::new();
                plugin.set_create_records(vec![Ok(sample_record())]);
                plugin
            },
            "test.observability.quota.plugin.v1",
        )
}

// ── Step 3 / Step 9 pair (ruling I51 — Step 9 is meaningless without
//    Step 3; see this file's module doc and each test's own doc) ────────

/// `inst-log-unchanged`: not re-encoded, truncated, prefixed or replaced.
///
/// Asserted by EQUALITY on the field, never by a `contains` over the
/// rendered line. A trace-id is 32 hex characters, and a substring
/// assertion over hex is H83's flake mechanism exactly — the defect that
/// produced 12 failures in 360 runs on a sentinel of `"999"`. There is no
/// non-hex sentinel embeddable in the id itself (it must parse as hex); the
/// robustness instead comes from the assertion style, not from the value.
///
/// **This test is Step 9's sibling and is load-bearing for it**: a field
/// never declared at span creation is *always* absent, so Step 9's own
/// absence assertion would pass vacuously without this test also existing
/// and passing — see `OperationLogEntry`'s module doc and `ruling I51`.
#[tokio::test]
async fn the_correlation_identifier_is_carried_through_unchanged() {
    let service = happy_path_service();
    let ctx = authenticated_ctx();

    let (outcome, entries) = capture(async {
        let span = ambient_request_span(Some(VALID_TRACEPARENT));
        service
            .create_usage_records(&ctx, vec![sample_create_record()])
            .instrument(span)
            .await
    })
    .await;
    let result = outcome
        .expect("a one-entry submission is not refused request-wide here")
        .into_iter()
        .next()
        .expect("one entry in, one slot out");

    result.expect("permitted single emit persists");
    assert_eq!(entries.len(), 1, "exactly one entry: {entries:?}");
    assert_eq!(
        entries[0].trace_id.as_deref(),
        Some(VALID_TRACE_ID),
        "the field must equal the inbound id, not merely contain it: {entries:?}"
    );
}

/// `inst-log-take-correlation` says take the identifier "rather than
/// generating one". A request with no `traceparent` is the common
/// development / internal-caller case, and the easiest to leave unhandled
/// by silently synthesizing a replacement id.
///
/// **Meaningful only together with
/// [`the_correlation_identifier_is_carried_through_unchanged`]** (ruling
/// I51): without that sibling proving the field is wired up at all, this
/// test's absence assertion would pass whether or not correlation worked,
/// because an undeclared field is always absent. Do not delete the sibling
/// as "redundant" — it is this test's only evidence of meaning.
#[tokio::test]
async fn an_operation_with_no_inbound_traceparent_still_emits_exactly_one_entry() {
    let service = happy_path_service();
    let ctx = authenticated_ctx();

    let (outcome, entries) = capture(async {
        let span = ambient_request_span(None);
        service
            .create_usage_records(&ctx, vec![sample_create_record()])
            .instrument(span)
            .await
    })
    .await;
    let result = outcome
        .expect("a one-entry submission is not refused request-wide here")
        .into_iter()
        .next()
        .expect("one entry in, one slot out");

    result.expect("permitted single emit persists");
    assert_eq!(entries.len(), 1, "exactly one entry: {entries:?}");
    assert_eq!(
        entries[0].trace_id, None,
        "no traceparent arrived, so no correlation field read off the ambient \
         span — and in particular no NEW, generated id — may appear: {entries:?}"
    );
    // Belt-and-braces against a *different* mutation shape than the one
    // above: a generated id threaded as the event's OWN field (bypassing
    // the ambient span entirely) wouldn't be caught by the assertion above
    // at all, since that one only reads what `OperationLogCaptureLayer`
    // resolved off the span. Checking the raw captured fields too closes
    // that gap.
    assert_eq!(
        entries[0].field("trace_id"),
        None,
        "no generated id may appear as the event's own field either: {entries:?}"
    );
}

// ── Step 4: the count oracle ─────────────────────────────────────────

/// `inst-log-one-per-operation`: "exactly one entry per completed
/// operation, on the accepting path and on every rejecting path alike".
///
/// The oracle is a COUNT, not an existence check: "a log line exists"
/// passes when two are emitted. Mutation this test catches: duplicate the
/// `tracing::info!` call inside `log_operation_completed` and this test
/// reds on the accepting arm (`entries.len() == 2`) while an
/// existence-only assertion would stay green.
#[tokio::test]
async fn exactly_one_entry_is_emitted_per_completed_operation() {
    let accepting_service = happy_path_service();
    let ctx = authenticated_ctx();
    let (accepted_outcome, accepted_entries) =
        capture(accepting_service.create_usage_records(&ctx, vec![sample_create_record()])).await;
    let accepted = accepted_outcome
        .expect("a one-entry submission is not refused request-wide here")
        .into_iter()
        .next()
        .expect("one entry in, one slot out");
    accepted.expect("permitted single emit persists");
    assert_eq!(
        accepted_entries.len(),
        1,
        "accepting path: exactly one entry, not >= 1: {accepted_entries:?}"
    );

    let denying = denying_service();
    let (rejected_outcome, rejected_entries) =
        capture(denying.create_usage_records(&ctx, vec![sample_create_record()])).await;
    let rejected = rejected_outcome
        .expect(
            "a PDP deny is a per-record rejection, not a request-wide refusal, so the batch \
             call still returns an outer Ok",
        )
        .into_iter()
        .next()
        .expect("one entry in, one slot out");
    rejected.expect_err("PDP deny rejects the submission");
    assert_eq!(
        rejected_entries.len(),
        1,
        "rejecting path: exactly one entry, not >= 1: {rejected_entries:?}"
    );
}

// ── Step 5: the three field-vocabulary tests ────────────────────────

/// `inst-log-shared-vocabulary`: the log's category string equals the
/// corresponding `as_str()` of the same enum the counter used — not a
/// hand-written literal, which could drift from the counter's spelling
/// while both tests stayed green. Driven over the PDP-deny rejection so
/// `error_category` is non-neutral.
///
/// Read straight off the exported `uc_ingestion_records_total` data
/// point's own `error_category` attribute — not recomputed via a second,
/// independent call to the classifier `Service` used — because re-deriving
/// the expected value the same way the production code derives it would
/// only prove the two calls agree, not that the log and the counter (the
/// two *outputs*) actually carry the same string.
#[tokio::test]
async fn the_log_entrys_vocabulary_equals_the_counters_enum_spelling() {
    let (denying, provider, exporter) = ServiceFixture::default()
        .with_resolver(Arc::new(DenyAllResolver))
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build_with_metrics(HappyPathPlugin::new(), "test.observability.vocab.plugin.v1");
    let ctx = authenticated_ctx();
    let (outcome, entries) =
        capture(denying.create_usage_records(&ctx, vec![sample_create_record()])).await;
    let result = outcome
        .expect(
            "a PDP deny is a per-record rejection, not a request-wide refusal, so the batch \
             call still returns an outer Ok",
        )
        .into_iter()
        .next()
        .expect("one entry in, one slot out");
    result.expect_err("PDP deny rejects the submission");
    provider.force_flush().expect("flush");

    let category_values: Vec<String> =
        crate::domain::test_support::counter_label_pairs(&exporter, "uc_ingestion_records_total")
            .into_iter()
            .filter(|(key, _)| key == "error_category")
            .map(|(_, value)| value)
            .collect();
    assert_eq!(
        category_values.len(),
        1,
        "exactly one error_category value recorded on the counter for this single rejection: \
         {category_values:?}"
    );
    let expected_category = category_values[0].as_str();

    assert_eq!(entries.len(), 1, "exactly one entry: {entries:?}");
    let entry = &entries[0];
    assert_eq!(entry.field("operation"), Some(PdpOp::Ingest.as_str()));
    // Genuine batch-path divergence (Task 2 migration finding, not a
    // migration artifact): `create_usage_records_for_origin` emits its
    // completion log ONCE PER REQUEST, after the per-record loop, labelled
    // with `IngestRequestOutcome` (accepted/partial/rejected) — never with
    // the single-emit path's per-record `RecordOutcome` (accepted/rejected).
    // A one-record batch whose only record is PDP-denied is `per_record =
    // Ok(vec![Err(..)])`, so `request_outcome` takes the
    // `per_record.iter().any(Result::is_err)` branch and is `Partial`
    // ("at least one per-record rejection"), not `Rejected` — `Rejected`
    // is reserved for a whole-request refusal (the outer `Err` arm), which
    // a per-record PDP deny never produces.
    assert_eq!(
        entry.field("outcome"),
        Some(IngestRequestOutcome::Partial.as_str()),
        "the batch completion log is request-scoped: a single denied record makes the \
         request `partial` (\"at least one per-record rejection\"), not `rejected` \
         (reserved for a whole-request refusal) - see service.rs's \
         `create_usage_records_for_origin` completion block"
    );
    // A second genuine divergence, same root cause: the completion log's
    // `error_category` is ALSO request-scoped, and
    // `create_usage_records_for_origin` passes `IngestRequestErrorCategory::None`
    // unconditionally whenever `request_outcome` is `accepted` or `partial`
    // — regardless of which per-record categories actually occurred. So,
    // unlike the single-emit path, the batch completion log can no longer
    // equal the per-record counter's non-neutral category for a rejection
    // folded into a `partial` request. `inst-log-shared-vocabulary`'s
    // original oracle (log category == per-record counter category) does
    // not hold at the request-log granularity; this is reported in the
    // Task 2 report as a spec-worthy finding, not patched around here.
    assert_eq!(
        entry.field("error_category"),
        Some(IngestRequestErrorCategory::None.as_str()),
        "the request-scoped completion log's error_category is unconditionally \"none\" \
         for an `accepted` or `partial` request, even though this request's one record was \
         denied for cause"
    );
    assert_ne!(
        entry.field("error_category"),
        Some(expected_category),
        "making the divergence explicit: the per-record counter still carries the \
         non-neutral `{expected_category}` category for this PDP deny, while the \
         request-scoped completion log does not carry it at all"
    );
}

/// `inst-log-identifiers-here`: tenant, subject, resource and referenced
/// type present as opaque strings, for an operation whose attribution
/// tuple carries exactly one of each.
///
/// **Task 2 migration finding, escalated ahead of Task 6.** This requirement
/// held for `Service::create_usage_record`'s completion log, before Task 6
/// deleted that method (`service.rs`'s since-removed single-emit path
/// chained `.subject(..)`, `.resource(..)` and `.referenced_type(..)`
/// alongside `.tenant(..)`), but it never held for
/// `Service::create_usage_records`'s: `create_usage_records_for_origin`'s own
/// completion block (`log_operation_completed` after the per-record loop)
/// chains only `.tenant(..)` — never `.resource(..)`, `.subject(..)` or
/// `.referenced_type(..)`, for any batch size, including one. That is
/// structurally deliberate in the production code (a batch has no single
/// resource/type/subject the request-level entry could attribute), not a
/// bug introduced by this migration. The consequence: now that Task 6 has
/// deleted `create_usage_record`, **no live ingestion code path will ever
/// again log `resource` / `referenced_type` / `subject`** on a completion
/// entry — `inst-log-identifiers-here` is unsatisfiable by any call this
/// gear makes. This was reported in the Task 2 report as the most
/// significant finding of that task; the assertions below were updated to
/// match current, genuine behaviour rather than silently dropped: the test
/// now asserts only that `tenant` is present and that `resource` /
/// `referenced_type` are absent (`subject` is not asserted on at all, since
/// the batch completion log never sets it either).
#[tokio::test]
async fn the_log_entry_carries_only_the_tenant_identifier() {
    let service = happy_path_service();
    let ctx = authenticated_ctx();
    let record = sample_create_record();
    let expected_tenant = record.tenant_id.to_string();
    let expected_resource = record.resource_ref.resource_id().to_owned();
    let expected_type = record.gts_type_id.as_str().to_owned();

    let (outcome, entries) = capture(service.create_usage_records(&ctx, vec![record])).await;
    let result = outcome
        .expect("a one-entry submission is not refused request-wide here")
        .into_iter()
        .next()
        .expect("one entry in, one slot out");
    result.expect("permitted single emit persists");

    assert_eq!(entries.len(), 1, "exactly one entry: {entries:?}");
    let entry = &entries[0];
    assert_eq!(entry.field("tenant"), Some(expected_tenant.as_str()));
    assert_eq!(
        entry.field("resource"),
        None,
        "the batch completion log never carries `resource` - \
         `create_usage_records_for_origin`'s completion block chains only `.tenant(..)`, \
         unlike the single-emit path's. Genuine divergence (Task 2 report), not a migration \
         artifact: {expected_resource:?} was this submission's own resource id"
    );
    assert_eq!(
        entry.field("referenced_type"),
        None,
        "same divergence: `referenced_type` is never set on the batch completion log either \
         - {expected_type:?} was this submission's own gts_type_id"
    );
}

/// `inst-log-no-metadata`: no caller-supplied metadata value and no
/// invalidation reason text, driven over an operation carrying both, with
/// recognizable marker strings neither may leak into.
///
/// Mutation this test catches: thread a `metadata` or `reason_code` field
/// into `OperationLogEntry` / `log_operation_completed` and this test reds
/// the moment the marker string appears in any captured field value.
#[tokio::test]
async fn a_rejection_log_entry_carries_no_caller_supplied_metadata_or_reason_text() {
    const METADATA_MARKER: &str = "OBS-METADATA-MARKER-DO-NOT-LEAK";
    const REASON_MARKER: &str = "OBS-REASON-MARKER-DO-NOT-LEAK";

    let service = happy_path_service();
    let ctx = authenticated_ctx();
    let mut record = sample_create_record();
    record.metadata.insert(
        usage_collector_sdk::MetadataKey::new("note").expect("valid metadata key"),
        METADATA_MARKER.to_owned(),
    );
    // Both carried in the SAME submission: `entry_type = Invalidation` is
    // the only shape that admits a reason code at all
    // (`CreateUsageRecord::invalidation`), and no target is configured on
    // the fixture plugin, so this submission is rejected
    // (`UsageRecordNotFound` resolving the invalidation's target) rather
    // than accepted. That is fine — and arguably a better oracle than the
    // happy path: the convergence point this operation logs through is the
    // one every path shares — `create_usage_records_for_origin`'s own
    // `log_operation_completed` call site, the sole surviving one now that
    // Task 6 deleted `Service::create_usage_record` and its separate call
    // site — so a marker leaking on the rejecting arm would equally red
    // here.
    let record = CreateUsageRecord {
        entry_type: EntryType::Invalidation,
        invalidation: Some(
            usage_collector_sdk::ReasonCode::new(REASON_MARKER).expect("valid reason code"),
        ),
        ..record
    };

    let (outcome, entries) = capture(service.create_usage_records(&ctx, vec![record])).await;
    let result = outcome
        .expect(
            "a target-not-found invalidation is a per-record rejection, not a request-wide \
             refusal, so the batch call still returns an outer Ok",
        )
        .into_iter()
        .next()
        .expect("one entry in, one slot out");
    assert!(
        result.is_err(),
        "no target is configured on the fixture plugin, so this invalidation rejects"
    );

    assert_eq!(entries.len(), 1, "exactly one entry: {entries:?}");
    let entry = &entries[0];
    for value in entry.fields.values() {
        assert!(
            !value.contains(METADATA_MARKER),
            "metadata value leaked into the log entry: {entry:?}"
        );
        assert!(
            !value.contains(REASON_MARKER),
            "reason text leaked into the log entry: {entry:?}"
        );
    }
}

// ── Step 7: the rejected-before-the-service path ────────────────────

/// Review Focus item 4. A submission rejected at the REST edge before the
/// service is called still completes an operation, and
/// `inst-log-one-per-operation` requires its entry "so the log count
/// matches the request counter." T5 made `uc_ingestion_requests_total`
/// move for this case (`Service::record_structural_cap_rejection`, the one
/// seam both the REST edge's own copy of the structural cap
/// (`api/rest/handlers/usage_records.rs::dispatch_usage_record_batch`) and
/// the service's internal cap arm call into); this test proves the log
/// moves with it.
///
/// Asserts the counter and the log count **together**, in one test: two
/// tests each asserting one would both pass against an implementation that
/// moved only one of them.
///
/// Driven by calling `record_structural_cap_rejection` directly — the
/// exact function both arms share — rather than assembling a full
/// `axum::Router`: this is a domain-layer test
/// (`usage-collector/src/domain/observability_tests.rs`), and the
/// REST-handler call site is a one-line pass-through onto this same
/// method, already covered by the production-code change in
/// `api/rest/handlers/usage_records.rs`.
#[tokio::test]
async fn a_submission_rejected_before_the_service_still_emits_exactly_one_entry() {
    let (service, provider, exporter) = ServiceFixture::default().build_with_metrics(
        HappyPathPlugin::new(),
        "test.observability.precap.plugin.v1",
    );
    let ctx = authenticated_ctx();

    let ((), entries) =
        capture(async { service.record_structural_cap_rejection(&ctx, PdpOp::Ingest) }).await;

    provider.force_flush().expect("flush");
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "outcome",
            "rejected"
        ),
        1,
        "the request counter must move for a submission rejected before the service"
    );
    assert_eq!(
        entries.len(),
        1,
        "the log count must match the request counter: {entries:?}"
    );
    assert_eq!(entries[0].field("operation"), Some(PdpOp::Ingest.as_str()));
    assert_eq!(
        entries[0].field("outcome"),
        Some(IngestRequestOutcome::Rejected.as_str())
    );
    assert_eq!(
        entries[0].field("error_category"),
        Some(IngestRequestErrorCategory::Validation.as_str())
    );
}

// ── Quota-rejection convergence pin ──────────────────────────────────

/// The deleted single-emit path charged the quota
/// through a bare `?`, which returned before its log-and-metrics convergence
/// block, so a quota-rejected single submission produced no
/// `usage_collector::operation` entry at all. The batch path's quota arm
/// always logged correctly; removing the other path is what made
/// `inst-log-one-per-operation`'s unconditional clause — "exactly one entry
/// per completed operation... on the accepting path and on every rejecting
/// path alike" — true without exception. This test is what keeps it true.
#[tokio::test]
async fn a_quota_rejected_submission_logs_exactly_one_operation_entry() {
    let service = quota_limited_service();
    let ctx = authenticated_ctx();

    let (first, _) =
        capture(service.create_usage_records(&ctx, vec![sample_create_record()])).await;
    first.expect("the one token the bucket holds admits the first submission");

    let (second, entries) =
        capture(service.create_usage_records(&ctx, vec![sample_create_record()])).await;
    second.expect_err("the second submission exceeds the allowance and is refused whole");

    // `capture` is scoped per call, so `entries` holds only the second
    // submission's. The layer already filters on the operation-log target,
    // so every captured entry is one.
    assert_eq!(
        entries.len(),
        1,
        "a quota-rejected submission is a completed, rejected operation and \
         owes exactly one operation log entry: {entries:?}",
    );
    assert_eq!(entries[0].field("outcome"), Some("rejected"));
    assert_eq!(entries[0].field("error_category"), Some("quota"));
}
