//! Output port for recording usage-collector operational metrics.
//!
//! Implementations live in [`crate::v2::infra::metrics`] (OpenTelemetry
//! instruments declared on a scoped `Meter` from `ToolKit`'s global
//! `SdkMeterProvider`). Domain code depends only on this trait and the
//! label enums below — it has no knowledge of `OTel`, honoring the DDD-light
//! layer boundary (domain must not depend on infra / transport types).
//!
//! ## Naming contract
//!
//! usage-collector bakes the **full literal** Prometheus name into the
//! instrument (counters carry `_total`, duration histograms carry `_seconds`)
//! and sets **no** `.with_unit(...)` hint, so the rendered name is identical
//! whether the downstream `OTel` collector runs with `add_metric_suffixes` on
//! or off.
//!
//! ## Label cardinality
//!
//! Every label is a closed, enumerated value set — modeled here as `enum`s
//! with `const fn as_str()`. Unbounded identifiers (`tenant_id`,
//! `resource_id`, `gts_id`, `trace_id`, idempotency keys) MUST NOT appear
//! as metric labels; they belong in structured logs and traces.

use toolkit_macros::domain_model;
use usage_collector_sdk::v2::{EntryType, RecordOrigin};

/// Label key constants shared by the instrument families below.
// @cpt-dod:cpt-cf-usage-collector-dod-metric-label-cardinality-bound:p2
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
/// `uc_authz_decisions_total`) — the usage-record gateway set.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdpOp {
    /// Live usage-record ingestion (single + batch emit) — see
    /// [`Self::Backfill`] for the import route's own label.
    Ingest,
    /// Bulk historical import — the backfill route's own ingestion.
    ///
    /// A **route** label, not the PEP verb
    /// `usage_record::actions::BACKFILL` whose string it shares: the label
    /// follows the entry point, the verb follows the covered period. Every entry
    /// of a backfill batch carries this label whichever verb it was authorized
    /// against, which keeps a bulk import's PDP latency and denial rate
    /// separable from live emission's.
    Backfill,
    /// Raw (non-aggregated) usage-record listing.
    QueryRaw,
    /// Aggregated usage-record query.
    QueryAggregated,
    /// Read a single usage record by id.
    GetRecord,
    /// Read a page of the usage feed.
    ReadFeed,
    /// Read per-scope reconciliation metadata.
    Reconciliation,
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
            Self::ReadFeed => "read_feed",
            Self::Reconciliation => "reconciliation",
        }
    }
}

/// `operation` label for the plugin-host instruments
/// (`uc_plugin_call_duration_seconds`, `uc_plugin_accept_errors_total`) — the
/// method names [`usage_collector_sdk::v2::UsageCollectorPluginV2`] declares.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginOp {
    /// `create_usage_records`.
    CreateUsageRecords,
    /// `query_aggregated_usage_records`.
    QueryAggregatedUsageRecords,
    /// `list_usage_records`.
    ListUsageRecords,
    /// `get_usage_record`.
    GetUsageRecord,
    /// `read_feed_page`.
    ReadFeedPage,
    /// `get_reconciliation_metadata`.
    GetReconciliationMetadata,
}

impl PluginOp {
    /// The bounded `operation` label value (verbatim SPI method name).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CreateUsageRecords => "create_usage_records",
            Self::QueryAggregatedUsageRecords => "query_aggregated_usage_records",
            Self::ListUsageRecords => "list_usage_records",
            Self::GetUsageRecord => "get_usage_record",
            Self::ReadFeedPage => "read_feed_page",
            Self::GetReconciliationMetadata => "get_reconciliation_metadata",
        }
    }

    /// Every variant, so a consumer deriving a per-operation set cannot
    /// silently miss one.
    pub const ALL: &'static [Self] = &[
        Self::CreateUsageRecords,
        Self::QueryAggregatedUsageRecords,
        Self::ListUsageRecords,
        Self::GetUsageRecord,
        Self::ReadFeedPage,
        Self::GetReconciliationMetadata,
    ];
}

/// `cause` label for `uc_pdp_failures_total`.
///
/// **v1 mapping:** the bootstrap-bound `PolicyEnforcer` surfaces
/// `AuthZResolverError` (via `EnforcerError::EvaluationFailed`), which carries
/// no timeout discriminator, so every PDP failure maps to
/// [`PdpFailureCause::Unreachable`]. [`PdpFailureCause::Timeout`] is reserved
/// for a future host-side PDP-dispatch deadline.
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
    /// Host-side dispatch deadline expiry.
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
/// successful return, `denied` on a completed PDP deny, `error` otherwise).
/// `uc_ingestion_requests_total` is also request-scoped but keeps its own
/// vocabulary in [`IngestRequestOutcome`], because a batch has a partial
/// outcome this one cannot express.
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
    /// Request-wide validation rejection: the structural
    /// `1..=max_batch_records` entry cap (empty or over-cap). The per-record
    /// vocabulary ([`RecordErrorCategory`]) spells its own validation-shaped
    /// rejections `semantics_violation` / `metadata_size`.
    Validation,
    /// Request-wide rejection by the per-subject ingestion quota
    /// (`crate::v2::domain::quota`). The submission is refused whole before any
    /// entry is validated, so — unlike every per-record category — it can never
    /// coexist with an `accepted` or `partial` outcome.
    ///
    /// It appears on this counter and **nowhere else**: a quota rejection does
    /// not touch the per-record counter at all (its required `entry_type` label
    /// is caller-supplied and unvalidated at the charge point), and the
    /// throttled volume rides the unlabelled
    /// `uc_ingestion_quota_rejections_total`.
    Quota,
}

impl IngestRequestErrorCategory {
    /// The bounded `error_category` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::MissingSecurityContext => "missing_security_context",
            Self::PluginError => "plugin_error",
            Self::Validation => "validation",
            Self::Quota => "quota",
        }
    }
}

/// `outcome` label for `uc_ingestion_records_total` (per-record). `Duplicate`
/// is reserved — the SPI returns `Ok` indistinguishably for a fresh persist and
/// an exact-equality replay, so it is never emitted in v1.
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
// [`usage_collector_sdk::v2::EntryType`] itself, not a second enum declared
// here: the label value a dashboard groups by has to be the value the wire and
// the `$filter` surface carry, and `EntryType::as_str` already produces it.

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
    /// It also carries the residual case of a lookup that resolved to nothing
    /// without being an invalidation rule: an entry id the store does not hold,
    /// raised from some SPI call other than the invalidation-target lookup
    /// (which counts as [`Self::InvalidationRule`]). The two share the
    /// `NotFound` variant and are separated by the typed `NotFoundReason`, not
    /// by `detail` prose — see `crate::v2::domain::service`'s
    /// `classify_record_error`.
    SemanticsViolation,
    /// An invalidation rejected against the entry it withdraws, scoped to the
    /// target-resolution and copy rules alone: a target that resolves to
    /// nothing (a `NotFound`, discriminated by
    /// `usage_collector_sdk::v2::NotFoundReason`), a target not yet converged,
    /// and an unfaithful copy.
    ///
    /// **At-most-one-invalidation is not among them**: a second invalidation
    /// rejected as already invalidated is [`Self::IdempotencyConflict`], like
    /// any dedup conflict. A target that is itself an invalidation is
    /// unreachable — the target's identifier is derived with
    /// `entry_type = record` — and a store that answers one anyway is a
    /// host-invariant breach counting as a plugin fault. A period-bound
    /// rejection belongs to the path rather than to the withdrawal.
    InvalidationRule,
    /// Metadata size-cap or closed-shape rejection (the sole metadata category).
    MetadataSize,
    /// Same-key canonical-field mismatch, **or** a second invalidation of a
    /// record rejected as already invalidated — like any dedup conflict, not
    /// under [`Self::InvalidationRule`].
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
    /// Per-scope reconciliation read (`get_reconciliation_metadata`).
    ///
    /// **Reaches `uc_query_requests_total` alone**: each query instrument has
    /// its own `query_kind` vocabulary, and `uc_query_duration_seconds`,
    /// `uc_query_result_rows` and `uc_query_inflight` do not admit
    /// `reconciliation`.
    Reconciliation,
    /// By-id point lookup (`get_usage_record`).
    ///
    /// Reaches `uc_query_requests_total` and `uc_query_duration_seconds` alone:
    /// a one-shot per-id read returns a single row rather than a page, and has
    /// no in-flight concurrency to track (see [`Self::admits_inflight_gauge`]).
    Point,
}

impl QueryKind {
    /// The bounded `query_kind` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aggregated => "aggregated",
            Self::Raw => "raw",
            Self::Reconciliation => "reconciliation",
            Self::Point => "point",
        }
    }

    /// Whether the `uc_query_duration_seconds` row admits this kind.
    ///
    /// [`UsageCollectorMetrics::record_query_request`] reads this before writing
    /// the duration sample, which lets one method serve every [`QueryKind`]
    /// without emitting a label value a narrower-vocabulary instrument does not
    /// admit. Exhaustive on purpose, with no wildcard arm, so adding a variant
    /// is a compile error until its admission is a deliberate line here.
    #[must_use]
    pub const fn admits_duration_sample(self) -> bool {
        match self {
            Self::Aggregated | Self::Raw | Self::Point => true,
            Self::Reconciliation => false,
        }
    }

    /// Whether the `uc_query_result_rows` row admits this kind.
    ///
    /// Narrower than the duration row: this one does **not** admit `Point`, a
    /// point lookup returning one record rather than a page. Read by
    /// [`UsageCollectorMetrics::observe_query_result_rows`], the same guard
    /// shape as [`Self::admits_duration_sample`].
    #[must_use]
    pub const fn admits_result_rows_sample(self) -> bool {
        match self {
            Self::Aggregated | Self::Raw => true,
            Self::Reconciliation | Self::Point => false,
        }
    }

    /// Whether the `uc_query_inflight` row admits this kind.
    ///
    /// Admits the same kinds as [`Self::admits_result_rows_sample`], and for
    /// the same reason: a one-shot per-scope read has no in-flight concurrency
    /// to track. [`UsageCollectorMetrics::query_inflight_inc`] and
    /// [`UsageCollectorMetrics::query_inflight_dec`] both read this before
    /// touching the gauge, so an increment this guard skips is never paired with
    /// a decrement that would underflow it.
    #[must_use]
    pub const fn admits_inflight_gauge(self) -> bool {
        match self {
            Self::Aggregated | Self::Raw => true,
            Self::Reconciliation | Self::Point => false,
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
    /// The by-id point lookup found no row: the id names nothing, or it names a
    /// row outside the caller's compiled scope. The plugin reports both as
    /// `UsageRecordNotFound` and this gear does not separate them, because
    /// separating them would make the surface an existence oracle.
    ///
    /// **A PDP deny is deliberately *not* here**, though the caller reads the
    /// identical `NotFound`: `get_usage_record` classifies the decision before
    /// `collapse_deny_to_not_found` discards it, so a deny labels
    /// `(denied, authz)` as on every other query surface.
    RecordNotFound,
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
    /// `$filter` naming a field reserved to a typed parameter, an undeclared
    /// `group_by` / `metadata_filter` key, an aggregate result over the declared
    /// bucket cap, or an `$orderby` that cannot be floored into a keyset. The
    /// mandatory read range never lands here: it is a typed parameter validated
    /// at the edge, before the service is entered.
    ///
    /// Wider than its name, knowingly: a continuation whose bound order is not
    /// a keyset folds in too, for want of a category that describes it.
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
            Self::RecordNotFound => "record_not_found",
            Self::CursorDecode => "cursor_decode",
            Self::OrderMismatch => "order_mismatch",
            Self::FilterMismatch => "filter_mismatch",
            Self::QueryBudget => "query_budget",
            Self::PluginError => "plugin_error",
        }
    }
}

/// `error_category` label for `uc_feed_requests_total`. The `outcome` label the
/// same counter carries is [`RequestOutcome`], reused rather than respelled: a
/// feed page completes `success` / `denied` / `error` exactly as a query does.
///
/// A **PDP outage** shares `ServiceUnavailable` with a plugin fault at this
/// seam, so a feed request failed by an unreachable `authz-resolver` counts
/// under [`Self::PluginError`] rather than [`Self::Authz`] (reserved for a
/// *completed* decision). An operator must not read a `plugin_error` spike here
/// as a storage-plugin verdict; `uc_pdp_failures_total` is the authoritative
/// PDP-unavailability signal.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedErrorCategory {
    /// `outcome` was `success`.
    None,
    /// PDP deny, or the empty-constraint fail-closed exit.
    Authz,
    /// A cursor or `until` token the gateway refused: malformed, not a feed
    /// cursor, bound to a different subscription, or naming an unusable
    /// position.
    CursorDecode,
    /// The plugin refused the cursor because retention has already removed an
    /// entry the continuation would have to deliver — the replay-refusal
    /// signal, whose rate the alerting table watches.
    CursorBeyondRetention,
    /// A `limit` or subscription breadth outside its published bound,
    /// refused behind the service (an in-process caller that never reached
    /// the REST edge's own validation). A caller-surface argument
    /// rejection, not a plugin fault.
    ArgumentRejected,
    /// Plugin transport, readiness or backend failure — **and a PDP
    /// outage**, which shares the `ServiceUnavailable` variant with them and
    /// so cannot be told apart at this seam. See the enum's own doc for the
    /// full list of what folds in here and where the authoritative
    /// PDP-unavailability signal lives.
    PluginError,
}

impl FeedErrorCategory {
    /// The bounded `error_category` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Authz => "authz",
            Self::CursorDecode => "cursor_decode",
            Self::CursorBeyondRetention => "cursor_beyond_retention",
            Self::ArgumentRejected => "argument_rejected",
            Self::PluginError => "plugin_error",
        }
    }
}

/// Outcome of one [`crate::v2::domain::type_resolver::TypeResolver::resolve`]
/// call — the `result` label set for `uc_type_resolution_total{result}`, every
/// value of which this gear emits. All but [`Self::Restored`] come from
/// `TypeResolver::resolve` / `populate`; `Restored` is `populate`'s restore arm
/// against the mirror table.
///
/// [`Self::Unresolved`] covers a definite not-found **and** an incomplete
/// declaration, deliberately: in both, `types-registry` answered fine and the
/// *declaration* is the problem — the distinction the alert `PromQL` keys on to
/// tell "a caller asked for a type that does not exist" apart from "the registry
/// is unreachable", which is [`Self::RegistryError`].
// @cpt-dod:cpt-cf-usage-collector-dod-resolution-telemetry:p1
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeResolutionOutcome {
    /// Served from a fresh cache entry.
    CacheHit,
    /// Fetched from `types-registry` (cold key or past-TTL refresh).
    CacheMiss,
    /// Served from a cached entry past its TTL because the registry failed.
    ServedStale,
    /// Served from a document the declaration mirror held, after
    /// `types-registry` answered a definite not-found and the resolver
    /// registered the stored document back. A sustained rate here means the
    /// registry is losing declarations.
    Restored,
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
            Self::Restored => "restored",
            Self::Unresolved => "unresolved",
            Self::RegistryError => "registry_error",
        }
    }
}

/// Output port for recording usage-collector operational metrics.
///
/// The foundation-owned instruments are the plugin-host set (`uc_plugin_ready`,
/// `uc_plugin_accept_errors_total`, `uc_plugin_call_duration_seconds`) and the
/// PDP-helper set (`uc_pdp_ready`, `uc_pdp_failures_total`,
/// `uc_pdp_duration_seconds`, `uc_authz_decisions_total`). Beside them sit the
/// per-component gateway instruments (ingestion, query) and the Type Resolver's.
// @cpt-algo:cpt-cf-usage-collector-algo-metric-emission-binding:p2
pub trait UsageCollectorMetrics: Send + Sync {
    /// `uc_pdp_ready` gauge — set to `1` while the `authz-resolver` client is
    /// bound in the bootstrap-constructed `PolicyEnforcer`, `0` otherwise.
    fn set_pdp_ready(&self, ready: bool);

    /// Observe a completed PDP authorization: `uc_pdp_duration_seconds{operation}`
    /// plus `uc_authz_decisions_total{operation, decision}`.
    ///
    /// `decision` is the **effective** gear decision, not the raw
    /// `access_scope_with` return: a permit-with-constraints the domain's
    /// post-permit gate rejects is recorded as [`AuthzDecision::Deny`]. Exactly
    /// one of this method or [`Self::record_pdp_failure`] fires per
    /// authorization.
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
    // @cpt-algo:cpt-cf-usage-collector-algo-slo-latency-attribution:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-ingestion-telemetry:p1
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
    // @cpt-dod:cpt-cf-usage-collector-dod-ingestion-telemetry:p1
    fn record_ingestion_record(
        &self,
        outcome: RecordOutcome,
        entry_type: EntryType,
        origin: RecordOrigin,
        error_category: RecordErrorCategory,
    );

    /// Increment `uc_ingestion_requests_total{outcome, error_category}` — once
    /// per completed submission request.
    // @cpt-dod:cpt-cf-usage-collector-dod-ingestion-telemetry:p1
    fn record_ingestion_request(
        &self,
        outcome: IngestRequestOutcome,
        error_category: IngestRequestErrorCategory,
    );

    /// Increment `uc_ingestion_quota_rejections_total` by `entries` — the
    /// **submitted entry count**, not one per rejected request.
    ///
    /// A per-request increment would make a 1-entry overrun and a 100-entry one
    /// read identically, which is the one thing this instrument exists to tell
    /// apart — `uc_ingestion_records_total` cannot carry throttled volume,
    /// because its required `entry_type` label is caller-supplied and not yet
    /// validated at the charge point.
    ///
    /// **Unlabelled**: the throttled caller's identity rides the structured log
    /// the service emits alongside this increment.
    // @cpt-algo:cpt-cf-usage-collector-algo-telemetry-label-admission:p2
    fn record_quota_rejection(&self, entries: u64);

    /// Set `uc_ingestion_quota_buckets_active` to `count` — the number of
    /// quota buckets currently resident on this replica.
    ///
    /// Watches map growth and confirms idle eviction is keeping up.
    /// Unlabelled, and per-replica like the state it measures: aggregate it with
    /// `max` or `last`, never `sum`.
    fn set_quota_buckets_active(&self, count: u64);

    // ── Query gateway (usage-query) ──

    /// Increment `uc_query_inflight{query_kind}` on query-gateway entry once
    /// authorization composes — when [`QueryKind::admits_inflight_gauge`]
    /// holds for `kind`; a no-op otherwise, since DESIGN §3.11.5 admits
    /// `aggregated`, `raw` on this row alone.
    fn query_inflight_inc(&self, kind: QueryKind);

    /// Decrement `uc_query_inflight{query_kind}` — only on exits that followed
    /// a matching [`Self::query_inflight_inc`]. Gated by
    /// [`QueryKind::admits_inflight_gauge`] identically to the increment, so
    /// a `kind` the gauge never incremented for is never decremented for
    /// either.
    fn query_inflight_dec(&self, kind: QueryKind);

    /// Observe `uc_query_result_rows{query_kind}` with the page size / group
    /// count — recorded only on a successful query completion, and only when
    /// [`QueryKind::admits_result_rows_sample`] holds for `kind`.
    fn observe_query_result_rows(&self, kind: QueryKind, rows: u64);

    /// Increment `uc_query_requests_total{query_kind, outcome,
    /// error_category}` — once per completed query attempt — plus, when
    /// [`QueryKind::admits_duration_sample`] holds for `kind`, observe
    /// `uc_query_duration_seconds{query_kind}` in the same call.
    ///
    /// The two instruments are sampled together, one completion per call, so
    /// they can never disagree about how many requests finished — except for
    /// the one `QueryKind` the duration row does not admit, where the
    /// implementation must skip the histogram write.
    // @cpt-algo:cpt-cf-usage-collector-algo-slo-latency-attribution:p1
    fn record_query_request(
        &self,
        kind: QueryKind,
        outcome: RequestOutcome,
        error_category: QueryErrorCategory,
        seconds: f64,
    );

    // ── Feed gateway (billing-usage-feed) ──

    /// Observe `uc_feed_page_duration_seconds` plus increment
    /// `uc_feed_requests_total{outcome, error_category}` — once per completed
    /// feed page request.
    ///
    /// The duration rides this method rather than taking one of its own,
    /// exactly as `uc_query_duration_seconds` rides
    /// [`Self::record_query_request`]: one completion, one call, so the
    /// counter and the histogram cannot disagree about how many requests
    /// finished.
    fn record_feed_request(
        &self,
        outcome: RequestOutcome,
        error_category: FeedErrorCategory,
        seconds: f64,
    );

    /// Observe `uc_feed_page_entries` — the entries served on one page.
    ///
    /// Recorded only on a successful completion, like
    /// [`Self::observe_query_result_rows`]: a failed read served no page, and
    /// an observation of `0` for it would be indistinguishable from the empty
    /// page a caught-up live reader legitimately receives.
    fn observe_feed_page_entries(&self, entries: u64);

    // ── Type Resolver (usage-type-lifecycle successor) ──

    /// Increment `uc_type_resolution_total{result}` once per
    /// [`crate::v2::domain::type_resolver::TypeResolver::resolve`] call.
    ///
    /// **Exactly once per resolution, on every path.** Every `CacheHit` site
    /// in `TypeResolver::resolve` returns immediately, and each arm of
    /// `TypeResolver::populate` records one outcome and only one. No path
    /// records twice and none records nothing.
    // @cpt-dod:cpt-cf-usage-collector-dod-resolution-telemetry:p1
    fn record_type_resolution(&self, outcome: TypeResolutionOutcome);

    /// `uc_resolved_types` — current entry count of the resolved-declaration
    /// cache.
    ///
    /// **Per-instance**: aggregate with `max` or `last`, never `sum`, every
    /// replica holding its own cache of the same platform-global declarations.
    /// Set imperatively rather than read by a collection callback, matching
    /// [`Self::set_quota_buckets_active`]: a callback would have to hold the
    /// resolver, which an output port must not.
    fn set_resolved_types(&self, count: u64);

    /// `uc_declaration_cache_age_seconds` — age of the oldest cached
    /// declaration since its **last successful refresh**. Backs the
    /// declaration-cache-staleness alert.
    ///
    /// `None` means the cache is empty, which is **not** age zero: a zero would
    /// read as perfectly fresh and satisfy the staleness alert forever. The
    /// adapter records nothing instead, so the series has no point for that
    /// interval.
    ///
    /// **A sampled gauge, not a continuously computed one**: the resolver
    /// publishes it on a successful fetch and on a stale serve past the TTL,
    /// never on a timer, so between samples the series reads fresher than
    /// reality. A **serving** gear samples at least once per TTL, so the alert
    /// still fires, just later. An **idle** gear — one no longer resolving any
    /// meter in its cache — samples not at all, so the gauge freezes and the
    /// alert never fires for it. The remedy, a periodic sampler independent of
    /// resolution, is unbuilt.
    // @cpt-dod:cpt-cf-usage-collector-dod-resolution-telemetry:p1
    fn set_declaration_cache_age_seconds(&self, age: Option<u64>);

    /// `uc_type_resolution_duration_seconds{result}` — how long one
    /// resolution took.
    ///
    /// This row's `result` label carries `cache_hit` and `cache_miss` alone,
    /// where the counter carries the whole [`TypeResolutionOutcome`] set: the
    /// histogram shows that a cache miss is a cold-path cost rather than a
    /// budget line, and the failure *rate* is already carried by
    /// `uc_type_resolution_total{result}`. **Do not map a failure onto
    /// `cache_miss` to close the gap** — that would put registry-timeout
    /// durations into the figure an operator reads as the cold-path cost.
    ///
    /// **The two label values are timed from different origins.**
    /// `TypeResolver::resolve` times a `cache_hit` from its own entry, which on
    /// the post-gate hit path includes however long the call waited behind the
    /// per-key single-flight gate for another resolution's fetch, while
    /// `TypeResolver::populate` times `cache_miss` from a later `Instant` taken
    /// once the gate is held. So on a cold start with N concurrent resolutions
    /// of one meter, N-1 fetch-cost samples land on `cache_hit`.
    // @cpt-dod:cpt-cf-usage-collector-dod-resolution-telemetry:p1
    fn observe_type_resolution_duration(&self, outcome: TypeResolutionOutcome, seconds: f64);

    /// Increment `uc_declaration_mirror_write_failures_total` — one
    /// declaration resolved but not mirrored.
    ///
    /// Each one is a type that a later registry restart will not restore;
    /// ingestion is unaffected. **Unlabelled.**
    ///
    /// **A failed mirror READ is not counted here.** This instrument's
    /// population is writes that did not land; a read failure may leave a
    /// perfectly good row behind, and counting it would tell an operator that
    /// types are failing to mirror when they are not.
    // @cpt-dod:cpt-cf-usage-collector-dod-resolution-telemetry:p1
    fn record_declaration_mirror_write_failure(&self);
}

/// No-op implementation for tests and pre-bootstrap contexts.
///
/// A [`crate::v2::infra::metrics::UcMetricsMeter`] bound to the process-global
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
    fn record_quota_rejection(&self, _: u64) {}
    fn set_quota_buckets_active(&self, _: u64) {}
    fn query_inflight_inc(&self, _: QueryKind) {}
    fn query_inflight_dec(&self, _: QueryKind) {}
    fn observe_query_result_rows(&self, _: QueryKind, _: u64) {}
    fn record_query_request(&self, _: QueryKind, _: RequestOutcome, _: QueryErrorCategory, _: f64) {
    }
    fn record_feed_request(&self, _: RequestOutcome, _: FeedErrorCategory, _: f64) {}
    fn observe_feed_page_entries(&self, _: u64) {}
    fn record_type_resolution(&self, _: TypeResolutionOutcome) {}
    fn set_resolved_types(&self, _: u64) {}
    fn set_declaration_cache_age_seconds(&self, _: Option<u64>) {}
    fn observe_type_resolution_duration(&self, _: TypeResolutionOutcome, _: f64) {}
    fn record_declaration_mirror_write_failure(&self) {}
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "metrics_tests.rs"]
mod metrics_tests;
