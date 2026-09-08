//! Output port for recording usage-collector operational metrics.
//!
//! Implementations live in [`crate::infra::metrics`] (OpenTelemetry
//! instruments declared on a scoped `Meter` from `ToolKit`'s global
//! `SdkMeterProvider`). Domain code depends only on this trait and the
//! label enums below — it has no knowledge of `OTel`, honoring the DDD-light
//! layer boundary (domain must not depend on infra / transport types).
//!
//! ## Naming contract (DESIGN §3.11.5)
//!
//! Unlike some sibling gears, usage-collector bakes the **full literal**
//! Prometheus name into the instrument (counters carry `_total`, duration
//! histograms carry `_seconds`) and sets **no** `.with_unit(...)` hint —
//! the account-management convention. The rendered Prometheus name is then
//! identical whether the downstream `OTel` collector runs with
//! `add_metric_suffixes` on or off. The concrete builder lives in the infra
//! impl; this port names each family in its method docs.
//!
//! ## Label cardinality (DESIGN §3.11.5 "Label cardinality")
//!
//! Every label is a closed, enumerated value set — modeled here as `enum`s
//! with `const fn as_str()`. Unbounded identifiers (`tenant_id`,
//! `resource_id`, `gts_id`, `trace_id`, idempotency keys) MUST NOT appear
//! as metric labels; they belong in structured logs and traces.

use toolkit_macros::domain_model;
use usage_collector_sdk::{EntryType, RecordOrigin};

/// Label key constants shared by the instrument families below.
pub mod key {
    /// `operation` — the domain operation (PDP) or SPI method (plugin host).
    pub const OPERATION: &str = "operation";
    /// `cause` — PDP-failure cause discriminator.
    pub const CAUSE: &str = "cause";
    /// `decision` — PDP permit/deny decision.
    pub const DECISION: &str = "decision";
    /// `error_category` — plugin-host backend-error classification.
    pub const ERROR_CATEGORY: &str = "error_category";
    /// `outcome` — request/record completion outcome.
    pub const OUTCOME: &str = "outcome";
    /// `entry_type` — measurement vs withdrawal.
    pub const ENTRY_TYPE: &str = "entry_type";
    /// `origin` — which ingestion path admitted the entry.
    pub const ORIGIN: &str = "origin";
    /// `query_kind` — aggregated vs raw query.
    pub const QUERY_KIND: &str = "query_kind";
    /// `result` — Type Resolver call outcome.
    pub const RESULT: &str = "result";
}

/// `operation` label for the PDP-helper instruments (`uc_pdp_*`,
/// `uc_authz_decisions_total`) — the usage-record gateway set. The
/// usage-type catalog surface (and its four PDP operations) is gone: every
/// type declaration is now owned by `types-registry` and resolved through
/// the Type Resolver, which is not a PDP-enforcing component.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdpOp {
    /// Live usage-record ingestion (single + batch emit) — see
    /// [`Self::Backfill`] for the import route's own label.
    Ingest,
    /// Bulk historical import — the backfill route's own ingestion.
    ///
    /// A **route** label, and it is not the same thing as the PEP verb
    /// `usage_record::actions::BACKFILL`, whose string it happens to
    /// share. The label follows the entry point; the verb follows the
    /// covered period. So a backfill-route entry whose period ends inside
    /// the configured window is labelled `operation="backfill"` here and
    /// authorized against `create` — and every entry of a backfill batch
    /// carries this label whichever verb it was authorized against, which
    /// is what keeps a bulk import's PDP latency and denial rate separable
    /// from live emission's.
    Backfill,
    /// Raw (non-aggregated) usage-record listing.
    QueryRaw,
    /// Aggregated usage-record query.
    QueryAggregated,
    /// Read a single usage record by id.
    GetRecord,
}

impl PdpOp {
    /// The bounded `operation` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ingest => "ingest",
            Self::Backfill => "backfill",
            Self::QueryRaw => "query_raw",
            Self::QueryAggregated => "query_aggregated",
            Self::GetRecord => "get_record",
        }
    }
}

/// `operation` label for the plugin-host instruments
/// (`uc_plugin_call_duration_seconds`, `uc_plugin_accept_errors_total`) —
/// the Plugin SPI method names. The four usage-type catalog SPI methods no
/// longer exist on [`usage_collector_sdk::UsageCollectorPluginV1`]: storage
/// plugins are pure usage-record persistence now.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginOp {
    /// `create_usage_record`.
    CreateUsageRecord,
    /// `create_usage_records`.
    CreateUsageRecords,
    /// `query_aggregated_usage_records`.
    QueryAggregatedUsageRecords,
    /// `list_usage_records`.
    ListUsageRecords,
    /// `get_usage_record`.
    GetUsageRecord,
}

impl PluginOp {
    /// The bounded `operation` label value (verbatim SPI method name).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CreateUsageRecord => "create_usage_record",
            Self::CreateUsageRecords => "create_usage_records",
            Self::QueryAggregatedUsageRecords => "query_aggregated_usage_records",
            Self::ListUsageRecords => "list_usage_records",
            Self::GetUsageRecord => "get_usage_record",
        }
    }
}

/// `cause` label for `uc_pdp_failures_total`.
///
/// **v1 mapping:** the bootstrap-bound `PolicyEnforcer` surfaces
/// `AuthZResolverError` (via `EnforcerError::EvaluationFailed`) which carries
/// no timeout discriminator, so every PDP failure maps to
/// [`PdpFailureCause::Unreachable`]. [`PdpFailureCause::Timeout`] is reserved
/// for a future host-side PDP-dispatch deadline (none exists in v1).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdpFailureCause {
    /// PDP unreachable / evaluation failed.
    Unreachable,
    /// Reserved for a future host-side dispatch deadline (not emitted in v1).
    Timeout,
}

impl PdpFailureCause {
    /// The bounded `cause` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unreachable => "unreachable",
            Self::Timeout => "timeout",
        }
    }
}

/// `decision` label for `uc_authz_decisions_total`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthzDecision {
    /// The gear permitted the request: the PDP permitted AND the post-permit
    /// gate (per-record attribution / query scope projection) admitted it.
    Permit,
    /// The gear denied the request: a PDP deny (`EnforcerError::Denied`), a
    /// fail-closed compile failure (`EnforcerError::CompileFailed`), or a
    /// permit-with-constraints the post-permit gate rejected (e.g. cross-tenant
    /// attribution outside the granted scope).
    Deny,
}

impl AuthzDecision {
    /// The bounded `decision` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Permit => "permit",
            Self::Deny => "deny",
        }
    }
}

/// `error_category` label for `uc_plugin_accept_errors_total`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginErrorCategory {
    /// Structural-unready short-circuit (no plugin handle resolved).
    Unready,
    /// Plugin returned a backend-classified fault (`Transient` / `Internal`).
    BackendError,
    /// Host-side dispatch deadline expiry (reserved; no deadline exists in v1).
    Timeout,
}

impl PluginErrorCategory {
    /// The bounded `error_category` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unready => "unready",
            Self::BackendError => "backend_error",
            Self::Timeout => "timeout",
        }
    }
}

// ── Phase 2: per-component gateway label vocabularies (DESIGN §3.11.5) ──

/// `outcome` label for `uc_query_requests_total` (§3.11.5: `success` on a
/// successful return, `denied` on a completed PDP deny, `error`
/// otherwise). A second counter shared this vocabulary until its
/// instrument was retired; the query counter is its sole consumer now.
/// `uc_ingestion_requests_total` is also request-scoped but keeps its own
/// vocabulary in [`IngestRequestOutcome`], because a batch has a partial
/// outcome the tri-state here cannot express.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestOutcome {
    /// Successful completion.
    Success,
    /// A completed PDP deny decision (the request was authorized against and denied).
    Denied,
    /// Any non-deny failure completion.
    Error,
}

impl RequestOutcome {
    /// The bounded `outcome` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Denied => "denied",
            Self::Error => "error",
        }
    }
}

/// `outcome` label for `uc_ingestion_requests_total` — maps to the HTTP
/// `200` / `207` / request-wide-`Problem` tri-state.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestRequestOutcome {
    /// All records accepted (HTTP 200).
    Accepted,
    /// At least one per-record rejection (HTTP 207).
    Partial,
    /// Request-wide rejection (a `Problem` envelope).
    Rejected,
}

impl IngestRequestOutcome {
    /// The bounded `outcome` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Partial => "partial",
            Self::Rejected => "rejected",
        }
    }
}

/// `error_category` label for `uc_ingestion_requests_total` (request-wide).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestRequestErrorCategory {
    /// `outcome` was `accepted` / `partial` (no request-wide reason).
    None,
    /// Reserved/defensive — unauthenticated calls are rejected upstream.
    MissingSecurityContext,
    /// Whole-request plugin transport / readiness / persistence failure.
    PluginError,
}

impl IngestRequestErrorCategory {
    /// The bounded `error_category` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::MissingSecurityContext => "missing_security_context",
            Self::PluginError => "plugin_error",
        }
    }
}

/// `outcome` label for `uc_ingestion_records_total` (per-record). `Duplicate`
/// is reserved — the Method 1/2 SPI returns `Ok` indistinguishably for a
/// fresh persist and an exact-equality replay, so it is never emitted in v1.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    /// Record accepted (fresh persist or silent-absorb idempotent replay).
    Accepted,
    /// Reserved — not emitted in v1 (no SPI dedup signal).
    Duplicate,
    /// Per-record rejection.
    Rejected,
}

impl RecordOutcome {
    /// The bounded `outcome` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Duplicate => "duplicate",
            Self::Rejected => "rejected",
        }
    }
}

// The `entry_type` label for `uc_ingestion_records_total` is
// [`usage_collector_sdk::EntryType`] itself, not a second enum declared here.
// The two would be the same closed pair spelled twice, and the label value a
// dashboard groups by has to be the value the wire and the `$filter` surface
// carry — one vocabulary, one spelling, and `EntryType::as_str` is already
// the function that produces it. Nothing about that type is
// infrastructure-shaped, so importing it costs the port no layer violation:
// the domain already depends on the SDK for every shape it names.

/// `error_category` label for `uc_ingestion_records_total` (per-record).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordErrorCategory {
    /// `outcome` was `accepted` (no per-record reason).
    None,
    /// PDP deny for this record's attribution tuple.
    Authz,
    /// The referenced `gts_type_id` does not resolve to a usable declaration
    /// (the Type Resolver's `DeclarationNotFound`).
    UnknownUsageType,
    /// A submission rejected against its meter's declaration or the shape
    /// rules of the ingest path — a covered period the identity derivation
    /// refuses, and the metadata-adjacent validation reasons that are not
    /// the closed-shape or size-cap pair below.
    ///
    /// It also carries **one** invalidation rule, which
    /// [`Self::InvalidationRule`] therefore does not: an `invalidates` that
    /// resolves to nothing. That rejection is a `NotFound`, which carries
    /// no typed reason, so nothing but `detail` prose separates it from an
    /// entry id that resolves to nothing — and a plugin's own
    /// `UsageRecordNotFound` reaches the same arm. See
    /// `crate::domain::service`'s `classify_record_error`.
    SemanticsViolation,
    /// An invalidation rejected against the entry it withdraws. DESIGN
    /// §3.11.5 gives it "the copy, reference and at-most-one rules alone";
    /// this carries the copy and at-most-one rules whole, and the
    /// **typed half** of the reference rule — a target that is itself an
    /// invalidation, and a half-shaped reference from the REST fold point.
    /// The untyped half, a reference resolving to nothing, is on
    /// [`Self::SemanticsViolation`] for the reason stated there, so this
    /// series under-counts the reference rule by exactly that condition.
    ///
    /// A period-bound rejection is not an invalidation rule for either
    /// entry type, because the bound belongs to the path rather than to the
    /// withdrawal.
    InvalidationRule,
    /// Metadata size-cap or closed-shape rejection (the sole metadata category).
    MetadataSize,
    /// Same-key canonical-field mismatch.
    IdempotencyConflict,
    /// Per-record plugin transport / readiness / persistence fault.
    PluginError,
}

impl RecordErrorCategory {
    /// The bounded `error_category` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Authz => "authz",
            Self::UnknownUsageType => "unknown_usage_type",
            Self::SemanticsViolation => "semantics_violation",
            Self::InvalidationRule => "invalidation_rule",
            Self::MetadataSize => "metadata_size",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::PluginError => "plugin_error",
        }
    }
}

/// `query_kind` label for the query-gateway instruments.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    /// Aggregated query (`query_aggregated_usage_records`).
    Aggregated,
    /// Raw listing (`list_usage_records`).
    Raw,
}

impl QueryKind {
    /// The bounded `query_kind` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aggregated => "aggregated",
            Self::Raw => "raw",
        }
    }
}

/// `error_category` label for `uc_query_requests_total`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryErrorCategory {
    /// `outcome` was `success`.
    None,
    /// Reserved/defensive — rejected upstream / unreachable on SDK.
    MissingSecurityContext,
    /// PDP deny (or empty-constraint fail-closed) / substrate-unreachable authz exit.
    Authz,
    /// The referenced `gts_type_id` does not resolve to a usable declaration
    /// (the Type Resolver's `DeclarationNotFound`).
    UnknownUsageType,
    /// Cursor decode failure (REST-handler boundary; reserved at this seam).
    CursorDecode,
    /// Cursor `$orderby` mismatch (REST-handler boundary; reserved at this seam).
    OrderMismatch,
    /// A continuation refused because the cursor was minted over a
    /// different query than the request carrying it — a changed `$filter`
    /// or a changed typed parameter (`gts_type_id`, the read range, a
    /// `metadata.<key>` filter). Emitted at the service seam, which owns
    /// that comparison on every surface.
    FilterMismatch,
    /// The catch-all for a query the gateway refused on its own surface: a
    /// `$filter` naming a field reserved to a typed parameter, an
    /// undeclared `group_by` / `metadata_filter` key, an aggregate result
    /// over the declared bucket cap, or an `$orderby` that cannot be
    /// floored into a keyset (mixed sort directions, or a key that is not
    /// a mandatory record attribute — both reachable in the domain since
    /// the keyset floor moved there). The mandatory read range never lands
    /// here: it is a typed parameter validated at the edge, before the
    /// service is entered.
    ///
    /// Wider than its name, and knowingly so. A continuation whose bound
    /// order is not a keyset folds in too, for want of a category that
    /// describes it — see `classify_query_result`, where that case has an
    /// explicit arm.
    QueryBudget,
    /// Plugin transport / readiness / backend failure.
    PluginError,
}

impl QueryErrorCategory {
    /// The bounded `error_category` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::MissingSecurityContext => "missing_security_context",
            Self::Authz => "authz",
            Self::UnknownUsageType => "unknown_usage_type",
            Self::CursorDecode => "cursor_decode",
            Self::OrderMismatch => "order_mismatch",
            Self::FilterMismatch => "filter_mismatch",
            Self::QueryBudget => "query_budget",
            Self::PluginError => "plugin_error",
        }
    }
}

/// Outcome of one [`crate::domain::type_resolver::TypeResolver::resolve`]
/// call, for the failure and staleness instruments DESIGN §3.11.5 requires.
/// Replaces the deleted catalog-lifecycle instruments
/// (`uc_usage_type_requests_total`, `uc_usage_types`): the catalog surface
/// is gone, and every fold / unit / metadata-surface read now goes through
/// the Type Resolver instead.
///
/// DESIGN §3.11.5 documents `uc_type_resolution_total{result}` with a
/// five-value `result` set: `cache_hit`, `cache_miss`, `served_stale`,
/// `unresolved`, `registry_error`, plus `restored` — a mirror-table restore
/// path (§3.7 / `cpt-cf-usage-collector-adr-declaration-rehydration`)
/// this gear does not implement yet, so no variant
/// below emits it. This enum names the five values `TypeResolver::resolve`
/// / `populate` (`domain/type_resolver/mod.rs`) actually emit:
///
/// - a fresh cache hit ([`Self::CacheHit`]);
/// - a successful fetch-and-parse from `types-registry`, whether the key
///   was cold or past its TTL ([`Self::CacheMiss`]);
/// - a fetch that failed with a stale cached entry to fall back on
///   ([`Self::ServedStale`]);
/// - a definite not-found, **or** an incomplete declaration — a schema that
///   fetched fine but does not carry what a meter needs (a missing trait, an
///   unserved fold) ([`Self::Unresolved`]). The two share one variant
///   deliberately: in both, `types-registry` answered fine and the
///   *declaration* is the problem, not the registry — the distinction the
///   alert `PromQL` (DESIGN §3.11.5) keys on to tell "a caller asked for a
///   type that does not exist" apart from "the registry is unreachable";
/// - a fetch that failed (registry unreachable, timed out, ...) with
///   nothing cached to fall back on ([`Self::RegistryError`]). Unlike
///   [`Self::Unresolved`], here the registry itself failed to answer — no
///   verdict on the type was ever reached.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeResolutionOutcome {
    /// Served from a fresh cache entry.
    CacheHit,
    /// Fetched from `types-registry` (cold key or past-TTL refresh).
    CacheMiss,
    /// Served from a cached entry past its TTL because the registry failed.
    ServedStale,
    /// The type does not resolve: a definite not-found, or a declaration
    /// that fetched fine but is incomplete. The registry answered fine; the
    /// declaration is simply unusable. The operation is rejected.
    Unresolved,
    /// The fetch itself failed (registry unreachable, timed out, ...) and
    /// nothing cached exists to serve instead — no verdict on the type was
    /// reached. The operation is rejected.
    RegistryError,
}

impl TypeResolutionOutcome {
    /// The bounded `result` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CacheHit => "cache_hit",
            Self::CacheMiss => "cache_miss",
            Self::ServedStale => "served_stale",
            Self::Unresolved => "unresolved",
            Self::RegistryError => "registry_error",
        }
    }
}

/// Output port for recording usage-collector operational metrics.
///
/// Phase 1 declares the foundation-owned instruments — the plugin-host set
/// (`uc_plugin_ready`, `uc_plugin_accept_errors_total`,
/// `uc_plugin_call_duration_seconds`) and the PDP-helper set
/// (`uc_pdp_ready`, `uc_pdp_failures_total`, `uc_pdp_duration_seconds`,
/// `uc_authz_decisions_total`). Phase 2 adds the per-component gateway
/// instruments (ingestion, query) plus the Type Resolver
/// instrument that replaced the deleted usage-type catalog counters.
pub trait UsageCollectorMetrics: Send + Sync {
    /// `uc_pdp_ready` gauge — set to `1` while the `authz-resolver` client is
    /// bound in the bootstrap-constructed `PolicyEnforcer`, `0` otherwise.
    fn set_pdp_ready(&self, ready: bool);

    /// Observe a completed PDP authorization: `uc_pdp_duration_seconds{operation}`
    /// plus `uc_authz_decisions_total{operation, decision}`.
    ///
    /// `decision` is the **effective** gear decision, not the raw
    /// `access_scope_with` return: a permit-with-constraints that the domain's
    /// post-permit gate rejects (cross-tenant attribution, an un-projectable row
    /// scope) is recorded as [`AuthzDecision::Deny`]. Exactly one of this method
    /// or [`Self::record_pdp_failure`] fires per authorization.
    fn record_pdp_decision(&self, op: PdpOp, decision: AuthzDecision, seconds: f64);

    /// Observe a PDP failure (transport / evaluation):
    /// `uc_pdp_duration_seconds{operation}` plus
    /// `uc_pdp_failures_total{operation, cause}`. A failure completion is
    /// still a completion, so the duration is observed.
    fn record_pdp_failure(&self, op: PdpOp, cause: PdpFailureCause, seconds: f64);

    /// `uc_plugin_ready` gauge — set to `1` iff the active plugin binding is
    /// resolved structurally (selector cached AND scoped client registered).
    fn set_plugin_ready(&self, ready: bool);

    /// Observe a completed Plugin SPI dispatch (success or error):
    /// `uc_plugin_call_duration_seconds{operation}`.
    fn record_plugin_call(&self, op: PluginOp, seconds: f64);

    /// Increment `uc_plugin_accept_errors_total{operation, error_category}` —
    /// only for structural-unready short-circuits and backend-classified
    /// faults, never for deterministic domain-typed plugin variants.
    fn record_plugin_accept_error(&self, op: PluginOp, category: PluginErrorCategory);

    // ── Ingestion gateway (usage-emission) ──

    /// Observe `uc_ingestion_batch_size` — one observation per received batch
    /// submission, before per-record processing.
    fn observe_ingestion_batch_size(&self, size: u64);

    /// Observe `uc_ingestion_duration_seconds{origin}` — one per completed
    /// ingestion request (single-emit call or batch submission). The label
    /// separates the bulk-import latency profile from the live path's,
    /// which the live-path p95 budget depends on not being averaged
    /// together with a catch-up job's.
    fn observe_ingestion_duration(&self, seconds: f64, origin: RecordOrigin);

    /// Observe `uc_record_metadata_bytes` — one per submitted record that
    /// carries metadata (recorded before the size-cap comparison).
    fn observe_record_metadata_bytes(&self, bytes: u64);

    /// Increment
    /// `uc_ingestion_records_total{outcome, entry_type, origin, error_category}`
    /// once per entry in a batch acknowledgement (and once for a single
    /// emit).
    ///
    /// `entry_type` carries the correction share and `origin` the backfill
    /// share — between them, what makes a withdrawal of closed history
    /// visible in the ingestion profile at all.
    fn record_ingestion_record(
        &self,
        outcome: RecordOutcome,
        entry_type: EntryType,
        origin: RecordOrigin,
        error_category: RecordErrorCategory,
    );

    /// Increment `uc_ingestion_requests_total{outcome, error_category}` — once
    /// per completed batch-submission request (not on the single-emit path).
    fn record_ingestion_request(
        &self,
        outcome: IngestRequestOutcome,
        error_category: IngestRequestErrorCategory,
    );

    // ── Query gateway (usage-query) ──

    /// Increment `uc_query_inflight{query_kind}` on query-gateway entry once
    /// authorization composes.
    fn query_inflight_inc(&self, kind: QueryKind);

    /// Decrement `uc_query_inflight{query_kind}` — only on exits that followed
    /// a matching [`Self::query_inflight_inc`].
    fn query_inflight_dec(&self, kind: QueryKind);

    /// Observe `uc_query_result_rows{query_kind}` with the page size / group
    /// count — recorded only on a successful query completion.
    fn observe_query_result_rows(&self, kind: QueryKind, rows: u64);

    /// Observe `uc_query_duration_seconds{query_kind}` plus increment
    /// `uc_query_requests_total{query_kind, outcome, error_category}` — once
    /// per completed query attempt.
    fn record_query_request(
        &self,
        kind: QueryKind,
        outcome: RequestOutcome,
        error_category: QueryErrorCategory,
        seconds: f64,
    );

    // ── Type Resolver (usage-type-lifecycle successor) ──

    /// Increment `uc_type_resolution_total{result}` once per
    /// [`crate::domain::type_resolver::TypeResolver::resolve`] call.
    fn record_type_resolution(&self, outcome: TypeResolutionOutcome);
}

/// No-op implementation for tests and pre-bootstrap contexts.
///
/// A [`crate::infra::metrics::UcMetricsMeter`] bound to the process-global
/// `NoopMeterProvider` is already effectively inert, but this ZST avoids
/// constructing any meter at all where metrics are irrelevant.
#[domain_model]
#[allow(dead_code)] // constructed only by test / pre-init builds
pub struct NoopMetrics;

impl UsageCollectorMetrics for NoopMetrics {
    fn set_pdp_ready(&self, _: bool) {}
    fn record_pdp_decision(&self, _: PdpOp, _: AuthzDecision, _: f64) {}
    fn record_pdp_failure(&self, _: PdpOp, _: PdpFailureCause, _: f64) {}
    fn set_plugin_ready(&self, _: bool) {}
    fn record_plugin_call(&self, _: PluginOp, _: f64) {}
    fn record_plugin_accept_error(&self, _: PluginOp, _: PluginErrorCategory) {}
    fn observe_ingestion_batch_size(&self, _: u64) {}
    fn observe_ingestion_duration(&self, _: f64, _: RecordOrigin) {}
    fn observe_record_metadata_bytes(&self, _: u64) {}
    fn record_ingestion_record(
        &self,
        _: RecordOutcome,
        _: EntryType,
        _: RecordOrigin,
        _: RecordErrorCategory,
    ) {
    }
    fn record_ingestion_request(&self, _: IngestRequestOutcome, _: IngestRequestErrorCategory) {}
    fn query_inflight_inc(&self, _: QueryKind) {}
    fn query_inflight_dec(&self, _: QueryKind) {}
    fn observe_query_result_rows(&self, _: QueryKind, _: u64) {}
    fn record_query_request(&self, _: QueryKind, _: RequestOutcome, _: QueryErrorCategory, _: f64) {
    }
    fn record_type_resolution(&self, _: TypeResolutionOutcome) {}
}
