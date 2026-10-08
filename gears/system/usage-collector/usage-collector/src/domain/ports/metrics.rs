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
//! usage-collector bakes the **full literal** Prometheus name into the
//! instrument (counters carry `_total`, duration histograms carry `_seconds`)
//! and sets **no** `.with_unit(...)` hint, so the rendered name is identical
//! whether the downstream `OTel` collector runs with `add_metric_suffixes` on
//! or off. The concrete builder lives in the infra impl; this port names each
//! family in its method docs.
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
/// `uc_authz_decisions_total`) — the usage-record gateway set. There is no
/// usage-type catalog surface: every type declaration is owned by
/// `types-registry` and resolved through the Type Resolver, which is not a
/// PDP-enforcing component.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdpOp {
    /// Live usage-record ingestion (single + batch emit) — see
    /// [`Self::Backfill`] for the import route's own label.
    Ingest,
    /// Bulk historical import — the backfill route's own ingestion.
    ///
    /// A **route** label, not the PEP verb
    /// `usage_record::actions::BACKFILL` whose string it shares. The label
    /// follows the entry point; the verb follows the covered period. So a
    /// backfill-route entry whose period ends inside the configured window is
    /// labelled `operation="backfill"` here and authorized against `create`.
    /// Every entry of a backfill batch carries this label whichever verb it was
    /// authorized against, which keeps a bulk import's PDP latency and denial
    /// rate separable from live emission's.
    Backfill,
    /// Raw (non-aggregated) usage-record listing.
    QueryRaw,
    /// Aggregated usage-record query.
    QueryAggregated,
    /// Read a single usage record by id.
    GetRecord,
    /// Read a page of the usage feed.
    ReadFeed,
    /// Read per-scope reconciliation metadata (DESIGN §3.11.5 names
    /// `reconciliation` for both `uc_pdp_failures_total{operation}` and
    /// `uc_authz_decisions_total{operation}`).
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
/// method names [`usage_collector_sdk::UsageCollectorPluginV1`] declares.
/// Storage plugins are pure usage-record persistence: no usage-type catalog
/// method exists on that trait.
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
    ///
    /// Pinned by `plugin_op_all_lists_every_variant`, whose wildcard-free
    /// match makes a new SPI method a **compile** error rather than a silently
    /// unspanned dispatch — the shape `EntryType` / `RecordOrigin` use.
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
    /// Request-wide validation rejection: the structural `1..=max_batch_records`
    /// entry cap (empty or over-cap). DESIGN §3.11.5 declares `validation`
    /// among this counter's values and does not narrow it to per-entry
    /// semantics, so a whole-request structural refusal is one.
    ///
    /// **Not symmetric with [`Self::Quota`].** §3.11.5 declares `validation`
    /// on this counter's row *and* on `uc_ingestion_records_total`'s, whereas
    /// `quota` is declared on this counter's row alone — so a pin treating the
    /// two label values the same way is wrong about one of them. The per-record
    /// vocabulary ([`RecordErrorCategory`]) spells its own validation-shaped
    /// rejections `semantics_violation` / `metadata_size`, a **deliberately
    /// kept** divergence: DESIGN's declared set was widened to name both rather
    /// than renaming an already-emitted label value, which would silently empty
    /// every dashboard keyed on the old one while the counter kept reporting.
    Validation,
    /// Request-wide rejection by the per-subject ingestion quota (DESIGN §3.2,
    /// `crate::domain::quota`). The submission is refused whole before any entry
    /// is validated, so — unlike every per-record category — it can never
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
// [`usage_collector_sdk::EntryType`] itself, not a second enum declared here:
// the label value a dashboard groups by has to be the value the wire and the
// `$filter` surface carry, and `EntryType::as_str` already produces it.
// Importing it costs the port no layer violation — the domain already depends
// on the SDK for every shape it names.

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
    /// It also carries a lookup that resolved to nothing without being an
    /// invalidation rule: an entry id the store does not hold. That is the
    /// residual case, **not** the documented flow — a `UsageRecordNotFound`
    /// from the invalidation-target lookup counts as
    /// [`Self::InvalidationRule`], so an operator triaging a record-not-found
    /// during ingest looks there first. Only one raised from some other SPI
    /// call, which is a misbehaving plugin, lands here. The two share the
    /// `NotFound` variant and are separated by the typed `NotFoundReason`, not
    /// by `detail` prose. See `crate::domain::service`'s
    /// `classify_record_error`.
    SemanticsViolation,
    /// An invalidation rejected against the entry it withdraws. DESIGN §3.11.5
    /// scopes it to "the target-resolution and copy rules alone, a target not
    /// yet converged included": a target that resolves to nothing (a `NotFound`,
    /// discriminated by `usage_collector_sdk::NotFoundReason`), a target not yet
    /// converged, and an unfaithful copy.
    ///
    /// **At-most-one-invalidation is not among them**, and the same sentence
    /// says where it goes: "a second invalidation rejected as already
    /// invalidated is `idempotency_conflict`, like any dedup conflict".
    /// `classify_record_error` maps `ConflictOutcome::AlreadyInvalidated` to
    /// [`Self::IdempotencyConflict`] and `TargetNotConverged` here.
    ///
    /// A target that is itself an invalidation is not among them either, and
    /// not because it stopped counting here: the target's identifier is derived
    /// with `entry_type = record`, so a caller cannot reach the case at all
    /// (DESIGN §3.1, "No invalidation of an invalidation"). A store that answers
    /// one anyway is a host-invariant breach and counts as a plugin fault.
    ///
    /// A period-bound rejection is not an invalidation rule for either entry
    /// type: the bound belongs to the path rather than to the withdrawal.
    InvalidationRule,
    /// Metadata size-cap or closed-shape rejection (the sole metadata category).
    MetadataSize,
    /// Same-key canonical-field mismatch, **or** a second invalidation of
    /// a record rejected as already invalidated — DESIGN §3.11.5 groups
    /// the latter here explicitly ("like any dedup conflict"), not under
    /// [`Self::InvalidationRule`].
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
    /// **Reaches `uc_query_requests_total` alone.** DESIGN §3.11.5 gives each
    /// query instrument its own `query_kind` vocabulary, and only the request
    /// counter admits `reconciliation`: `uc_query_duration_seconds`,
    /// `uc_query_result_rows` and `uc_query_inflight` do not. The asymmetry is
    /// the document's rather than this path's — one scope is not rows, and the
    /// document declines to give this path a latency histogram.
    /// `each_query_instrument_emits_only_the_query_kinds_its_row_declares`
    /// holds the code to it.
    Reconciliation,
    /// By-id point lookup (`get_usage_record`).
    ///
    /// Reaches `uc_query_requests_total` and `uc_query_duration_seconds` alone.
    /// DESIGN §3.11.5 lists `point` on both those rows but not on
    /// `uc_query_result_rows` or `uc_query_inflight` — a one-shot per-id read
    /// returns a single row rather than a page, and has no in-flight concurrency
    /// to track (see [`Self::admits_inflight_gauge`]).
    /// `each_query_instrument_emits_only_the_query_kinds_its_row_declares`
    /// holds the code to it, same as [`Self::Reconciliation`].
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

    /// Whether DESIGN §3.11.5's `uc_query_duration_seconds` row admits this
    /// kind.
    ///
    /// `Self::Reconciliation` does not — not because reconciliation carries no
    /// latency budget of its own (§3.11.2 grants `raw` no sub-allocation either,
    /// and `raw` **is** admitted here), but simply because the row does not list
    /// it. [`UsageCollectorMetrics::record_query_request`] reads this before
    /// writing the duration sample, which lets one method serve every
    /// [`QueryKind`] without emitting a label value a narrower-vocabulary
    /// instrument does not admit.
    ///
    /// Exhaustive on purpose, with no wildcard arm: a negated single-arm match
    /// (`!matches!(self, Self::Reconciliation)`) would let a future `QueryKind`
    /// inherit `true` with no compiler-forced decision. The match below forces
    /// whoever adds a variant to consult §3.11.5's row first.
    #[must_use]
    pub const fn admits_duration_sample(self) -> bool {
        match self {
            Self::Aggregated | Self::Raw | Self::Point => true,
            Self::Reconciliation => false,
        }
    }

    /// Whether DESIGN §3.11.5's `uc_query_result_rows` row admits this kind.
    ///
    /// Narrower than the duration row: this one does **not** admit `Point`, a
    /// point lookup returning one record rather than a page.
    /// [`UsageCollectorMetrics::observe_query_result_rows`] reads this before
    /// recording, the same guard shape as [`Self::admits_duration_sample`], so a
    /// caller cannot put a label this row does not carry onto the instrument.
    /// The match stays exhaustive, so adding a variant is a compile error until
    /// its admission is a deliberate line here.
    #[must_use]
    pub const fn admits_result_rows_sample(self) -> bool {
        match self {
            Self::Aggregated | Self::Raw => true,
            Self::Reconciliation | Self::Point => false,
        }
    }

    /// Whether DESIGN §3.11.5's `uc_query_inflight` row admits this kind.
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
    /// `(denied, authz)` as on every other query surface. A counter aggregated
    /// over every caller leaks no per-id existence.
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
    /// Wider than its name, knowingly: a continuation whose bound order is not a
    /// keyset folds in too, for want of a category that describes it — see
    /// `classify_query_result`'s explicit arm.
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

/// `error_category` label for `uc_feed_requests_total` — DESIGN §3.11.5's
/// published inventory. The `outcome` label the same counter carries is
/// [`RequestOutcome`], reused rather than respelled: a feed page completes
/// `success` / `denied` / `error` exactly as a query does.
///
/// Two reachable cases the originally declared values could not name go in
/// **opposite** directions under the declare-or-fold rule (spec §7.1: *"declare
/// the emitted value when folding it into a declared one changes an alert's
/// firing or an operator's action; fold when it changes neither"*):
///
/// 1. A **request parameter outside its published bound** — a `limit` or
///    subscription breadth `crate::domain::feed` refuses behind the service, the
///    REST edge enforcing each bound of its own. **Declared** as
///    [`Self::ArgumentRejected`]: folding it into [`Self::PluginError`] told an
///    operator a caller-surface rejection was a plugin fault.
/// 2. A **PDP outage**, which shares `ServiceUnavailable` with a plugin fault at
///    this seam, so a feed request failed by an unreachable `authz-resolver`
///    counts under [`Self::PluginError`] rather than [`Self::Authz`] (reserved
///    for a *completed* decision). **Folded**: `uc_pdp_failures_total` carries
///    its own §3.11.6 alert row for this cause and is the authoritative
///    PDP-unavailability signal, so an operator's action is unchanged — but must
///    not read a `plugin_error` spike here as a storage-plugin verdict. No
///    §3.11 alert reads `plugin_error` on this counter, so the residual
///    imprecision costs no page.
///
/// Everything else the residual arm receives is an `Internal`, a
/// `ServiceUnavailable`, or a variant only a misbehaving plugin can produce on a
/// feed read. The one exception is unreachable in v1: `read_usage_feed`'s
/// `FeedStart` wildcard raises an `Internal` for a start mode admitted after
/// this gateway was written, which no category describes.
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
    /// entry the continuation would have to deliver. DESIGN §3.11.5 names
    /// this the replay-refusal signal, and §3.11's alerting table watches its
    /// rate.
    CursorBeyondRetention,
    /// A `limit` or subscription breadth outside its published bound,
    /// refused behind the service (an in-process caller that never reached
    /// the REST edge's own validation). A caller-surface argument
    /// rejection, not a plugin fault.
    ///
    /// **Deliberately not [`QueryErrorCategory::QueryBudget`]'s spelling.**
    /// Prometheus label values are scoped per metric, so reusing one across the
    /// two counters enables no cross-counter aggregation — and that name
    /// misdescribes this population: an operator reading
    /// `error_category="query_budget"` for a `limit` below the published floor
    /// would look for an over-budget query on a surface that serves no
    /// queries.
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

/// Outcome of one [`crate::domain::type_resolver::TypeResolver::resolve`]
/// call, for the failure and staleness instruments DESIGN §3.11.5 requires.
/// Replaces the deleted catalog-lifecycle instruments
/// (`uc_usage_type_requests_total`, `uc_usage_types`): the catalog surface
/// is gone, and every fold / unit / metadata-surface read now goes through
/// the Type Resolver instead.
///
/// The variants below are the `result` label set DESIGN §3.11.5 declares for
/// `uc_type_resolution_total{result}`, and this gear emits every one. All but
/// [`Self::Restored`] come from `TypeResolver::resolve` / `populate`;
/// `Restored` is `populate`'s restore arm against DESIGN §3.7's mirror table
/// (`cpt-cf-usage-collector-adr-declaration-rehydration`).
///
/// [`Self::Unresolved`] covers a definite not-found **and** an incomplete
/// declaration, deliberately: in both, `types-registry` answered fine and the
/// *declaration* is the problem — the distinction the alert `PromQL` keys on to
/// tell "a caller asked for a type that does not exist" apart from "the registry
/// is unreachable", which is [`Self::RegistryError`].
// @cpt-dod:cpt-cf-usage-collector-dod-resolution-telemetry:p1
//
// Realizes the LABEL-VOCABULARY half of this DoD's first MUST clause — the
// declared `result` values — and that half alone: they are this enum's
// variants, and `Self::as_str` is the only place their wire spellings exist.
// The clause's other half ("MUST emit one counter observation per resolution")
// is realized by `UsageCollectorMetrics::record_type_resolution`. Each
// remaining MUST clause is marked at the trait method that realizes it: the
// duration histogram on `observe_type_resolution_duration`, the mirror-write
// count on `record_declaration_mirror_write_failure`, and the oldest cached
// declaration's age on `set_declaration_cache_age_seconds`.
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
    /// registered the stored document back
    /// (`cpt-cf-usage-collector-adr-declaration-rehydration`, DESIGN §3.7).
    /// A sustained rate here means the registry is losing declarations.
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
/// per-component gateway instruments (ingestion, query) plus the Type Resolver
/// instrument that replaced the deleted usage-type catalog counters.
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
    ///
    /// Realizes `dod-ingestion-telemetry`'s "ingestion duration histogram
    /// labelled by origin" sentence's label-mandate half: the signature requires
    /// an `origin` on every call, so no implementor can observe a duration
    /// without it. The one-observation-per-request emission half belongs to the
    /// adapter — a bare trait declaration cannot be an emission seam, since
    /// `NoopMetrics` satisfies this signature with an empty body. See
    /// `UcMetricsMeter::observe_ingestion_duration`'s own marker.
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
    ///
    /// Realizes `dod-ingestion-telemetry`'s "one counter observation per entry,
    /// labelled with the outcome as accepted, duplicate or rejected, with the
    /// entry type, with the origin marker, and with an error category on a
    /// rejection" sentence's label-mandate half: the signature requires every
    /// one of those values on each call. The one-observation-per-entry emission
    /// half belongs to the adapter — see
    /// `UcMetricsMeter::record_ingestion_record`'s own marker.
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
    ///
    /// Realizes `dod-ingestion-telemetry`'s "one counter observation per
    /// submission request, labelled with a request-wide outcome and a
    /// request-wide error category" sentence's label-mandate half: the signature
    /// requires both values on every call. The emission half belongs to the
    /// adapter — see `UcMetricsMeter::record_ingestion_request`'s own marker.
    // @cpt-dod:cpt-cf-usage-collector-dod-ingestion-telemetry:p1
    fn record_ingestion_request(
        &self,
        outcome: IngestRequestOutcome,
        error_category: IngestRequestErrorCategory,
    );

    /// Increment `uc_ingestion_quota_rejections_total` by `entries` — the
    /// **submitted entry count**, not one per rejected request.
    ///
    /// DESIGN §3.11.5: "Incremented by the submitted entry count when a
    /// submission is rejected for exceeding its allowance. This carries
    /// throttled volume: `uc_ingestion_records_total` cannot, because its
    /// required `entry_type` label is caller-supplied and not yet validated at
    /// the charge point." A per-request increment would make a 1-entry overrun
    /// and a 100-entry one read identically, which is the one thing this
    /// instrument exists to tell apart.
    ///
    /// **Unlabelled.** §3.11.5's "Label cardinality" paragraph names the quota
    /// as its own worked example of why `subject_id` and `tenant_id` must not be
    /// labels. The throttled caller's identity rides the structured log the
    /// service emits alongside this increment.
    // @cpt-algo:cpt-cf-usage-collector-algo-telemetry-label-admission:p2
    fn record_quota_rejection(&self, entries: u64);

    /// Set `uc_ingestion_quota_buckets_active` to `count` — the number of
    /// quota buckets currently resident on this replica.
    ///
    /// DESIGN §3.11.5: "Live quota buckets, one per recently-active calling
    /// subject. Watches map growth and confirms idle eviction is keeping up.
    /// Unlabelled." Per-replica, like the state it measures: aggregate it with
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
    /// [`QueryKind::admits_result_rows_sample`] holds for `kind`: DESIGN
    /// §3.11.5 admits `aggregated`, `raw` on this row alone.
    fn observe_query_result_rows(&self, kind: QueryKind, rows: u64);

    /// Increment `uc_query_requests_total{query_kind, outcome,
    /// error_category}` — once per completed query attempt — plus, when
    /// [`QueryKind::admits_duration_sample`] holds for `kind`, observe
    /// `uc_query_duration_seconds{query_kind}` in the same call.
    ///
    /// The two instruments are sampled together, one completion per call, so
    /// they can never disagree about how many requests finished — except for
    /// the one `QueryKind` the duration row does not admit, where the
    /// implementation must skip the histogram write rather than emit a label
    /// value DESIGN §3.11.5 forbids there.
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
    /// [`crate::domain::type_resolver::TypeResolver::resolve`] call.
    ///
    /// **Exactly once per resolution, on every path.** Every `CacheHit` site
    /// in `TypeResolver::resolve` returns immediately, and each arm of
    /// `TypeResolver::populate` records one outcome and only one. No path
    /// records twice and none records nothing.
    // @cpt-dod:cpt-cf-usage-collector-dod-resolution-telemetry:p1
    //
    // Realizes the ONE-OBSERVATION-PER-RESOLUTION half of this DoD's first
    // MUST clause, and that half alone. The clause's label-vocabulary half
    // is realized by [`TypeResolutionOutcome`], which carries this same tag
    // scoped to it; see that type's own marker block for the full four-clause
    // attribution across this trait.
    fn record_type_resolution(&self, outcome: TypeResolutionOutcome);

    /// `uc_resolved_types` — current entry count of the resolved-declaration
    /// cache (DESIGN §3.11.5).
    ///
    /// **Per-instance**: §3.11.5 says *"aggregate with `max` or `last`, never
    /// `sum`"*, every replica holding its own cache of the same platform-global
    /// declarations. Set imperatively rather than read by a collection callback,
    /// matching [`Self::set_quota_buckets_active`]: a callback would have to
    /// hold the resolver, which an output port must not.
    fn set_resolved_types(&self, count: u64);

    /// `uc_declaration_cache_age_seconds` — age of the oldest cached
    /// declaration since its **last successful refresh** (DESIGN §3.11.5).
    ///
    /// Backs §3.11.6's declaration-cache-staleness alert, which fires when
    /// the oldest cached declaration exceeds the configured refresh interval
    /// by 2x or more.
    ///
    /// `None` means the cache is empty, which is **not** age zero: a zero would
    /// read as perfectly fresh and satisfy the staleness alert forever. §3.11.5
    /// gives the row no sentinel, so the adapter records nothing and the series
    /// has no point for that interval — the disposition the plugin's own
    /// `uc_timescaledb_rollup_refresh_age_seconds` documents as *"unset until the
    /// policy's first success"*.
    ///
    /// **A sampled gauge, not a continuously computed one, which has a freeze
    /// property worth stating.** The resolver publishes it only when it samples
    /// — on a successful fetch and on a stale serve past the TTL (both call
    /// `publish_cache_gauges` / `sample_cache_gauges` in
    /// `domain/type_resolver/mod.rs`) — never on a timer, so between samples the
    /// series reads fresher than reality. A **serving** gear samples at least
    /// once per TTL, so its worst-case lag is one TTL and §3.11.6's >=2x
    /// threshold is still crossed, just later. An **idle** gear — one no longer
    /// resolving any meter in its cache — samples not at all, so the gauge
    /// freezes and the staleness alert never fires for it.
    ///
    /// **The remedy, a periodic sampler independent of resolution, is
    /// unbuilt**: `publish_cache_gauges`' only production callers
    /// (`TypeResolver::store` and the `ServedStale` arm via
    /// `sample_cache_gauges`) are both resolution-driven.
    // @cpt-dod:cpt-cf-usage-collector-dod-resolution-telemetry:p1
    //
    // Realizes this DoD's "It MUST publish the age of the oldest served
    // declaration, so cache staleness is measurable" clause, and that clause
    // alone. The *publish* obligation is met: the age is
    // `entries.values().map(|e| e.fetched_at).min()`, DESIGN §3.11.5's own
    // wording for this row. The trailing rationale holds for a SERVING gear but
    // not an idle one — the freeze property above. See
    // [`TypeResolutionOutcome`]'s marker block for the per-clause attribution.
    fn set_declaration_cache_age_seconds(&self, age: Option<u64>);

    /// `uc_type_resolution_duration_seconds{result}` — how long one
    /// resolution took (DESIGN §3.11.5).
    ///
    /// §3.11.5 declares this row's `result` label with `cache_hit` and
    /// `cache_miss` alone, where the counter carries the whole
    /// [`TypeResolutionOutcome`] set. The adapter therefore records only those
    /// two, and the rest are **deliberately unobserved on this series**:
    /// §3.11.2 fixes the histogram's purpose as showing that *"a cache miss is a
    /// cold-path cost, not a budget line"*, and the failure *rate* is already
    /// carried by `uc_type_resolution_total{result}`.
    ///
    /// **Do not map a failure onto `cache_miss` to close the gap.** That would
    /// put registry-timeout durations into the figure an operator reads as the
    /// cold-path cost, which is worse than the blind spot. Widening the row is a
    /// §3.11.5 edit, recorded as rejected-not-overlooked in
    /// `spec-8-errors-metrics-observability.md` §6 T2.
    ///
    /// **The two label values are timed from different origins, and that matters
    /// for how `cache_hit` reads.** `TypeResolver::resolve` times a `cache_hit`
    /// from its own entry, which on the post-gate hit path includes however long
    /// the call waited behind the per-key single-flight gate for another
    /// resolution's fetch. `TypeResolver::populate` times `cache_miss` from a
    /// later `Instant`, taken once the gate is held, excluding that wait. This
    /// is deliberate — the wait is part of what the waiting caller experienced —
    /// but it is unqualified in the code: on a cold start or TTL expiry with N
    /// concurrent resolutions of one meter, one sample lands on `cache_miss` at
    /// roughly the fetch cost and the other N-1 land on `cache_hit` at roughly
    /// the same cost. An operator reading `cache_hit` as the steady-state budget
    /// line will see fetch-cost samples there, from a label value whose name
    /// gives no hint that it can include a fetch. See the call sites in
    /// `domain/type_resolver/mod.rs`.
    // @cpt-dod:cpt-cf-usage-collector-dod-resolution-telemetry:p1
    //
    // Realizes this DoD's "MUST emit a duration histogram for the hit and miss
    // paths" clause, and that clause alone. The clause names the hit and miss
    // paths and no other, which is why a restore observes nothing here (pinned
    // by `resolver_tests.rs`'s
    // `a_restore_observes_no_duration_sample_while_a_miss_does`). See
    // [`TypeResolutionOutcome`]'s marker block for the per-clause attribution.
    fn observe_type_resolution_duration(&self, outcome: TypeResolutionOutcome, seconds: f64);

    /// Increment `uc_declaration_mirror_write_failures_total` — one
    /// declaration resolved but not mirrored.
    ///
    /// DESIGN §3.11.5 (`uc_declaration_mirror_write_failures_total`): *"A
    /// declaration resolved but not mirrored. Each one is a type that a later
    /// registry restart will not restore. Ingestion is unaffected."*
    ///
    /// **Unlabelled** — the row's label column is `—`.
    ///
    /// **A failed mirror READ is not counted here.** This instrument's
    /// population is writes that did not land; a read failure may leave a
    /// perfectly good row behind, and counting it would tell an operator that
    /// types are failing to mirror when they are not.
    // @cpt-dod:cpt-cf-usage-collector-dod-resolution-telemetry:p1
    //
    // Realizes this DoD's "It MUST count a failed mirror write separately,
    // since that failure changes no caller-visible outcome" clause, and that
    // clause alone. Both halves are here: *separately* is this instrument being
    // its own unlabelled counter rather than a `result` value on
    // `uc_type_resolution_total`, and *changes no caller-visible outcome* is
    // `TypeResolver::mirror_document` counting through this method and returning
    // `()`, so the resolution it rode on still serves (ADR statement 4). See
    // [`TypeResolutionOutcome`]'s marker block for the per-clause attribution.
    fn record_declaration_mirror_write_failure(&self);
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
