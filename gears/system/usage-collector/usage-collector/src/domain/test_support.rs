//! Shared test infrastructure for domain-layer unit tests.
//!
//! GTS registry: use `MockTypesRegistryClient` and `make_test_instance` from
//! `types_registry_sdk::testing` directly (gated on the `test-util`
//! dev-dependency feature).
//!
//! PDP: this module exposes `AuthZResolverApi` fakes plus `PolicyEnforcer`
//! constructors that let tests pin every outcome (permit + constraints, deny,
//! empty-constraints fail-closed, transport unreachable, and no-cache via the
//! per-call counting resolvers).
//!
//! Every helper here is test-only, so `.lock().expect("…")` is fine: a
//! poisoned mutex inside a unit test is an unrecoverable test failure anyway.
//! `missing_panics_doc` is suppressed module-wide rather than answered with a
//! boilerplate `# Panics` section on every setter.
#![allow(clippy::missing_panics_doc)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use authz_resolver_sdk::constraints::Constraint;
use authz_resolver_sdk::models::{
    DenyReason, EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_odata::{ODataQuery, ast};
use toolkit_security::{PlatformSecurityContext, pep_properties};
use usage_collector_sdk::{
    AggregationDimension, AggregationFold, AggregationResult, CreateUsageRecord, EntryType,
    FeedPage, FeedPosition, FeedStart, Keyset, MetadataFilter, MeterRef, MeterTypeId,
    ReconciliationMetadata, RecordOrigin, RecordPage, StoredUsageRecord, TimeRange,
    UsageCollectorPluginError, UsageCollectorPluginV1, UsageRecord,
};
use uuid::Uuid;

/// The registry reference this module's `DeclarationSource` doubles resolve
/// `id` to.
///
/// Read off the very schema [`fake_meter_schema`] builds rather than derived
/// a second way, so a double answering a record and the forward resolver that
/// resolved its meter cannot disagree about the reference — which is what the
/// gear's re-attachment check compares.
#[must_use]
pub(crate) fn declared_uuid(id: &MeterTypeId) -> Uuid {
    let type_uuid = fake_meter_schema(id, "SUM", &[]).type_uuid;
    // Recorded so [`FixtureReverseDeclarationSource`] can answer the reverse
    // direction: a `UUIDv5` cannot be inverted.
    if let Ok(mut known) = DECLARED_REFERENCES
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
    {
        known.insert(type_uuid, id.clone());
    }
    type_uuid
}

/// Every reference [`declared_uuid`] has handed out, by the identifier it
/// was derived for.
static DECLARED_REFERENCES: std::sync::OnceLock<
    Mutex<std::collections::HashMap<Uuid, MeterTypeId>>,
> = std::sync::OnceLock::new();

/// `record` in the storage shape, under the reference this module's
/// declaration doubles resolve its own meter to.
///
/// The conversion to reach for when programming a double with a record the
/// *gear* will re-attach an identifier to: a page item, or the `existing`
/// entry a conflict names. Any other reference is refused by the gear's
/// re-attachment check.
#[must_use]
pub(crate) fn as_stored(record: UsageRecord) -> StoredUsageRecord {
    let gts_type_uuid = declared_uuid(&record.gts_type_id);
    record.into_stored(gts_type_uuid)
}

/// A covered period a live emitter could plausibly have just closed: one
/// hour long, ending an hour ago.
///
/// Ingestion fixtures need this because the live path bounds the end of the
/// covered period against the wall clock, 48 hours by default. An hour clear
/// of both bounds rather than minutes: a test 30 seconds from a boundary
/// fails on a loaded CI runner. Read paths keep their epoch fixtures — they
/// read no clock.
///
/// Resolved once per test process and reused verbatim, so two fixtures built
/// by separate calls describe the *same* period. Both bounds are
/// dedup-identity inputs, so a pair that drifted by a microsecond would
/// derive two different ids and quietly stop corresponding.
pub(crate) fn recent_window() -> (time::OffsetDateTime, time::OffsetDateTime) {
    static WINDOW: std::sync::LazyLock<(time::OffsetDateTime, time::OffsetDateTime)> =
        std::sync::LazyLock::new(|| {
            // Whole seconds: `try_into_usage_record` admits at most
            // microsecond precision, and a hand-written RFC 3339 fixture can
            // echo a whole second without surprises.
            let now = time::OffsetDateTime::now_utc();
            let end =
                now.replace_nanosecond(0).expect("0 ns is in range") - time::Duration::hours(1);
            (end - time::Duration::hours(1), end)
        });
    *WINDOW
}

/// The inclusive start of [`recent_window`].
pub(crate) fn recent_window_start() -> time::OffsetDateTime {
    recent_window().0
}

/// The exclusive end of [`recent_window`] — the only bound the ingestion
/// path's tolerances read.
///
/// The two accessors exist alongside the pair because most fixtures are
/// struct literals with nowhere convenient to bind a tuple. Both read the one
/// memoised pair, so the halves cannot come from different clock reads.
pub(crate) fn recent_window_end() -> time::OffsetDateTime {
    recent_window().1
}

/// The covered-period bounds every fixture `Service` is built with: the
/// published defaults, projected from `UsageCollectorConfig::default()`
/// rather than restated here, so a fixture can never quietly enforce a
/// tolerance the shipped configuration does not.
#[must_use]
pub fn default_covered_period_bounds() -> crate::domain::covered_period::CoveredPeriodBounds {
    crate::config::UsageCollectorConfig::default().covered_period_bounds()
}

/// Projects a create submission into its persisted shape, for a plugin echo
/// fixture that must agree with the service on the derived `id`.
///
/// Routes through the real SDK projections rather than hand-building a
/// [`UsageRecord`]: a stub that invented its own `id` would pass even if the
/// service stopped deriving one. The projection is chosen off the declared
/// `entry_type` exactly as `Service::project_and_admit` chooses it, so a
/// fixture cannot produce an entry the gateway would not have.
///
/// Stamped `Live`; for an imported entry use [`projected_with_origin`].
pub(crate) fn projected(submission: &CreateUsageRecord) -> UsageRecord {
    projected_with_origin(submission, RecordOrigin::Live)
}

/// [`projected`] for a route other than the live one — the shape the
/// backfill route hands its storage plugin.
///
/// An echo fixture programmed with a `Live` projection returns `origin = live`
/// however the gateway stamped the entry it was handed, so a test asserting
/// the marker on the returned record would be asserting its own fixture.
pub(crate) fn projected_with_origin(
    submission: &CreateUsageRecord,
    origin: RecordOrigin,
) -> UsageRecord {
    let now = time::OffsetDateTime::UNIX_EPOCH;
    let clone = submission.clone();
    match submission.entry_type() {
        EntryType::Record => clone.try_into_usage_record(origin, now),
        EntryType::Invalidation => {
            let target = crate::domain::invalidation::derive_invalidation_target(submission)
                .expect("test fixture withdrawal carries an idempotency key");
            clone.try_into_invalidation_record(origin, now, target)
        }
    }
    .expect("test fixture supplies a valid covered period")
}

/// A test quantity. Panics on an invalid literal, which is a test bug.
pub(crate) fn qty(text: &str) -> usage_collector_sdk::UsageQuantity {
    usage_collector_sdk::UsageQuantity::parse(text).expect("test quantity literal")
}

/// The mandatory read-path range the domain read-path tests hand to
/// `list_usage_records` / `query_aggregated_usage_records`.
///
/// One hour from the epoch — narrow enough that a throwaway "all time"
/// range substituted anywhere on the way to the SPI is not equal to it.
#[must_use]
pub(crate) fn test_time_range() -> TimeRange {
    TimeRange::new(
        time::OffsetDateTime::UNIX_EPOCH,
        time::OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
    )
    .expect("a one-hour range from the epoch is a valid TimeRange")
}

/// Minimal mock storage-plugin client.
///
/// Exists so the Plugin Host can resolve a concrete
/// `Arc<dyn UsageCollectorPluginV1>` from `ClientHub` and cache tests can
/// assert `Arc::ptr_eq` on the handle. Every method returns a deterministic
/// `Internal("test_fake: …")`, so a test that accidentally dispatches through
/// it fails obviously rather than getting a plausible answer.
pub struct MockPlugin;

impl MockPlugin {
    /// Returns a fresh mock plugin as a scoped trait object.
    #[must_use]
    pub fn arc() -> Arc<dyn UsageCollectorPluginV1> {
        Arc::new(Self)
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for MockPlugin {
    async fn create_usage_records(
        &self,
        _records: Vec<(MeterRef, StoredUsageRecord)>,
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        Err(UsageCollectorPluginError::internal(
            "test_fake: MockPlugin::create_usage_records not implemented",
        ))
    }

    async fn query_aggregated_usage_records(
        &self,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _fold: AggregationFold,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: MockPlugin::query_aggregated_usage_records not implemented",
        ))
    }

    async fn list_usage_records(
        &self,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _keyset: Option<&Keyset>,
    ) -> Result<RecordPage, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: MockPlugin::list_usage_records not implemented",
        ))
    }

    async fn get_usage_record(
        &self,
        _id: Uuid,
        _scope: &ast::Expr,
        _converged_only: bool,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: MockPlugin::get_usage_record not implemented",
        ))
    }

    async fn read_feed_page(
        &self,
        _subscription: &[MeterRef],
        _scope: &ast::Expr,
        _start: FeedStart<FeedPosition>,
        _until: Option<FeedPosition>,
        _limit: u64,
    ) -> Result<FeedPage<FeedPosition, StoredUsageRecord>, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: MockPlugin::read_feed_page not implemented",
        ))
    }

    async fn get_reconciliation_metadata(
        &self,
        _tenant_id: Uuid,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _fold: AggregationFold,
        _scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: MockPlugin::get_reconciliation_metadata not implemented",
        ))
    }
}

/// Recording storage-plugin double for the reconciliation read path.
///
/// Every SPI method other than `get_reconciliation_metadata` returns a
/// deterministic `Internal`, on [`MockPlugin`]'s posture.
///
/// `get_reconciliation_metadata` **records every argument it was handed — the
/// compiled PDP `scope` included**, so a read-path test can observe the
/// argument carrying the authorization decision. It answers with
/// [`ReconciliationMetadata::empty_for`] the fold it was built with
/// ([`Self::answering_for`]) — **not** the `fold` argument the call carries —
/// so a test can program a plugin that disagrees with the declaration's fold,
/// which is what the branch-mismatch guard test needs.
pub struct RecordingReconciliationPlugin {
    answers_with: AggregationFold,
    calls: AtomicUsize,
    last: Mutex<Option<RecordingReconciliationCall>>,
}

/// The arguments of one `get_reconciliation_metadata` call, as
/// [`RecordingReconciliationPlugin`] recorded them.
type RecordingReconciliationCall = (Uuid, MeterRef, TimeRange, AggregationFold, ast::Expr);

impl RecordingReconciliationPlugin {
    /// Build a double that answers every `get_reconciliation_metadata` call
    /// with the empty summary for `fold` — the branch `fold` selects, not
    /// necessarily the branch the caller's declared fold would select.
    #[must_use]
    pub fn answering_for(fold: AggregationFold) -> Arc<Self> {
        Arc::new(Self {
            answers_with: fold,
            calls: AtomicUsize::new(0),
            last: Mutex::new(None),
        })
    }

    /// Number of `get_reconciliation_metadata` calls observed so far.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// The `fold` argument of the most recent call.
    #[must_use]
    pub fn last_fold(&self) -> Option<AggregationFold> {
        self.last
            .lock()
            .expect("mutex")
            .as_ref()
            .map(|(_, _, _, fold, _)| *fold)
    }

    /// The compiled PDP `scope` argument of the most recent call.
    #[must_use]
    pub fn last_scope(&self) -> Option<ast::Expr> {
        self.last
            .lock()
            .expect("mutex")
            .as_ref()
            .map(|(_, _, _, _, scope)| scope.clone())
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for RecordingReconciliationPlugin {
    async fn create_usage_records(
        &self,
        _records: Vec<(MeterRef, StoredUsageRecord)>,
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        Err(UsageCollectorPluginError::internal(
            "test_fake: RecordingReconciliationPlugin::create_usage_records not implemented",
        ))
    }

    async fn query_aggregated_usage_records(
        &self,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _fold: AggregationFold,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: RecordingReconciliationPlugin::query_aggregated_usage_records not \
             implemented",
        ))
    }

    async fn list_usage_records(
        &self,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _keyset: Option<&Keyset>,
    ) -> Result<RecordPage, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: RecordingReconciliationPlugin::list_usage_records not implemented",
        ))
    }

    async fn get_usage_record(
        &self,
        _id: Uuid,
        _scope: &ast::Expr,
        _converged_only: bool,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: RecordingReconciliationPlugin::get_usage_record not implemented",
        ))
    }

    async fn read_feed_page(
        &self,
        _subscription: &[MeterRef],
        _scope: &ast::Expr,
        _start: FeedStart<FeedPosition>,
        _until: Option<FeedPosition>,
        _limit: u64,
    ) -> Result<FeedPage<FeedPosition, StoredUsageRecord>, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: RecordingReconciliationPlugin::read_feed_page not implemented",
        ))
    }

    async fn get_reconciliation_metadata(
        &self,
        tenant_id: Uuid,
        meter: &MeterRef,
        time_range: TimeRange,
        fold: AggregationFold,
        scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last.lock().expect("mutex") =
            Some((tenant_id, meter.clone(), time_range, fold, scope.clone()));
        Ok(ReconciliationMetadata::empty_for(self.answers_with))
    }
}

/// A storage-plugin double that panics from every SPI method, carrying a
/// fixed message.
///
/// Built for the all-or-nothing feed-resolve test. A call counter checked
/// after the fact only shows no call was *recorded*, which a counter the
/// resolve path never touched also shows; a panic fails loudly from inside
/// the plugin the instant any method is reached.
pub(crate) struct PanickingPlugin {
    message: &'static str,
}

impl PanickingPlugin {
    /// Build a double that panics `message` from every SPI method.
    #[must_use]
    pub(crate) fn new(message: &'static str) -> Self {
        Self { message }
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for PanickingPlugin {
    async fn create_usage_records(
        &self,
        _records: Vec<(MeterRef, StoredUsageRecord)>,
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        panic!("{}", self.message)
    }

    async fn query_aggregated_usage_records(
        &self,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _fold: AggregationFold,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        panic!("{}", self.message)
    }

    async fn list_usage_records(
        &self,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _keyset: Option<&Keyset>,
    ) -> Result<RecordPage, UsageCollectorPluginError> {
        panic!("{}", self.message)
    }

    async fn get_usage_record(
        &self,
        _id: Uuid,
        _scope: &ast::Expr,
        _converged_only: bool,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        panic!("{}", self.message)
    }

    async fn read_feed_page(
        &self,
        _subscription: &[MeterRef],
        _scope: &ast::Expr,
        _start: FeedStart<FeedPosition>,
        _until: Option<FeedPosition>,
        _limit: u64,
    ) -> Result<FeedPage<FeedPosition, StoredUsageRecord>, UsageCollectorPluginError> {
        panic!("{}", self.message)
    }

    async fn get_reconciliation_metadata(
        &self,
        _tenant_id: Uuid,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _fold: AggregationFold,
        _scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        panic!("{}", self.message)
    }
}

// ── PDP (authz-resolver) mocks ─────────────────────────────────────────────

/// Build a permit `EvaluationResponse` carrying a single
/// `property = value` string-equality constraint. Compiles (against a resource
/// type that lists `property` as supported) to a non-empty `AccessScope`, so it
/// exercises the permit-with-constraints `Ok` path through
/// `require_constraints(true)`.
#[must_use]
pub fn permit_with_string_constraint(property: &'static str, value: String) -> EvaluationResponse {
    use authz_resolver_sdk::constraints::{EqPredicate, Predicate};

    EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::Eq(EqPredicate::new(property, value))],
            }],
            deny_reason: None,
        },
    }
}

/// Build a permit `EvaluationResponse` scoped to the request's own
/// `OWNER_TENANT_ID`, simulating a tenant-scoped grant that authorizes
/// exactly the tenant the record under test names.
///
/// The per-record authz path runs under `require_constraints(true)` and
/// applies `authz::scope_admits_attribution_tuple`'s gate. An
/// empty-constraints permit fails closed there, and a fixed-tenant constraint
/// satisfies the gate for one hard-coded tenant only; echoing the request's
/// own `OWNER_TENANT_ID` back admits ANY record tenant a test picks.
///
/// A request carrying no `OWNER_TENANT_ID` falls back to an
/// empty-constraints `allow_all` permit. No gear path produces such a
/// request, so that arm is reachable only from a test that builds the
/// request itself.
#[must_use]
pub fn permit_scoped_to_request_tenant(request: &EvaluationRequest) -> EvaluationResponse {
    match request
        .resource
        .properties
        .get(pep_properties::OWNER_TENANT_ID)
        .and_then(serde_json::Value::as_str)
    {
        Some(tenant) => {
            permit_with_string_constraint(pep_properties::OWNER_TENANT_ID, tenant.to_owned())
        }
        None => EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: Vec::new(),
                deny_reason: None,
            },
        },
    }
}

/// PDP fake that denies every request whose composed `resource_id` equals
/// `deny_resource_id` and permits every other, scoping each permit to the
/// request's own tenant (see [`permit_scoped_to_request_tenant`]).
///
/// `domain/authz.rs` populates the request's `PROP_RESOURCE_ID` from the
/// attribution tuple, so this resolver discriminates **per attribution
/// tuple** — which is what lets a batch test mix permitted and denied records
/// in one call and assert the per-index outcomes.
#[derive(Debug)]
pub struct DenyOneResourceResolver {
    deny_resource_id: String,
}

impl DenyOneResourceResolver {
    /// Denies the attribution tuple whose `resource_id` is
    /// `deny_resource_id`; permits every other.
    #[must_use]
    pub fn new(deny_resource_id: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            deny_resource_id: deny_resource_id.into(),
        })
    }
}

#[async_trait]
impl AuthZResolverApi for DenyOneResourceResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        let matches_deny = request
            .resource
            .properties
            .get(crate::domain::authz::usage_record::PROP_RESOURCE_ID)
            .and_then(serde_json::Value::as_str)
            == Some(self.deny_resource_id.as_str());
        if matches_deny {
            return Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext {
                    constraints: Vec::new(),
                    deny_reason: Some(DenyReason {
                        error_code: "test-deny".to_owned(),
                        details: None,
                    }),
                },
            });
        }
        // Scoped to the record's own tenant so the per-record gate admits it;
        // an empty-constraints permit would fail closed as `CompileFailed`.
        Ok(permit_scoped_to_request_tenant(&request))
    }
}

/// Build a permit [`EvaluationResponse`] scoped to `request.subject`'s own
/// home tenant — `request.subject.properties["tenant_id"]`, which
/// `PolicyEnforcer::build_request_with` always populates from
/// `ctx.subject_tenant_id()` (see `authz-resolver-sdk/src/pep/enforcer.rs`).
///
/// The subject-property counterpart of [`permit_scoped_to_request_tenant`],
/// which reads the **resource** properties a per-record request carries. A
/// pre-row read composes no resource properties at all, so a fake wanting a
/// grant tied to *this* caller has to read the subject side instead.
///
/// # Panics
///
/// Panics (test-only) if `request.subject.properties` carries no
/// `"tenant_id"` string — every [`SecurityContext`]-derived request does.
#[must_use]
pub fn permit_scoped_to_request_subject_tenant(request: &EvaluationRequest) -> EvaluationResponse {
    let tenant = request
        .subject
        .properties
        .get("tenant_id")
        .and_then(serde_json::Value::as_str)
        .expect("PolicyEnforcer::build_request_with always sets subject.properties[\"tenant_id\"]")
        .to_owned();
    permit_with_string_constraint(pep_properties::OWNER_TENANT_ID, tenant)
}

/// PDP fake for the pre-row read paths that compose no per-instance resource
/// properties (`list_usage_records`, `read_usage_feed`,
/// `get_reconciliation_metadata`): denies every evaluation whose
/// `request.subject.id` is `deny_subject_id`, and permits every other by
/// echoing the caller's **own** subject tenant back as the
/// `OWNER_TENANT_ID` constraint (see
/// [`permit_scoped_to_request_subject_tenant`]) — not a constant every
/// permitted caller would share.
///
/// Echoing the caller's own tenant is what lets a test tell "the scope
/// compiled for this call" apart from "some fixed tenant-shaped scope every
/// permit produces": two permitted [`SecurityContext`]s naming two tenants
/// get two different compiled scopes back.
///
/// One `Service` with this one resolver therefore serves both halves of a
/// permit/deny pair, varying only the caller's [`SecurityContext`].
#[derive(Debug)]
pub struct DenyOneSubjectResolver {
    deny_subject_id: Uuid,
}

impl DenyOneSubjectResolver {
    /// Denies every request whose `subject.id` is `deny_subject_id`; permits
    /// every other, scoped to that request's own subject tenant.
    #[must_use]
    pub fn new(deny_subject_id: Uuid) -> Arc<Self> {
        Arc::new(Self { deny_subject_id })
    }
}

#[async_trait]
impl AuthZResolverApi for DenyOneSubjectResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        if request.subject.id == self.deny_subject_id {
            return Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext {
                    constraints: Vec::new(),
                    deny_reason: Some(DenyReason {
                        error_code: "test-deny".to_owned(),
                        details: None,
                    }),
                },
            });
        }
        Ok(permit_scoped_to_request_subject_tenant(&request))
    }
}

/// Counting PDP fake that permits and scopes the grant to the request's own
/// `OWNER_TENANT_ID` (see [`permit_scoped_to_request_tenant`]), recording the
/// call count so per-record dedup tests can still assert the exact number of
/// PDP round-trips. Use this for the per-record (`usage_record`) paths under
/// `require_constraints(true)` where [`CountingAllowAllResolver`] would fail
/// closed.
#[derive(Debug, Default)]
pub struct CountingTenantPermitResolver {
    calls: AtomicUsize,
}

impl CountingTenantPermitResolver {
    /// Build a tenant-scoped permit resolver.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Number of `evaluate` calls observed so far.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AuthZResolverApi for CountingTenantPermitResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(permit_scoped_to_request_tenant(&request))
    }
}

/// PDP fake that permits with two OR-ed constraints: the request's own
/// `OWNER_TENANT_ID` (see [`permit_scoped_to_request_tenant`]) and a sibling
/// that narrows by `resource_type` only, with no `OWNER_TENANT_ID` predicate
/// of its own.
///
/// The sibling must survive the PDP round-trip and still be unprojectable.
/// Predicate kinds the SDK's `compile_constraint` drops never reach the gear
/// at all, leaving only the tenant constraint, which admits everything. So
/// the sibling is built from what `constraint_to_odata_conjunction`
/// (`domain/authz.rs`) itself denies: an `Eq` on the *supported*
/// `PROP_RESOURCE_TYPE` with no `OWNER_TENANT_ID` narrowing.
///
/// `scope_to_odata_filter`'s per-constraint loop propagates with `?`, so that
/// one un-pinned disjunct fails the whole projection closed. The per-record
/// gate still admits through the tenant constraint alone, since
/// `constraint_verdict` also requires tenant pinning.
#[derive(Debug, Default)]
pub struct UncompilableSiblingPermitResolver;

#[async_trait]
impl AuthZResolverApi for UncompilableSiblingPermitResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        use authz_resolver_sdk::constraints::{EqPredicate, Predicate};

        let mut response = permit_scoped_to_request_tenant(&request);
        response.context.constraints.push(Constraint {
            // A synthetic value matching no fixture's real `resource_type`.
            // Its only job is to be a well-formed, PEP-supported filter
            // carrying no `OWNER_TENANT_ID` narrowing.
            predicates: vec![Predicate::Eq(EqPredicate::new(
                crate::domain::authz::usage_record::PROP_RESOURCE_TYPE,
                "test.uncompilable-sibling.non-tenant-narrowing",
            ))],
        });
        Ok(response)
    }
}

/// PDP fake that permits and narrows the grant **by meter**: one constraint
/// pinning the caller's own tenant AND
/// [`crate::domain::authz::usage_record::PROP_GTS_TYPE_ID`] to a fixed value,
/// optionally beside a second, tenant-only constraint.
///
/// It goes through a real PDP round-trip because a hand-built
/// [`toolkit_security::AccessScope`] skips `compile_constraint`, which drops
/// a constraint naming an unadvertised property **before the gear sees it**.
/// With `PROP_GTS_TYPE_ID` advertised, as it is:
///
/// * **Sole constraint** — it reaches the gear and is denied there by name,
///   rather than denied as `AllConstraintsFailed`, naming nothing.
/// * **With the tenant-only sibling** — the whole projection fails closed,
///   rather than the read being served narrowed to the tenant alone and
///   silently wider than the PDP granted.
///
/// The tenant it pins is the one the request itself names: the **resource**
/// `OWNER_TENANT_ID` on the per-record write path, falling back to the
/// **subject**'s own tenant on the pre-row read paths, which compose no
/// resource properties. Either way the tenant half always admits, so a test's
/// outcome turns on the meter half alone.
#[derive(Debug)]
pub struct MeterNarrowingPermitResolver {
    meter: &'static str,
    tenant_only_sibling: bool,
}

impl MeterNarrowingPermitResolver {
    /// Permit narrowed to `meter` and the caller's tenant, as the single
    /// constraint.
    #[must_use]
    pub fn sole(meter: &'static str) -> Arc<Self> {
        Arc::new(Self {
            meter,
            tenant_only_sibling: false,
        })
    }

    /// Permit narrowed to `meter` and the caller's tenant, OR-ed with a
    /// second constraint that pins the tenant alone.
    #[must_use]
    pub fn with_tenant_only_sibling(meter: &'static str) -> Arc<Self> {
        Arc::new(Self {
            meter,
            tenant_only_sibling: true,
        })
    }
}

#[async_trait]
impl AuthZResolverApi for MeterNarrowingPermitResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        use authz_resolver_sdk::constraints::{EqPredicate, Predicate};

        let tenant = request
            .resource
            .properties
            .get(pep_properties::OWNER_TENANT_ID)
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                request
                    .subject
                    .properties
                    .get("tenant_id")
                    .and_then(serde_json::Value::as_str)
            })
            .expect("every request names a tenant on one side or the other")
            .to_owned();

        let mut constraints = vec![Constraint {
            predicates: vec![
                Predicate::Eq(EqPredicate::new(
                    pep_properties::OWNER_TENANT_ID,
                    tenant.clone(),
                )),
                Predicate::Eq(EqPredicate::new(
                    crate::domain::authz::usage_record::PROP_GTS_TYPE_ID,
                    self.meter,
                )),
            ],
        }];
        if self.tenant_only_sibling {
            constraints.push(Constraint {
                predicates: vec![Predicate::Eq(EqPredicate::new(
                    pep_properties::OWNER_TENANT_ID,
                    tenant,
                ))],
            });
        }
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints,
                deny_reason: None,
            },
        })
    }
}

/// PDP fake that permits everything and records the `action` of every
/// request it sees.
///
/// The counting fakes answer "how many decisions"; this one answers "which
/// verb". A test that only counts calls cannot tell a batch authorizing
/// `create` twice from one authorizing `create` and `backfill`. Permits
/// through [`permit_scoped_to_request_tenant`], clearing the per-record
/// attribution gate for any tenant a test picks.
#[derive(Debug, Default)]
pub struct ActionRecordingPermitResolver {
    actions: std::sync::Mutex<Vec<String>>,
}

impl ActionRecordingPermitResolver {
    /// Build an action-recording tenant-scoped permit resolver.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The actions authorized so far, sorted.
    ///
    /// Sorted rather than in call order: the batch PDP fan-out is
    /// `buffer_unordered`, so call order is not deterministic.
    #[must_use]
    pub fn actions_sorted(&self) -> Vec<String> {
        let mut seen = self.actions.lock().expect("mutex").clone();
        seen.sort();
        seen
    }
}

#[async_trait]
impl AuthZResolverApi for ActionRecordingPermitResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.actions
            .lock()
            .expect("mutex")
            .push(request.action.name.clone());
        Ok(permit_scoped_to_request_tenant(&request))
    }
}

/// Counting PDP fake that always permits with a fixed string-equality
/// constraint and records how many times it was called, so the no-cache test
/// can assert that two identical authorize calls each hit the resolver.
#[derive(Debug)]
pub struct CountingPermitResolver {
    property: &'static str,
    value: String,
    calls: AtomicUsize,
}

impl CountingPermitResolver {
    /// Build a resolver that permits with a single `property = value` string
    /// constraint.
    #[must_use]
    pub fn new(property: &'static str, value: String) -> Arc<Self> {
        Arc::new(Self {
            property,
            value,
            calls: AtomicUsize::new(0),
        })
    }

    /// Number of `evaluate` calls observed so far.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AuthZResolverApi for CountingPermitResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(permit_with_string_constraint(
            self.property,
            self.value.clone(),
        ))
    }
}

/// Counting PDP fake that always permits with NO constraints (an
/// `allow_all` decision) and records call counts. Use this whenever the
/// resource type under test declares no supported PEP attributes — a
/// constraint-bearing permit (e.g. [`CountingPermitResolver`]) would fail
/// to compile under such a resource type, surfacing as
/// `EnforcerError::CompileFailed`.
#[derive(Debug, Default)]
pub struct CountingAllowAllResolver {
    calls: AtomicUsize,
}

impl CountingAllowAllResolver {
    /// Build an `allow_all` permit resolver.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Number of `evaluate` calls observed so far.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AuthZResolverApi for CountingAllowAllResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: Vec::new(),
                deny_reason: None,
            },
        })
    }
}

/// PDP fake that captures the most-recent [`EvaluationRequest`] it received
/// (so tests can inspect the request shape the PEP composed) AND scopes its
/// permit to the request's own `OWNER_TENANT_ID` (see
/// [`permit_scoped_to_request_tenant`]), which is what lets the call return
/// `Ok` under the per-record gate where an empty-constraints permit would
/// fail closed. Used by the `authz_tests` equivalence regression over the
/// per-record and per-tuple PDP composers.
#[derive(Debug, Default)]
pub struct CapturingTenantPermitResolver {
    last_request: std::sync::Mutex<Option<EvaluationRequest>>,
}

impl CapturingTenantPermitResolver {
    /// Build a capturing tenant-scoped permit resolver.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns and clears the most-recently captured request.
    #[must_use]
    pub fn take_last_request(&self) -> Option<EvaluationRequest> {
        self.last_request.lock().expect("mutex").take()
    }
}

#[async_trait]
impl AuthZResolverApi for CapturingTenantPermitResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        let response = permit_scoped_to_request_tenant(&request);
        *self.last_request.lock().expect("mutex") = Some(request);
        Ok(response)
    }
}

/// PDP fake that permits but returns an EMPTY constraint set. With
/// `require_constraints(true)` the PEP fails this closed as
/// `EnforcerError::CompileFailed` (`empty_constraints`).
#[derive(Debug, Default)]
pub struct PermitEmptyConstraintsResolver;

#[async_trait]
impl AuthZResolverApi for PermitEmptyConstraintsResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: Vec::new(),
                deny_reason: None,
            },
        })
    }
}

/// PDP fake that denies every evaluation (`decision: false`), surfacing as
/// `EnforcerError::Denied` (`deny`).
#[derive(Debug, Default)]
pub struct DenyAllResolver;

#[async_trait]
impl AuthZResolverApi for DenyAllResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext {
                constraints: Vec::new(),
                deny_reason: None,
            },
        })
    }
}

/// PDP fake whose transport is unreachable: every evaluation returns
/// `AuthZResolverError::ServiceUnavailable`, surfacing as
/// `EnforcerError::EvaluationFailed` (`unreachable`).
#[derive(Debug, Default)]
pub struct UnreachableResolver;

#[async_trait]
impl AuthZResolverApi for UnreachableResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Err(CanonicalError::service_unavailable()
            .with_detail(
                "usage-collector test fake: simulated authz-resolver transport failure".to_owned(),
            )
            .create())
    }
}

/// PDP fake that combines an unreachable transport with a call counter,
/// so handler tests asserting a pre-service short-circuit can pin
/// `calls() == 0` as direct evidence the service path was never reached.
#[derive(Debug, Default)]
pub struct CountingUnreachableResolver {
    calls: AtomicUsize,
}

impl CountingUnreachableResolver {
    /// Build a counting unreachable resolver.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Number of `evaluate` calls observed so far.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AuthZResolverApi for CountingUnreachableResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(CanonicalError::service_unavailable()
            .with_detail(
                "usage-collector test fake: simulated authz-resolver transport failure".to_owned(),
            )
            .create())
    }
}

/// Wrap a `dyn AuthZResolverApi` in a `PolicyEnforcer` for tests, mirroring
/// the production `module.rs` wiring. No capabilities are advertised, as in
/// production: `usage_record` is a flat resource that does not advertise
/// `Capability::TenantHierarchy`, so the PDP expands a caller's tenant closure
/// eagerly into a flat `OWNER_TENANT_ID In [..]` constraint rather than an
/// `InTenantSubtree` predicate. A test needing a hierarchy-aware enforcer must
/// build one explicitly.
#[must_use]
pub fn enforcer_for(authz: Arc<dyn AuthZResolverApi>) -> PolicyEnforcer {
    PolicyEnforcer::new(authz)
}

// ── Plugin Host wiring helpers (`ClientHub` + scoped plugin) ────────────────

use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit_security::SecurityContext;
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
use usage_collector_sdk::UsageCollectorPluginSpecV1;

use crate::domain::Service;
use crate::domain::meter_reverse::MeterReverseResolver;
use crate::domain::ports::declaration_mirror::{
    DeclarationMirror, MirrorError, NoopDeclarationMirror,
};
use crate::domain::ports::declarations::{
    UnavailableDeclarationRegistrar, UnavailableDeclarationSource,
};
use crate::domain::type_resolver::{TypeResolver, TypeResolverConfig};

/// A Type Resolver with no working backend, for `Service` test builders in
/// this module that don't exercise type resolution at all (the plugin-host /
/// PDP / metrics paths). Mirrors `Service::new`'s own default — see
/// [`UnavailableDeclarationSource`] for why a domain-only placeholder is used
/// here rather than the real `types-registry` adapter (that would require
/// this domain-layer module to import a concrete `infra` type).
#[must_use]
fn inert_type_resolver(metrics: Arc<dyn UsageCollectorMetrics>) -> Arc<TypeResolver> {
    Arc::new(TypeResolver::new(
        Arc::new(UnavailableDeclarationSource),
        TypeResolverConfig {
            ttl: std::time::Duration::from_secs(1),
            capacity: 1,
        },
        metrics,
    ))
}

/// Build a usage-collector storage-plugin instance id under the schema
/// prefix advertised by [`UsageCollectorPluginSpecV1`], with `suffix` as
/// the five-token instance tail (e.g.
/// `"test.usage_collector.recording.plugin.v1"`).
#[must_use]
pub fn usage_collector_instance_id(suffix: &str) -> String {
    format!("{}{suffix}", UsageCollectorPluginSpecV1::gts_type_id())
}

fn plugin_instance_content(gts_id: &str, vendor: &str) -> serde_json::Value {
    serde_json::json!({
        "id": gts_id,
        "vendor": vendor,
        "priority": 0,
        "properties": {}
    })
}

/// Wire a fresh [`ClientHub`] with a `MockTypesRegistryClient` advertising
/// one usage-collector plugin instance and a scoped client binding the
/// supplied `plugin` under that instance id.
#[must_use]
pub fn hub_with_plugin(
    plugin: Arc<dyn UsageCollectorPluginV1>,
    suffix: &str,
    vendor: &str,
) -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::default());
    let instance_id = usage_collector_instance_id(suffix);
    let instance = make_test_instance(&instance_id, plugin_instance_content(&instance_id, vendor));
    let registry: Arc<dyn TypesRegistryClient> =
        Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    hub.register::<dyn TypesRegistryClient>(registry);
    hub.register_scoped::<dyn UsageCollectorPluginV1>(ClientScope::gts_id(&instance_id), plugin);
    hub
}

/// Fixture parameters for building a test [`Service`] against a
/// caller-supplied plugin stub (registered under the `constructorfabric` vendor).
///
/// Each independent axis — the Type Resolver's [`DeclarationSource`], the PDP
/// fake behind the enforcer, the metrics sink — is a setter rather than a
/// suffix in a function name, so a new axis is a new method. Every terminal
/// method funnels through the single private [`build_service`] constructor.
///
/// Defaults: the inert [`UnavailableDeclarationSource`] Type Resolver (see
/// [`inert_type_resolver`]), [`crate::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES`],
/// and an internally-built [`CountingTenantPermitResolver`], whose permit is
/// scoped to the request's own `OWNER_TENANT_ID` so per-record paths pass the
/// tenant gate for whatever tenant the record names.
#[derive(Default)]
pub(crate) struct ServiceFixture {
    source: Option<Arc<dyn DeclarationSource>>,
    cap: Option<usize>,
    resolver: Option<Arc<dyn AuthZResolverApi>>,
    max_batch_records: Option<usize>,
    ingestion_quota: Option<crate::config::IngestionQuotaConfig>,
    unavailable_retry_after_secs: Option<u64>,
    target_not_converged_retry_after_secs: Option<u64>,
    failing_mirror: bool,
    /// The reverse resolver the built service consults on its point read.
    ///
    /// `None` is [`fixture_reverse_resolver`], which answers back whatever
    /// reference this module's forward doubles resolved a meter to. A test
    /// whose subject *is* the resolver wires its own here.
    reverse_resolver: Option<Arc<MeterReverseResolver>>,
}

impl ServiceFixture {
    /// Wire a working Type Resolver over `source` instead of the inert
    /// default — for tests that must reach declaration resolution
    /// (`create_usage_records` and `query_aggregated_usage_records` both
    /// resolve the referenced meter's declaration before dispatch).
    #[must_use]
    pub(crate) fn with_source(mut self, source: Arc<dyn DeclarationSource>) -> Self {
        self.source = Some(source);
        self
    }

    /// Configure a non-default `metadata_size_cap_bytes`, for tests pinning
    /// that a non-default configured cap is actually honoured by the
    /// ingestion path.
    #[must_use]
    pub(crate) fn with_cap(mut self, cap: usize) -> Self {
        self.cap = Some(cap);
        self
    }

    /// Use `resolver` as the PDP fake instead of the default
    /// tenant-scoped [`CountingTenantPermitResolver`] — for tests that need
    /// a specific decision shape (deny, unreachable, a fixed constraint, …).
    #[must_use]
    pub(crate) fn with_resolver(mut self, resolver: Arc<dyn AuthZResolverApi>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// Wire a specific reverse resolver instead of the inert default — for
    /// the point-read tests, which are the only ones that reach it.
    #[must_use]
    pub(crate) fn with_reverse_resolver(mut self, resolver: Arc<MeterReverseResolver>) -> Self {
        self.reverse_resolver = Some(resolver);
        self
    }

    /// Configure a non-default per-request entry cap.
    #[must_use]
    pub(crate) fn with_max_batch_records(mut self, cap: usize) -> Self {
        self.max_batch_records = Some(cap);
        self
    }

    /// Swap the resolver's declaration mirror for one whose `upsert` always
    /// fails; a fixture that never calls this keeps the no-op default mirror.
    ///
    /// Drives `uc_declaration_mirror_write_failures_total`: a cold miss still
    /// resolves and the record is still accepted (ADR-0015 statement 4), so
    /// that counter is the only observable difference a failing mirror makes.
    #[must_use]
    pub(crate) fn with_failing_mirror(mut self) -> Self {
        self.failing_mirror = true;
        self
    }

    /// Configure a non-default per-subject ingestion allowance.
    ///
    /// The default is the shipped
    /// [`crate::config::IngestionQuotaConfig::default`] burst, so an ordinary
    /// fixture is quota-governed exactly as production is. A test about the
    /// quota uses this to make the allowance small enough to exhaust.
    #[must_use]
    pub(crate) fn with_ingestion_quota(
        mut self,
        quota: crate::config::IngestionQuotaConfig,
    ) -> Self {
        self.ingestion_quota = Some(quota);
        self
    }

    /// Build the `Service` against a `NoopMetrics` sink, discarding the PDP
    /// fake handle.
    #[must_use]
    pub(crate) fn build(
        self,
        plugin: Arc<dyn UsageCollectorPluginV1>,
        suffix: &str,
    ) -> Arc<Service> {
        build_service(self, plugin, suffix, Arc::new(NoopMetrics)).0
    }

    /// Build the `Service` against a `NoopMetrics` sink, exposing the
    /// default [`CountingTenantPermitResolver`] fake so the test can assert
    /// the exact number of PDP `evaluate` round-trips the service issued.
    ///
    /// Mutually exclusive with `.with_resolver(..)`.
    ///
    /// # Panics
    ///
    /// Panics (test-only) if `.with_resolver` overrode the default fake —
    /// there would be no `CountingTenantPermitResolver` handle to hand back.
    #[must_use]
    pub(crate) fn build_with_default_resolver_handle(
        self,
        plugin: Arc<dyn UsageCollectorPluginV1>,
        suffix: &str,
    ) -> (Arc<Service>, Arc<CountingTenantPermitResolver>) {
        assert!(
            self.resolver.is_none(),
            "build_with_default_resolver_handle expects the default \
             CountingTenantPermitResolver fake; an explicit .with_resolver() \
             call has nothing to hand back"
        );
        let counting = CountingTenantPermitResolver::new();
        let params = Self {
            resolver: Some(Arc::clone(&counting) as Arc<dyn AuthZResolverApi>),
            ..self
        };
        let (service, _) = build_service(params, plugin, suffix, Arc::new(NoopMetrics));
        (service, counting)
    }

    /// Build the `Service` against a real metrics adapter bound to a fresh
    /// local `SdkMeterProvider` + `InMemoryMetricExporter` pair, returned
    /// alongside the service.
    #[must_use]
    pub(crate) fn build_with_metrics(
        self,
        plugin: Arc<dyn UsageCollectorPluginV1>,
        suffix: &str,
    ) -> (Arc<Service>, SdkMeterProvider, InMemoryMetricExporter) {
        let (metrics, provider, exporter) = local_metrics();
        let (service, _) = build_service(self, plugin, suffix, metrics);
        (service, provider, exporter)
    }
}

/// The one private full-parameter constructor every [`ServiceFixture`]
/// terminal method funnels through: wires the Type Resolver (inert default
/// or working over `params.source`), the PDP fake (`params.resolver` or a
/// freshly built [`CountingTenantPermitResolver`]), the given `metrics` sink,
/// and `params.cap` (or the default), against `plugin` registered under the
/// `constructorfabric` vendor.
fn build_service(
    params: ServiceFixture,
    plugin: Arc<dyn UsageCollectorPluginV1>,
    suffix: &str,
    metrics: Arc<dyn UsageCollectorMetrics>,
) -> (Arc<Service>, Arc<dyn AuthZResolverApi>) {
    let failing_mirror = params.failing_mirror;
    let type_resolver = match params.source {
        Some(source) if failing_mirror => {
            type_resolver_over_with_failing_mirror(source, Arc::clone(&metrics))
        }
        Some(source) => type_resolver_over(source, Arc::clone(&metrics)),
        None => inert_type_resolver(Arc::clone(&metrics)),
    };
    let reverse_resolver = params
        .reverse_resolver
        .unwrap_or_else(|| fixture_reverse_resolver(Arc::clone(&metrics)));
    let resolver = params.resolver.unwrap_or_else(|| {
        Arc::clone(&CountingTenantPermitResolver::new()) as Arc<dyn AuthZResolverApi>
    });
    let hub = hub_with_plugin(plugin, suffix, "constructorfabric");
    let enforcer = enforcer_for(Arc::clone(&resolver));
    let service = Arc::new(Service::new_with_metrics(
        hub,
        "constructorfabric".to_owned(),
        enforcer,
        metrics,
        type_resolver,
        params
            .cap
            .unwrap_or(crate::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES),
        default_covered_period_bounds(),
        params
            .max_batch_records
            .unwrap_or(crate::domain::service::DEFAULT_MAX_BATCH_RECORDS),
        params.ingestion_quota.unwrap_or_default(),
        params
            .unavailable_retry_after_secs
            .unwrap_or(crate::domain::service::DEFAULT_UNAVAILABLE_RETRY_AFTER_SECS),
        params
            .target_not_converged_retry_after_secs
            .unwrap_or(crate::domain::service::DEFAULT_TARGET_NOT_CONVERGED_RETRY_AFTER_SECS),
        reverse_resolver,
    ));
    (service, resolver)
}

/// A [`DeclarationSource`] answering the reverse direction out of
/// [`declared_uuid`]'s own table.
///
/// The counterpart of this module's forward doubles rather than a double of
/// its own: whatever reference one of them resolved a meter to, this answers
/// back with that meter, so an ordinary fixture can drive the point read
/// without wiring a resolver per test.
///
/// The forward direction panics; a caller wanting one wires its own through
/// [`ServiceFixture::with_source`].
struct FixtureReverseDeclarationSource;

#[async_trait]
impl DeclarationSource for FixtureReverseDeclarationSource {
    async fn fetch(&self, id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError> {
        panic!("FixtureReverseDeclarationSource::fetch({id}) must not be reached");
    }

    async fn fetch_by_uuid(&self, type_uuid: Uuid) -> Result<GtsTypeSchema, DomainError> {
        let known = DECLARED_REFERENCES
            .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
            .lock()
            .ok()
            .and_then(|known| known.get(&type_uuid).cloned());
        match known {
            Some(id) => Ok(fake_meter_schema(&id, "SUM", &[])),
            None => Err(DomainError::DeclarationNotFound {
                gts_type_id: type_uuid.to_string(),
                reason: "is not declared under this registry reference".to_owned(),
            }),
        }
    }
}

/// The reverse resolver a [`ServiceFixture`] wires by default: one over
/// [`FixtureReverseDeclarationSource`], so the point read resolves whatever
/// reference this module's doubles answer with.
#[must_use]
fn fixture_reverse_resolver(metrics: Arc<dyn UsageCollectorMetrics>) -> Arc<MeterReverseResolver> {
    Arc::new(MeterReverseResolver::new(
        Arc::new(FixtureReverseDeclarationSource),
        Arc::new(NoopDeclarationMirror),
        metrics,
    ))
}

/// A reverse resolver with no working backend, for the builders that mean
/// one.
///
/// Mirrors `Service::new`'s own default: an [`UnavailableDeclarationSource`]
/// over the no-op mirror, answering every reference with a transport failure.
/// Only `get_usage_record` consults it.
#[must_use]
pub(crate) fn inert_reverse_resolver(
    metrics: Arc<dyn UsageCollectorMetrics>,
) -> Arc<MeterReverseResolver> {
    Arc::new(MeterReverseResolver::new(
        Arc::new(UnavailableDeclarationSource),
        Arc::new(NoopDeclarationMirror),
        metrics,
    ))
}

/// A working Type Resolver over `source`, with a TTL generous enough that a
/// test's several service calls hit the same cached entry rather than
/// re-resolving.
#[must_use]
fn type_resolver_over(
    source: Arc<dyn DeclarationSource>,
    metrics: Arc<dyn UsageCollectorMetrics>,
) -> Arc<TypeResolver> {
    Arc::new(TypeResolver::new(
        source,
        TypeResolverConfig {
            ttl: std::time::Duration::from_mins(1),
            capacity: 16,
        },
        metrics,
    ))
}

/// A [`DeclarationMirror`] whose `upsert` always fails — the
/// `uc_declaration_mirror_write_failures_total` driving scenario's only
/// collaborator of interest. That scenario never reaches `read`, so `read`
/// answers the ordinary fail-closed "no row".
struct FailingDeclarationMirror;

#[async_trait]
impl DeclarationMirror for FailingDeclarationMirror {
    async fn upsert(
        &self,
        _id: &MeterTypeId,
        _type_uuid: uuid::Uuid,
        _document: &serde_json::Value,
    ) -> Result<(), MirrorError> {
        Err(MirrorError::new("scripted write failure"))
    }

    async fn read(&self, _id: &MeterTypeId) -> Result<Option<serde_json::Value>, MirrorError> {
        Ok(None)
    }

    async fn resolve_id(&self, _type_uuid: uuid::Uuid) -> Result<Option<MeterTypeId>, MirrorError> {
        Ok(None)
    }
}

/// A working Type Resolver over `source`, wired through
/// [`TypeResolver::with_rehydration`] with [`FailingDeclarationMirror`] —
/// [`ServiceFixture::with_failing_mirror`]'s resolver construction. The
/// registrar is [`UnavailableDeclarationRegistrar`]: this scenario never
/// reaches restore mode (the registry never answers not-found), so there is
/// nothing for it to register back.
#[must_use]
fn type_resolver_over_with_failing_mirror(
    source: Arc<dyn DeclarationSource>,
    metrics: Arc<dyn UsageCollectorMetrics>,
) -> Arc<TypeResolver> {
    Arc::new(TypeResolver::with_rehydration(
        source,
        Arc::new(FailingDeclarationMirror),
        Arc::new(UnavailableDeclarationRegistrar),
        TypeResolverConfig {
            ttl: std::time::Duration::from_mins(1),
            capacity: 16,
        },
        metrics,
    ))
}

// ── Metrics-instrumented Service builders + in-memory readback ──────────────
//
// These wire a `Service` with a real [`UcMetricsMeter`] bound to a local
// `SdkMeterProvider` + `InMemoryMetricExporter`, so emission tests can call a
// service method, `force_flush()` the returned provider, and read back the
// exported instruments. `opentelemetry_sdk` is a dev-dependency; this module
// is test-only.

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

use crate::infra::metrics::UcMetricsMeter;

/// Build a fresh local `SdkMeterProvider` + `InMemoryMetricExporter` and a
/// `UcMetricsMeter` (prefix `uc`) bound to it.
#[must_use]
pub fn local_metrics() -> (
    Arc<UcMetricsMeter>,
    SdkMeterProvider,
    InMemoryMetricExporter,
) {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    let metrics = Arc::new(UcMetricsMeter::new(
        &provider.meter("usage-collector"),
        "uc",
        crate::domain::service::DEFAULT_MAX_BATCH_RECORDS,
    ));
    (metrics, provider, exporter)
}

/// Wire a [`ClientHub`] whose types-registry advertises one usage-collector
/// plugin instance but does **not** register a scoped client under it, so
/// `Service::get_plugin` resolves an instance id yet fails with
/// `PluginUnavailable` — the structural-unready path.
#[must_use]
pub fn hub_registry_only(suffix: &str, vendor: &str) -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::default());
    let instance_id = usage_collector_instance_id(suffix);
    let instance = make_test_instance(&instance_id, plugin_instance_content(&instance_id, vendor));
    let registry: Arc<dyn TypesRegistryClient> =
        Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    hub.register::<dyn TypesRegistryClient>(registry);
    hub
}

/// A metrics-instrumented [`Service`] whose plugin binding is structurally
/// unready (registry advertises an instance, no scoped client registered).
///
/// The Type Resolver is a working one over
/// [`fake_declaration_source_with_fold`] rather than the inert default,
/// because the feed read path resolves its subscription *before*
/// `resolve_plugin_for`: an inert resolver would fail the dispatch closed on
/// `ServiceUnavailable` at the wrong step and never reach the unready plugin
/// binding this fixture exists to exercise.
#[must_use]
pub fn service_with_metrics_unready_plugin(
    suffix: &str,
    resolver: Arc<dyn AuthZResolverApi>,
) -> (Arc<Service>, SdkMeterProvider, InMemoryMetricExporter) {
    let hub = hub_registry_only(suffix, "constructorfabric");
    let (metrics, provider, exporter) = local_metrics();
    let type_resolver = type_resolver_over(
        fake_declaration_source_with_fold("SUM"),
        Arc::clone(&metrics) as Arc<dyn UsageCollectorMetrics>,
    );
    // Inert: no caller of this fixture reaches the point read, which is the
    // only path that consults it.
    let reverse_resolver =
        inert_reverse_resolver(Arc::clone(&metrics) as Arc<dyn UsageCollectorMetrics>);
    let service = Arc::new(Service::new_with_metrics(
        hub,
        "constructorfabric".to_owned(),
        enforcer_for(resolver),
        metrics,
        type_resolver,
        crate::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES,
        default_covered_period_bounds(),
        crate::domain::service::DEFAULT_MAX_BATCH_RECORDS,
        crate::config::IngestionQuotaConfig::default(),
        crate::domain::service::DEFAULT_UNAVAILABLE_RETRY_AFTER_SECS,
        crate::domain::service::DEFAULT_TARGET_NOT_CONVERGED_RETRY_AFTER_SECS,
        reverse_resolver,
    ));
    (service, provider, exporter)
}

/// Total summed value of a `u64` counter series, filtered to the data points
/// carrying `label_key == label_value`. Returns `0` when the instrument or
/// label is absent.
#[must_use]
pub fn counter_sum_with_label(
    exporter: &InMemoryMetricExporter,
    name: &str,
    label_key: &str,
    label_value: &str,
) -> u64 {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data()
                {
                    return sum
                        .data_points()
                        .filter(|dp| {
                            dp.attributes().any(|kv| {
                                kv.key.as_str() == label_key && kv.value.as_str() == label_value
                            })
                        })
                        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                        .sum();
                }
            }
        }
    }
    0
}

/// Total summed value of a `u64` counter series across **every** data point,
/// unfiltered. `0` when the instrument is absent.
///
/// The unlabelled counterpart to [`counter_sum_with_label`], for the
/// instruments DESIGN §3.11.5 gives a label column of `—`: there is no label
/// to filter on, and a filtered read of one would silently answer `0`.
#[must_use]
pub fn counter_sum(exporter: &InMemoryMetricExporter, name: &str) -> u64 {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data()
                {
                    return sum
                        .data_points()
                        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                        .sum();
                }
            }
        }
    }
    0
}

/// Every `(label_key, label_value)` pair a `u64` counter emitted, across all
/// of its data points. Empty when the instrument is absent **or** when it is
/// present and unlabelled — the two are told apart by reading
/// [`counter_sum`] alongside.
///
/// The readback the §8.2 vocabulary pin is built on: it reports what the
/// recorder *emitted*, so a pin written against it cannot restate §3.11.5's
/// rows back to itself.
#[must_use]
pub fn counter_label_pairs(
    exporter: &InMemoryMetricExporter,
    name: &str,
) -> std::collections::BTreeSet<(String, String)> {
    let mut pairs = std::collections::BTreeSet::new();
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data()
                {
                    for dp in sum.data_points() {
                        for kv in dp.attributes() {
                            pairs.insert((kv.key.to_string(), kv.value.to_string()));
                        }
                    }
                }
            }
        }
    }
    pairs
}

/// Every `(label_key, label_value)` pair an `i64` gauge emitted.
///
/// The gauge twin of [`counter_label_pairs`], needed because
/// `uc_ingestion_quota_buckets_active` is a gauge: the §8.2 pin must read its
/// labels off the gauge's own data points.
#[must_use]
pub fn gauge_label_pairs(
    exporter: &InMemoryMetricExporter,
    name: &str,
) -> std::collections::BTreeSet<(String, String)> {
    let mut pairs = std::collections::BTreeSet::new();
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::I64(MetricData::Gauge(g)) = metric.data()
                {
                    for dp in g.data_points() {
                        for kv in dp.attributes() {
                            pairs.insert((kv.key.to_string(), kv.value.to_string()));
                        }
                    }
                }
            }
        }
    }
    pairs
}

/// How many data points a `u64` counter emitted. `0` when the instrument is
/// absent.
///
/// An absence oracle a summed value cannot provide: a counter incremented by
/// zero and one never touched both sum to `0`.
#[must_use]
pub fn counter_points(exporter: &InMemoryMetricExporter, name: &str) -> usize {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data()
                {
                    return sum.data_points().count();
                }
            }
        }
    }
    0
}

/// Total sample count across all data points of an `f64` histogram. Returns
/// `0` when the instrument is absent.
#[must_use]
pub fn histogram_count(exporter: &InMemoryMetricExporter, name: &str) -> u64 {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    return h
                        .data_points()
                        .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::count)
                        .sum();
                }
            }
        }
    }
    0
}

/// Total sample count across the `f64` histogram data points carrying
/// `label_key == label_value`. `0` when the instrument or label is absent. Use
/// this rather than [`histogram_count`] when one call drives several
/// dispatches through the same instrument and the assertion pins one series.
#[must_use]
pub fn histogram_count_with_label(
    exporter: &InMemoryMetricExporter,
    name: &str,
    label_key: &str,
    label_value: &str,
) -> u64 {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    return h
                        .data_points()
                        .filter(|dp| {
                            dp.attributes().any(|kv| {
                                kv.key.as_str() == label_key && kv.value.as_str() == label_value
                            })
                        })
                        .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::count)
                        .sum();
                }
            }
        }
    }
    0
}

/// Sum of every value recorded into an `f64` histogram. `0.0` when the
/// instrument is absent. Pins the observed *magnitude*, where
/// [`histogram_count`] pins the sample count.
#[must_use]
pub fn histogram_sum(exporter: &InMemoryMetricExporter, name: &str) -> f64 {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    return h
                        .data_points()
                        .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::sum)
                        .sum();
                }
            }
        }
    }
    0.0
}

/// Sum of the values recorded into the `f64` histogram data points carrying
/// `label_key == label_value`. `0.0` when the instrument or label is absent.
/// Pins the magnitude of one label series, where
/// [`histogram_count_with_label`] pins its sample count.
#[must_use]
pub fn histogram_sum_with_label(
    exporter: &InMemoryMetricExporter,
    name: &str,
    label_key: &str,
    label_value: &str,
) -> f64 {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    return h
                        .data_points()
                        .filter(|dp| {
                            dp.attributes().any(|kv| {
                                kv.key.as_str() == label_key && kv.value.as_str() == label_value
                            })
                        })
                        .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::sum)
                        .sum();
                }
            }
        }
    }
    0.0
}

/// Last recorded value of an `i64` gauge series. `None` when absent.
#[must_use]
pub fn gauge_last(exporter: &InMemoryMetricExporter, name: &str) -> Option<i64> {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::I64(MetricData::Gauge(g)) = metric.data()
                {
                    return g
                        .data_points()
                        .next()
                        .map(opentelemetry_sdk::metrics::data::GaugeDataPoint::value);
                }
            }
        }
    }
    None
}

/// Every instrument name present in the exporter's collected metrics.
///
/// The inventory pin needs the *set* of emitted names rather than one name's
/// value. Read after `provider.force_flush()`, like every sibling here.
#[must_use]
pub fn exported_instrument_names(
    exporter: &InMemoryMetricExporter,
) -> std::collections::BTreeSet<String> {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    let mut names = std::collections::BTreeSet::new();
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                names.insert(metric.name().to_owned());
            }
        }
    }
    names
}

/// Build an authenticated [`SecurityContext`] sufficient for PDP requests
/// composed from a [`UsageRecord`]'s attribution tuple.
#[must_use]
pub fn authenticated_ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(1))
        .subject_tenant_id(Uuid::from_u128(2))
        .subject_type("user")
        .build()
        .expect("authenticated context")
}

// ── HappyPathPlugin: programmable Ok-by-default SPI stub ────────────────────

use std::sync::Mutex;

/// The keyset order an SPI dispatch was handed, as `(field, direction)`
/// pairs. Recorded rather than the whole `ODataOrderBy` so an assertion is
/// a plain `assert_eq!` on comparable values — `OrderKey` carries no
/// `PartialEq`.
pub type RecordedOrder = Vec<(String, toolkit_odata::SortDir)>;

/// Per-record outcome shape for a `create_usage_records` SPI batch
/// response — `Ok(persisted_record)` or
/// `Err(UsageCollectorPluginError)`. Factored out so the
/// `HappyPathPlugin` field type and the `set_create_records` parameter
/// type stay readable.
pub type CreateRecordsBatchResult = Vec<Result<UsageRecord, UsageCollectorPluginError>>;

/// The one `read_feed_page` outcome a test programs, named so the slot that
/// holds it does not restate a four-parameter type inside two `Option`s.
pub type ProgrammedFeedPage =
    Result<FeedPage<FeedPosition, StoredUsageRecord>, UsageCollectorPluginError>;

/// A programmed `Transient` held as its `(detail, retry_after_seconds)`
/// parts, because [`UsageCollectorPluginError`] is not `Clone` and a
/// deduped fan-out may surface the same fault more than once.
pub type ProgrammedTransient = (String, Option<u64>);

/// The `get_usage_record` half of [`HappyPathPlugin`], as its own type.
///
/// Its own type so the **precedence order** is written once here rather than
/// once per setter. [`Self::lookup`] consults, stopping at the first answer:
///
/// 1. the blanket transient — a store that cannot answer cannot report
///    absence either, so it deliberately outranks the not-found set;
/// 2. the per-id transient, for a batch mixing one unreadable target with a
///    readable one;
/// 3. the not-found set;
/// 4. the per-id row, so a batch resolving several targets answers
///    differently per id — with one shared row a mis-keyed lookup would
///    return the same row and every assertion would still pass;
/// 5. the shared fallback row.
///
/// Every call is recorded in [`Self::inputs`] first, so a dedup assertion
/// counts dispatches whatever answer they produced, and the `scope` is
/// captured so a test can prove a compiled PDP scope reached the plugin.
#[derive(Default)]
pub struct TargetLookupDouble {
    row: Mutex<Option<UsageRecord>>,
    row_by_id: Mutex<std::collections::BTreeMap<Uuid, UsageRecord>>,
    not_found: Mutex<std::collections::BTreeSet<Uuid>>,
    transient: Mutex<Option<ProgrammedTransient>>,
    transient_by_id: Mutex<std::collections::BTreeMap<Uuid, ProgrammedTransient>>,
    inputs: Mutex<Vec<Uuid>>,
    not_converged: Mutex<std::collections::BTreeSet<Uuid>>,
    converged_only: Mutex<Vec<bool>>,
    /// The most-recent `scope`, kept as the AST rather than a rendering of
    /// it: a `Debug` string can only be substring-matched, which lets a scope
    /// no backend could translate (a bare `Expr::Value(Bool(true))`) pass
    /// unnoticed. Holding the `Expr` lets a test hand it to
    /// `convert_expr_to_filter_node`. [`Self::last_scope`] still renders it
    /// for callers that only want the string.
    last_scope: Mutex<Option<ast::Expr>>,
}

/// Whether a programmed row satisfies the scope a lookup carries, for the one
/// shape the gateway compiles from a single-tenant permit: `tenant_id eq <uuid>`.
/// Any other shape admits, so tests that do not exercise scope are unaffected.
fn target_scope_admits(scope: &ast::Expr, row: &UsageRecord) -> bool {
    match scope {
        ast::Expr::Compare(field, ast::CompareOperator::Eq, value) => {
            match (field.as_ref(), value.as_ref()) {
                (ast::Expr::Identifier(name), ast::Expr::Value(ast::Value::Uuid(tenant)))
                    if name == "tenant_id" =>
                {
                    row.tenant_id == *tenant
                }
                _ => true,
            }
        }
        _ => true,
    }
}

impl TargetLookupDouble {
    /// The shared fallback row, step 5 of the precedence order.
    pub fn set_row(&self, record: UsageRecord) {
        *self.row.lock().expect("mutex") = Some(record);
    }
    /// A row for one id, step 4.
    pub fn set_row_for(&self, id: Uuid, record: UsageRecord) {
        self.row_by_id.lock().expect("mutex").insert(id, record);
    }
    /// Mark `id` absent, step 3.
    pub fn set_not_found(&self, id: Uuid) {
        self.not_found.lock().expect("mutex").insert(id);
    }
    /// A retryable fault on every id, step 1. Outranks the not-found set
    /// and both row knobs, so a test relying on those must not set this.
    pub fn set_transient(&self, detail: &str, retry_after_seconds: Option<u64>) {
        *self.transient.lock().expect("mutex") = Some((detail.to_owned(), retry_after_seconds));
    }
    /// The same fault for one id, step 2.
    pub fn set_transient_for(&self, id: Uuid, detail: &str, retry_after_seconds: Option<u64>) {
        self.transient_by_id
            .lock()
            .expect("mutex")
            .insert(id, (detail.to_owned(), retry_after_seconds));
    }
    /// Mark `id` not yet converged: a converged-only lookup of it answers
    /// `UsageRecordNotConverged`. Checked after the transient knobs and before
    /// the not-found set.
    pub fn set_not_converged(&self, id: Uuid) {
        self.not_converged.lock().expect("mutex").insert(id);
    }
    /// The `converged_only` flag of every lookup, in call order.
    #[must_use]
    pub fn converged_only_flags(&self) -> Vec<bool> {
        self.converged_only.lock().expect("mutex").clone()
    }
    /// Every `id` handed to [`Self::lookup`], in call order.
    #[must_use]
    pub fn inputs(&self) -> Vec<Uuid> {
        self.inputs.lock().expect("mutex").clone()
    }
    /// Number of [`Self::lookup`] calls so far.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.inputs.lock().expect("mutex").len()
    }
    /// `Debug` rendering of the most-recent `scope`, or `None` before the
    /// first call.
    #[must_use]
    pub fn last_scope(&self) -> Option<String> {
        self.last_scope
            .lock()
            .expect("mutex")
            .as_ref()
            .map(|scope| format!("{scope:?}"))
    }

    /// The most-recent `scope` itself, or `None` before the first call.
    ///
    /// For the assertion [`Self::last_scope`] cannot support: whether a
    /// conforming backend could translate what the service handed the SPI.
    #[must_use]
    pub fn last_scope_expr(&self) -> Option<ast::Expr> {
        self.last_scope.lock().expect("mutex").clone()
    }

    /// Record the call, then answer it by the precedence order on this
    /// type's doc.
    ///
    /// # Errors
    ///
    /// The programmed fault, if any: a transient (steps 1-2), a not-found
    /// (step 3), or `not_programmed` when no row was ever set.
    pub fn lookup(
        &self,
        id: Uuid,
        scope: &ast::Expr,
        converged_only: bool,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        self.inputs.lock().expect("mutex").push(id);
        self.converged_only
            .lock()
            .expect("mutex")
            .push(converged_only);
        *self.last_scope.lock().expect("mutex") = Some(scope.clone());
        if let Some((detail, retry_after_seconds)) = self.transient.lock().expect("mutex").clone() {
            return Err(UsageCollectorPluginError::transient_with_retry(
                detail,
                retry_after_seconds,
            ));
        }
        if let Some((detail, retry_after_seconds)) = self
            .transient_by_id
            .lock()
            .expect("mutex")
            .get(&id)
            .cloned()
        {
            return Err(UsageCollectorPluginError::transient_with_retry(
                detail,
                retry_after_seconds,
            ));
        }
        if converged_only && self.not_converged.lock().expect("mutex").contains(&id) {
            return Err(UsageCollectorPluginError::UsageRecordNotConverged { id });
        }
        if self.not_found.lock().expect("mutex").contains(&id) {
            return Err(UsageCollectorPluginError::UsageRecordNotFound { id });
        }
        let row = match self.row_by_id.lock().expect("mutex").get(&id) {
            Some(row) => Some(row.clone()),
            None => self.row.lock().expect("mutex").clone(),
        };
        match row {
            // Answered in the storage shape under the reference this
            // module's declaration doubles resolve the meter to, so the gear
            // can re-attach it rather than refuse it.
            Some(row) if target_scope_admits(scope, &row) => {
                let gts_type_uuid = declared_uuid(&row.gts_type_id);
                Ok(row.into_stored(gts_type_uuid))
            }
            Some(_) => Err(UsageCollectorPluginError::UsageRecordNotFound { id }),
            None => Err(not_programmed("get_usage_record")),
        }
    }
}

/// Programmable plugin stub that returns the configured response for each
/// SPI method, defaulting to `UsageCollectorPluginError::internal("not
/// programmed")` for methods the test has not explicitly set up. Methods
/// also record their last-seen input so handler-level tests can verify
/// the service forwarded the expected argument.
///
/// The stub is `Arc<Self>` everywhere; interior state lives behind
/// `Mutex` so callers can program responses after construction.
pub struct HappyPathPlugin {
    create_records_response: Mutex<Option<CreateRecordsBatchResult>>,
    /// The whole `get_usage_record` half, in its own type. See
    /// [`TargetLookupDouble`], which owns the precedence order.
    target_lookup: TargetLookupDouble,
    list_usage_records_response: Mutex<Option<RecordPage>>,
    query_aggregated_usage_records_response: Mutex<Option<AggregationResult>>,
    /// Every [`AggregationFold`] passed to `query_aggregated_usage_records`,
    /// in call order. `len()` is the call count ([`HappyPathPlugin::calls`])
    /// and the last entry is [`HappyPathPlugin::last_fold`] — the
    /// [`RecordingPlugin`] spy shape for the declared-fold tests.
    query_aggregated_usage_records_folds: Mutex<Vec<AggregationFold>>,

    create_records_input: Mutex<Option<Vec<UsageRecord>>>,

    /// The `time_range` passed to the most-recent `list_usage_records`
    /// dispatch. The range is a typed parameter rather than a `$filter`
    /// conjunct, so nothing in the `ODataQuery` a test inspects would
    /// reveal a range the gateway dropped on the way to the SPI — and a
    /// dropped range is an unbounded scan that still answers `200`.
    list_time_range: Mutex<Option<TimeRange>>,
    /// The same recorder for `query_aggregated_usage_records`.
    aggregate_time_range: Mutex<Option<TimeRange>>,
    /// The whole [`ODataQuery`] the most-recent
    /// `query_aggregated_usage_records` dispatch was handed, symmetric with
    /// [`Self::list_query`], so a test can read the composed `$filter` an
    /// aggregate dispatch carried rather than only that it returned `Ok`.
    aggregate_query: Mutex<Option<ODataQuery>>,
    /// The `query.order` the most-recent `list_usage_records` dispatch was
    /// handed, as `(field, direction)` pairs. The SPI documents this slot
    /// as a populated, uniform-direction, never-null keyset on every
    /// surface, and nothing in a returned page would show an order the
    /// gateway failed to floor — an unfloored order is a keyset the plugin
    /// cannot continue, or silently drops rows from.
    list_order: Mutex<Option<RecordedOrder>>,
    /// The whole [`ODataQuery`] the most-recent `list_usage_records`
    /// dispatch was handed, so a test can read the composed `$filter` and the
    /// order computed alongside it; a returned page shows neither. Also where
    /// a test confirms the gateway strips `query.cursor` and
    /// `query.filter_hash` to `None` before dispatch rather than leaving a
    /// plugin to ignore them.
    list_query: Mutex<Option<ODataQuery>>,
    /// The `keyset` the most-recent `list_usage_records` dispatch was handed:
    /// `None` on a first page, `Some` on a continuation, positionally aligned
    /// with [`Self::list_order`]. Recorded so a continuation test can assert
    /// the plugin received the boundary values the token carried.
    list_keyset: Mutex<Option<Keyset>>,
    /// The [`MeterRef`] the most-recent `list_usage_records` dispatch was
    /// handed. Proves the gear takes the reference from the resolved
    /// declaration's `type_uuid` rather than from anywhere else — a returned
    /// page shows neither it nor the identifier that rode alongside it.
    list_meter: Mutex<Option<MeterRef>>,

    /// The one `read_feed_page` outcome a test programs. `Option` inside
    /// `Result` order is deliberate and matches `create_records_response`:
    /// `UsageCollectorPluginError` is `!Clone`, so the slot is `take()`n and
    /// a second dispatch in one test is a visible `not_programmed` failure
    /// rather than a silent replay.
    read_feed_page_response: Mutex<Option<ProgrammedFeedPage>>,
    /// Everything the most-recent `read_feed_page` dispatch was handed.
    ///
    /// Recorded because a returned page reveals almost none of it: the
    /// gateway could drop, reorder or invent any of these without changing
    /// the page this double hands back, and a dropped `until` is a bounded
    /// replay that never ends.
    read_feed_page_input: Mutex<Option<FeedDispatch>>,
}

/// One recorded `read_feed_page` dispatch — see
/// [`HappyPathPlugin::last_read_feed_page_input`].
#[derive(Clone, Debug)]
pub(crate) struct FeedDispatch {
    /// The subscription slice the plugin was handed — the resolved
    /// references, not `FeedSubscription::types()`'s bare identifiers.
    pub types: Vec<MeterRef>,
    /// The compiled PDP scope the page was to be filtered by.
    ///
    /// Recorded for a stronger reason than the four beside it: this is the
    /// **authorization decision**. A page served under a constant-true
    /// expression, or under a scope compiled for another caller, is a
    /// cross-tenant read that answers `200` and is indistinguishable from a
    /// correct page everywhere else in this double, which replays its
    /// programmed page regardless of the scope.
    pub scope: ast::Expr,
    /// Where the page was asked to begin.
    pub start: FeedStart<FeedPosition>,
    /// The bound, if the caller supplied one.
    pub until: Option<FeedPosition>,
    /// The resolved page size (never the caller's raw `Option`).
    pub limit: u64,
}

impl HappyPathPlugin {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            create_records_response: Mutex::new(None),
            target_lookup: TargetLookupDouble::default(),
            list_usage_records_response: Mutex::new(None),
            query_aggregated_usage_records_response: Mutex::new(None),
            query_aggregated_usage_records_folds: Mutex::new(Vec::new()),
            create_records_input: Mutex::new(None),
            list_time_range: Mutex::new(None),
            aggregate_time_range: Mutex::new(None),
            aggregate_query: Mutex::new(None),
            list_order: Mutex::new(None),
            list_query: Mutex::new(None),
            list_keyset: Mutex::new(None),
            list_meter: Mutex::new(None),
            read_feed_page_response: Mutex::new(None),
            read_feed_page_input: Mutex::new(None),
        })
    }

    pub fn set_create_records(&self, results: CreateRecordsBatchResult) {
        *self.create_records_response.lock().expect("mutex") = Some(results);
    }
    /// The shared fallback row. Delegates to [`TargetLookupDouble`], which
    /// documents where it sits in the precedence order.
    pub fn set_get_record(&self, record: UsageRecord) {
        self.target_lookup.set_row(record);
    }
    /// Program `get_usage_record(id, _)` to answer with `record`, ahead of
    /// whatever [`Self::set_get_record`] set, so one batch can resolve
    /// several targets to *different* rows — which is what makes a mis-keyed
    /// lookup observable.
    pub fn set_get_record_for(&self, id: Uuid, record: UsageRecord) {
        self.target_lookup.set_row_for(id, record);
    }
    /// Mark `id` so the next (and every subsequent) `get_usage_record`
    /// call carrying it returns `UsageRecordNotFound` regardless of the
    /// shared row.
    pub fn set_get_usage_record_not_found(&self, id: Uuid) {
        self.target_lookup.set_not_found(id);
    }
    /// See [`TargetLookupDouble::set_not_converged`].
    pub fn set_get_usage_record_not_converged(&self, id: Uuid) {
        self.target_lookup.set_not_converged(id);
    }
    /// See [`TargetLookupDouble::converged_only_flags`].
    #[must_use]
    pub fn get_usage_record_converged_only_flags(&self) -> Vec<bool> {
        self.target_lookup.converged_only_flags()
    }
    /// Program every subsequent `get_usage_record` call to fail as
    /// `Transient`, whatever the id: a backend fault on the
    /// invalidation-target read, which must fail the submission rather than
    /// reject it. Outranks every other knob — see [`TargetLookupDouble`].
    pub fn set_get_usage_record_transient(&self, detail: &str, retry_after_seconds: Option<u64>) {
        self.target_lookup
            .set_transient(detail, retry_after_seconds);
    }
    /// The same fault for one id only, mirroring
    /// [`Self::set_get_usage_record_not_found`]'s shape.
    pub fn set_get_usage_record_transient_for(
        &self,
        id: Uuid,
        detail: &str,
        retry_after_seconds: Option<u64>,
    ) {
        self.target_lookup
            .set_transient_for(id, detail, retry_after_seconds);
    }
    /// Every record `id` passed to `get_usage_record`, in call order.
    #[must_use]
    pub fn get_usage_record_inputs(&self) -> Vec<Uuid> {
        self.target_lookup.inputs()
    }
    /// Total number of `get_usage_record` SPI dispatches so far.
    #[must_use]
    pub fn get_usage_record_calls(&self) -> usize {
        self.target_lookup.calls()
    }
    /// `Debug` rendering of the `scope` filter passed to the most-recent
    /// `get_usage_record` call, or `None` if never invoked. Proves the point
    /// lookup handed the plugin a compiled PDP scope (DESIGN §3.3).
    #[must_use]
    pub fn last_get_scope(&self) -> Option<String> {
        self.target_lookup.last_scope()
    }
    /// The `scope` filter passed to the most-recent `get_usage_record`
    /// call, as the AST rather than a rendering of it, or `None` if it was
    /// never invoked.
    ///
    /// Use this, not [`Self::last_get_scope`], when the question is whether a
    /// real backend could serve the scope: a `Debug` string answers only
    /// "does it mention X", which an untranslatable `Expr::Value(Bool(true))`
    /// passes.
    #[must_use]
    pub fn last_get_scope_expr(&self) -> Option<ast::Expr> {
        self.target_lookup.last_scope_expr()
    }
    pub fn set_list_usage_records_response(&self, page: RecordPage) {
        *self.list_usage_records_response.lock().expect("mutex") = Some(page);
    }
    pub fn set_query_aggregated_usage_records_response(&self, result: AggregationResult) {
        *self
            .query_aggregated_usage_records_response
            .lock()
            .expect("mutex") = Some(result);
    }
    /// Total number of `query_aggregated_usage_records` SPI dispatches so
    /// far — the [`RecordingPlugin`] spy's call counter.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.query_aggregated_usage_records_folds
            .lock()
            .expect("mutex")
            .len()
    }
    /// The most-recent [`AggregationFold`] passed to
    /// `query_aggregated_usage_records`, or `None` if it was never invoked.
    #[must_use]
    pub fn last_fold(&self) -> Option<AggregationFold> {
        self.query_aggregated_usage_records_folds
            .lock()
            .expect("mutex")
            .last()
            .copied()
    }
    /// The [`TimeRange`] handed to the most-recent `list_usage_records`
    /// dispatch, or `None` if it was never invoked. Proves the range
    /// survived the gateway as a typed parameter, not merely that the read
    /// returned `Ok`.
    #[must_use]
    pub fn last_list_time_range(&self) -> Option<TimeRange> {
        *self.list_time_range.lock().expect("mutex")
    }
    /// The [`TimeRange`] handed to the most-recent
    /// `query_aggregated_usage_records` dispatch, or `None` if it was never
    /// invoked.
    #[must_use]
    pub fn last_aggregate_time_range(&self) -> Option<TimeRange> {
        *self.aggregate_time_range.lock().expect("mutex")
    }
    /// The whole [`ODataQuery`] handed to the most-recent
    /// `query_aggregated_usage_records` dispatch, or `None` if never invoked
    /// — symmetric with [`Self::last_list_query`]. A returned
    /// [`AggregationResult`] shows neither the `$filter` nor the scope.
    #[must_use]
    pub fn last_aggregate_query(&self) -> Option<ODataQuery> {
        self.aggregate_query.lock().expect("mutex").clone()
    }
    /// The whole [`ODataQuery`] handed to the most-recent
    /// `list_usage_records` dispatch, or `None` if it was never invoked.
    /// See [`HappyPathPlugin::list_query`] for why the projections next
    /// door are not enough.
    #[must_use]
    pub fn last_list_query(&self) -> Option<ODataQuery> {
        self.list_query.lock().expect("mutex").clone()
    }

    /// The [`Keyset`] handed to the most-recent `list_usage_records`
    /// dispatch; `None` if never invoked or if the dispatch was a first page.
    /// Proves the token's own boundary values reached the plugin.
    #[must_use]
    pub fn last_list_keyset(&self) -> Option<Keyset> {
        self.list_keyset.lock().expect("mutex").clone()
    }

    /// The [`MeterRef`] handed to the most-recent `list_usage_records`
    /// dispatch, or `None` if it was never invoked. Proves the reference
    /// reached the plugin, not merely that the call was dispatched.
    #[must_use]
    pub fn last_list_meter(&self) -> Option<MeterRef> {
        self.list_meter.lock().expect("mutex").clone()
    }

    /// The keyset order handed to the most-recent `list_usage_records`
    /// dispatch as `(field, direction)` pairs, or `None` if it was never
    /// invoked. Proves the gateway floored the order the SPI requires,
    /// rather than merely that the read returned `Ok`.
    #[must_use]
    pub fn last_list_order(&self) -> Option<RecordedOrder> {
        self.list_order.lock().expect("mutex").clone()
    }
    /// Program the next `read_feed_page` dispatch to answer with `page`.
    pub fn set_read_feed_page(&self, page: FeedPage<FeedPosition, StoredUsageRecord>) {
        *self.read_feed_page_response.lock().expect("mutex") = Some(Ok(page));
    }

    /// Program the next `read_feed_page` dispatch to fail with `err` — the
    /// retention refusal above all, which is a plugin-raised outcome the
    /// gateway can only pass along.
    pub fn set_read_feed_page_err(&self, err: UsageCollectorPluginError) {
        *self.read_feed_page_response.lock().expect("mutex") = Some(Err(err));
    }

    /// Everything the most-recent `read_feed_page` dispatch was handed, or
    /// `None` if it was never invoked — which is itself the assertion a
    /// "must not reach the plugin" test makes.
    #[must_use]
    pub(crate) fn last_read_feed_page_input(&self) -> Option<FeedDispatch> {
        self.read_feed_page_input.lock().expect("mutex").clone()
    }

    /// The `subscription` slice the most-recent `read_feed_page` dispatch was
    /// handed, or `None` if it was never invoked. A thin accessor over
    /// [`Self::last_read_feed_page_input`] for tests that only care about the
    /// resolved references and not the rest of the dispatch.
    #[must_use]
    pub fn last_subscription(&self) -> Option<Vec<MeterRef>> {
        self.last_read_feed_page_input()
            .map(|dispatch| dispatch.types)
    }

    pub fn last_create_records_input(&self) -> Option<Vec<UsageRecord>> {
        self.create_records_input.lock().expect("mutex").clone()
    }
}

fn not_programmed(method: &'static str) -> UsageCollectorPluginError {
    UsageCollectorPluginError::internal(format!("HappyPathPlugin::{method} not programmed"))
}

/// One programmed create outcome, in the storage shape the SPI answers with.
///
/// Tests program a [`UsageRecord`] — the shape they also read back off the
/// service — and this attaches the reference the dispatch carried, so the
/// gear's re-attachment check admits it. The fakes built to be refused name
/// their foreign reference deliberately ([`ConflictingPlugin`],
/// [`ForeignMeterPlugin`]).
fn stored_outcome(
    outcome: Result<UsageRecord, UsageCollectorPluginError>,
    meter: &MeterRef,
) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
    outcome.map(|record| record.into_stored(meter.uuid))
}

#[async_trait]
impl UsageCollectorPluginV1 for HappyPathPlugin {
    async fn create_usage_records(
        &self,
        records: Vec<(MeterRef, StoredUsageRecord)>,
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        let meters: Vec<MeterRef> = records.iter().map(|(meter, _)| meter.clone()).collect();
        *self.create_records_input.lock().expect("mutex") = Some(
            records
                .into_iter()
                .map(|(meter, record)| record.into_usage_record(meter.id))
                .collect(),
        );
        let programmed = self
            .create_records_response
            .lock()
            .expect("mutex")
            .take()
            .ok_or_else(|| not_programmed("create_usage_records"))?;
        // Each outcome answers under the meter its own entry was dispatched
        // with, so programming fewer outcomes than entries still produces the
        // alignment the gear's own size check reports.
        Ok(programmed
            .into_iter()
            .zip(meters)
            .map(|(outcome, meter)| stored_outcome(outcome, &meter))
            .collect())
    }

    async fn query_aggregated_usage_records(
        &self,
        _meter: &MeterRef,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        *self.aggregate_time_range.lock().expect("mutex") = Some(time_range);
        *self.aggregate_query.lock().expect("mutex") = Some(query.clone());
        self.query_aggregated_usage_records_folds
            .lock()
            .expect("mutex")
            .push(fold);
        self.query_aggregated_usage_records_response
            .lock()
            .expect("mutex")
            .clone()
            .ok_or_else(|| not_programmed("query_aggregated_usage_records"))
    }

    async fn list_usage_records(
        &self,
        meter: &MeterRef,
        time_range: TimeRange,
        query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        keyset: Option<&Keyset>,
    ) -> Result<RecordPage, UsageCollectorPluginError> {
        *self.list_time_range.lock().expect("mutex") = Some(time_range);
        *self.list_order.lock().expect("mutex") = Some(
            query
                .order
                .0
                .iter()
                .map(|key| (key.field.clone(), key.dir))
                .collect(),
        );
        *self.list_query.lock().expect("mutex") = Some(query.clone());
        *self.list_keyset.lock().expect("mutex") = keyset.cloned();
        *self.list_meter.lock().expect("mutex") = Some(meter.clone());
        self.list_usage_records_response
            .lock()
            .expect("mutex")
            .clone()
            .ok_or_else(|| not_programmed("list_usage_records"))
    }

    async fn get_usage_record(
        &self,
        id: Uuid,
        scope: &ast::Expr,
        converged_only: bool,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        self.target_lookup.lookup(id, scope, converged_only)
    }

    /// Records the whole dispatch, then replays the one programmed outcome.
    async fn read_feed_page(
        &self,
        subscription: &[MeterRef],
        scope: &ast::Expr,
        start: FeedStart<FeedPosition>,
        until: Option<FeedPosition>,
        limit: u64,
    ) -> Result<FeedPage<FeedPosition, StoredUsageRecord>, UsageCollectorPluginError> {
        *self.read_feed_page_input.lock().expect("mutex") = Some(FeedDispatch {
            types: subscription.to_vec(),
            scope: scope.clone(),
            start,
            until,
            limit,
        });
        match self.read_feed_page_response.lock().expect("mutex").take() {
            Some(outcome) => outcome,
            None => Err(not_programmed("read_feed_page")),
        }
    }

    /// Unprogrammed: no test points this double at reconciliation.
    async fn get_reconciliation_metadata(
        &self,
        _tenant_id: Uuid,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _fold: AggregationFold,
        _scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        Err(not_programmed("get_reconciliation_metadata"))
    }
}

/// Spy alias over [`HappyPathPlugin`] for the declared-fold aggregate
/// tests: [`HappyPathPlugin::calls`] and
/// [`HappyPathPlugin::last_fold`] already record every
/// `query_aggregated_usage_records` dispatch, so this is a naming alias
/// rather than a second spy type.
pub(crate) type RecordingPlugin = HappyPathPlugin;

// ── FoldingPlugin: the in-memory reference for withdrawal exclusion ────────

use bigdecimal::BigDecimal;
use usage_collector_sdk::{AggregationBucket, Invalidation, ReasonCode, derive_usage_record_id};

/// In-memory storage double that actually folds, so the withdrawal
/// exclusion has something to be demonstrated against.
///
/// A separate type rather than a third name for [`HappyPathPlugin`], which
/// replays a programmed [`AggregationResult`] and so cannot show what a fold
/// does with a withdrawn pair. This one holds entries and computes the answer:
/// it selects with [`TimeRange::contains_window_end`], leaves out every entry
/// that is an invalidation **or** is named by one, and folds what is left
/// (`cpt-cf-usage-collector-adr-append-only-invalidation`).
///
/// It is a **fixture, not a backend** — the reference semantics DESIGN §3.3
/// requires of a conforming plugin. What it does **not** implement:
///
/// * **`group_by`** — every fold produces one bucket with an empty key,
///   whatever dimensions are requested.
/// * **The PDP scope and `query.filter`** — both ignored on every path.
///   Honouring either would need an `OData` evaluator this fixture does not
///   have, so a row outside the caller's scope is returned rather than
///   withheld, which a conforming plugin must not do.
/// * **`metadata_filter`, paging and `query.order`** — a page carries every
///   selected entry in insertion order and `next` is always `None`.
/// * **The create surface** — entries arrive through [`Self::store`] and
///   [`Self::store_withdrawn`]; `create_usage_records` refuses.
///
/// Empty-selection answers follow SQL: `COUNT` is zero and every other fold
/// is absent.
pub(crate) struct FoldingPlugin {
    stored: Mutex<Vec<UsageRecord>>,
}

impl FoldingPlugin {
    #[must_use]
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            stored: Mutex::new(Vec::new()),
        })
    }

    /// Append one entry to the ledger, exactly as persisted.
    pub(crate) fn store(&self, record: UsageRecord) {
        self.stored.lock().expect("mutex").push(record);
    }

    /// The invalidation that withdraws `record`, as it would be persisted.
    ///
    /// Built **from** the target rather than written out beside it, so the
    /// two cannot drift: a faithful copy departing only in the entry kind and
    /// the reason it carries. The idempotency key is copied like every other
    /// field, so the pair's `id`s differ in the entry-type input alone, and
    /// the quantity is echoed rather than negated.
    ///
    /// `origin` is a parameter rather than a copied field because it is
    /// server-assigned from the route the **withdrawal** travelled, which
    /// need not be its target's: withdrawing a closed period goes by the
    /// backfill route while the entry it retracts came in live. Inheriting it
    /// silently would make that mixed-route pair unreachable, so callers
    /// state it.
    ///
    /// Separate from [`Self::store_withdrawn`] because an orphan invalidation
    /// has no target to store.
    pub(crate) fn withdrawal_of(record: &UsageRecord, origin: RecordOrigin) -> UsageRecord {
        UsageRecord {
            id: derive_usage_record_id(
                record.tenant_id,
                &record.gts_type_id,
                &record.idempotency_key,
                record.window_start,
                record.window_end,
                EntryType::Invalidation,
            ),
            invalidation: Some(Invalidation {
                target: record.id,
                reason: ReasonCode::new("emitter_defect").expect("valid reason code"),
            }),
            origin,
            ..record.clone()
        }
    }

    /// Append `record` together with the invalidation that withdraws it,
    /// and hand back that invalidation as persisted.
    pub(crate) fn store_withdrawn(&self, record: UsageRecord) -> UsageRecord {
        // One call stands for one emitter correcting itself on the path it
        // is already using, so the withdrawal takes the target's origin. A
        // mixed-route pair is built from `withdrawal_of` + `store`.
        let withdrawal = Self::withdrawal_of(&record, record.origin);
        self.store(record);
        self.store(withdrawal.clone());
        withdrawal
    }

    /// Every entry `time_range` selects, in insertion order.
    fn selection(&self, time_range: TimeRange) -> Vec<UsageRecord> {
        self.stored
            .lock()
            .expect("mutex")
            .iter()
            .filter(|entry| time_range.contains_window_end(entry.window_end))
            .cloned()
            .collect()
    }

    /// [`Self::selection`] with both entries of every withdrawn pair left
    /// out — the invalidation, and the record it names.
    fn folded_selection(&self, time_range: TimeRange) -> Vec<UsageRecord> {
        let selected = self.selection(time_range);
        // Read off the selection rather than the whole ledger, as a
        // single-statement backend does. The two agree because a pair carries
        // one covered period, so no range takes one entry without the other.
        let withdrawn: std::collections::BTreeSet<Uuid> = selected
            .iter()
            .filter_map(|entry| entry.invalidation.as_ref().map(|inv| inv.target))
            .collect();
        selected
            .into_iter()
            .filter(|entry| entry.invalidation.is_none() && !withdrawn.contains(&entry.id))
            .collect()
    }
}

/// `quantity` as a [`BigDecimal`], through its decimal string so no binary
/// float sits between the two representations.
fn quantity_of(record: &UsageRecord) -> BigDecimal {
    record
        .quantity
        .to_string()
        .parse::<BigDecimal>()
        .expect("a Decimal renders as a parseable decimal string")
}

/// `fold` over `entries`, as the single bucket value a plugin would report.
///
/// `None` is the empty-selection answer for every fold but `COUNT`, which
/// counts zero — the same split SQL makes.
fn fold_over(fold: AggregationFold, entries: &[UsageRecord]) -> Option<BigDecimal> {
    match fold {
        AggregationFold::Count => Some(BigDecimal::from(
            u64::try_from(entries.len()).expect("a fixture holds a countable number of entries"),
        )),
        AggregationFold::Sum => (!entries.is_empty()).then(|| {
            entries.iter().fold(BigDecimal::from(0), |total, entry| {
                total + quantity_of(entry)
            })
        }),
        AggregationFold::Max => entries.iter().map(quantity_of).max(),
        AggregationFold::Min => entries.iter().map(quantity_of).min(),
        // The declared tie-break, all three keys: greatest `window_end`, then
        // `accepted_at`, then `id` in byte order (DESIGN section 3.1).
        // `Uuid`'s `Ord` is over the bytes, which is that order. Resolving on
        // `window_end` alone would answer differently from a conforming plugin
        // on exactly the population a tie-break exists for.
        AggregationFold::Latest => entries
            .iter()
            .max_by_key(|entry| (entry.window_end, entry.accepted_at, entry.id))
            .map(quantity_of),
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for FoldingPlugin {
    async fn create_usage_records(
        &self,
        _records: Vec<(MeterRef, StoredUsageRecord)>,
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        Err(UsageCollectorPluginError::internal(
            "FoldingPlugin admits no create: entries arrive through store()",
        ))
    }

    async fn get_usage_record(
        &self,
        id: Uuid,
        _scope: &ast::Expr,
        _converged_only: bool,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        self.stored
            .lock()
            .expect("mutex")
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| {
                let gts_type_uuid = declared_uuid(&entry.gts_type_id);
                entry.clone().into_stored(gts_type_uuid)
            })
            .ok_or(UsageCollectorPluginError::UsageRecordNotFound { id })
    }

    async fn query_aggregated_usage_records(
        &self,
        _meter: &MeterRef,
        time_range: TimeRange,
        fold: AggregationFold,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        let entries = self.folded_selection(time_range);
        Ok(AggregationResult {
            buckets: vec![AggregationBucket {
                key: Vec::new(),
                value: fold_over(fold, &entries),
            }],
        })
    }

    async fn list_usage_records(
        &self,
        meter: &MeterRef,
        time_range: TimeRange,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _keyset: Option<&Keyset>,
    ) -> Result<RecordPage, UsageCollectorPluginError> {
        Ok(RecordPage {
            items: self
                .selection(time_range)
                // Answered under the meter this read named, which is the
                // one meter this double's ledger holds.
                .into_iter()
                .map(|entry| entry.into_stored(meter.uuid))
                .collect(),
            next: None,
        })
    }

    /// This double exists for the withdrawal exclusion inside a fold; no
    /// test points it at the feed.
    async fn read_feed_page(
        &self,
        _subscription: &[MeterRef],
        _scope: &ast::Expr,
        _start: FeedStart<FeedPosition>,
        _until: Option<FeedPosition>,
        _limit: u64,
    ) -> Result<FeedPage<FeedPosition, StoredUsageRecord>, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "FoldingPlugin serves no feed page: it exists for the withdrawal exclusion inside a \
             fold",
        ))
    }

    /// Likewise unmodelled: no test points this double at reconciliation.
    async fn get_reconciliation_metadata(
        &self,
        _tenant_id: Uuid,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _fold: AggregationFold,
        _scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "FoldingPlugin reports no reconciliation metadata: it exists for the withdrawal \
             exclusion inside a fold",
        ))
    }
}

// ── DeclarationSource fakes: fold / metadata / not-found / counting ────────

use types_registry_sdk::{GtsTypeId, GtsTypeSchema};
use usage_collector_sdk::USAGE_RECORD_BASE_TYPE;

use crate::domain::error::DomainError;
use crate::domain::ports::declarations::DeclarationSource;
use crate::domain::ports::metrics::{NoopMetrics, UsageCollectorMetrics};

/// Builds a base + single-derived-meter `GtsTypeSchema` pair for `id`,
/// declaring `x-gts-traits` at the schema's **top level** (never nested in
/// `allOf` — that is the placement `GtsTypeSchema::effective_traits` reads;
/// see `type_resolver::declaration_tests` for the history) with
/// `aggregation_fold: fold`, `canonical_unit: "bytes"`, and a closed
/// `metadata` surface admitting exactly `metadata_keys`.
fn fake_meter_schema(id: &MeterTypeId, fold: &str, metadata_keys: &[String]) -> GtsTypeSchema {
    let base = GtsTypeSchema::try_new(
        GtsTypeId::try_new(USAGE_RECORD_BASE_TYPE).expect("base type id"),
        serde_json::json!({
            "type": "object",
            "x-gts-abstract": true,
            "properties": {
                "metadata": { "type": "object", "additionalProperties": { "type": "string" } }
            }
        }),
        None,
        None,
    )
    .expect("base schema");

    let properties: serde_json::Map<String, serde_json::Value> = metadata_keys
        .iter()
        .map(|key| (key.clone(), serde_json::json!({ "type": "string" })))
        .collect();

    GtsTypeSchema::try_new(
        id.as_gts().clone(),
        serde_json::json!({
            "allOf": [{ "$ref": format!("gts://{USAGE_RECORD_BASE_TYPE}") }],
            "x-gts-traits": {
                "aggregation_fold": fold,
                "canonical_unit": "bytes"
            },
            "properties": {
                "metadata": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": properties
                }
            }
        }),
        None,
        Some(Arc::new(base)),
    )
    .expect("derived schema")
}

/// A [`DeclarationSource`] that always resolves — regardless of the `id` it
/// is asked to `fetch` — to a meter declaring `fold`, unit `bytes`, and the
/// given closed `metadata_keys` surface.
///
/// `type_uuid`, when set, overrides the schema's own derived
/// `GtsTypeSchema::type_uuid`, so a test can pin a
/// `ResolvedDeclaration::type_uuid` disagreeing with what `GtsId::to_uuid`
/// would derive — the only way to tell "read off the declaration" from
/// "derived from the identifier" apart.
struct FakeDeclarationSource {
    fold: String,
    metadata_keys: Vec<String>,
    type_uuid: Option<Uuid>,
}

#[async_trait]
impl DeclarationSource for FakeDeclarationSource {
    async fn fetch(&self, id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError> {
        let schema = fake_meter_schema(id, &self.fold, &self.metadata_keys);
        Ok(match self.type_uuid {
            Some(type_uuid) => GtsTypeSchema {
                type_uuid,
                ..schema
            },
            None => schema,
        })
    }

    async fn fetch_by_uuid(&self, type_uuid: uuid::Uuid) -> Result<GtsTypeSchema, DomainError> {
        // No double in this module is reached through the reverse direction:
        // `MeterReverseResolver` has its own fixtures in `meter_reverse_tests`.
        unimplemented!("fetch_by_uuid({type_uuid}) is not reached by this double")
    }
}

/// A `DeclarationSource` that always resolves to a meter declaring `fold`,
/// unit `bytes` and no metadata properties.
#[must_use]
pub(crate) fn fake_declaration_source_with_fold(fold: &str) -> Arc<dyn DeclarationSource> {
    Arc::new(FakeDeclarationSource {
        fold: fold.to_owned(),
        metadata_keys: Vec::new(),
        type_uuid: None,
    })
}

/// A `DeclarationSource` resolving to a meter whose metadata surface declares
/// exactly `keys`.
#[must_use]
pub(crate) fn fake_declaration_source_with_metadata(keys: &[&str]) -> Arc<dyn DeclarationSource> {
    Arc::new(FakeDeclarationSource {
        fold: "SUM".to_owned(),
        metadata_keys: keys.iter().map(|k| (*k).to_owned()).collect(),
        type_uuid: None,
    })
}

/// A `DeclarationSource` resolving `meter`'s declaration with `type_uuid`
/// pinned as [`ResolvedDeclaration::type_uuid`](crate::domain::type_resolver::ResolvedDeclaration::type_uuid),
/// overriding what `GtsId::to_uuid()` would otherwise derive from `meter`
/// itself.
///
/// The fixture [`service_with_declaration_uuid`] needs: without a reference
/// that disagrees with the derivation the two are indistinguishable.
#[must_use]
pub(crate) fn fake_declaration_source_with_uuid(type_uuid: Uuid) -> Arc<dyn DeclarationSource> {
    Arc::new(FakeDeclarationSource {
        fold: "SUM".to_owned(),
        metadata_keys: Vec::new(),
        type_uuid: Some(type_uuid),
    })
}

/// A `Service` over `plugin` whose Type Resolver answers every declaration
/// with `type_uuid` pinned as `ResolvedDeclaration::type_uuid`, instead of
/// the value `GtsId::to_uuid()` would derive from `meter` itself.
///
/// `type_uuid` is a reference the derivation would never produce for `meter`,
/// so the plugin's recorded dispatch can only carry it if the service read
/// the declaration.
#[must_use]
pub(crate) fn service_with_declaration_uuid(
    plugin: Arc<dyn UsageCollectorPluginV1>,
    _meter: &MeterTypeId,
    type_uuid: Uuid,
) -> Arc<Service> {
    ServiceFixture::default()
        .with_source(fake_declaration_source_with_uuid(type_uuid))
        .with_resolver(recording_plugin_resolver())
        .build(plugin, RECORDING_PLUGIN_SUFFIX)
}

/// A [`DeclarationSource`] that resolves only the meters named in its table,
/// each to the paired `type_uuid`, and answers every other identifier with a
/// definite not-found.
///
/// [`NotFoundDeclarationSource`] cannot express "resolves some meters, not
/// others", and [`PerIdDeclarationSource`] panics on a meter it has no row
/// for rather than answering a declaration-not-found.
struct ResolvingOnlyDeclarationSource {
    resolvable: std::collections::HashMap<MeterTypeId, Uuid>,
}

#[async_trait]
impl DeclarationSource for ResolvingOnlyDeclarationSource {
    async fn fetch(&self, id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError> {
        match self.resolvable.get(id) {
            Some(&type_uuid) => Ok(GtsTypeSchema {
                type_uuid,
                ..fake_meter_schema(id, "SUM", &[])
            }),
            None => Err(DomainError::declaration_not_found(id)),
        }
    }

    async fn fetch_by_uuid(&self, type_uuid: uuid::Uuid) -> Result<GtsTypeSchema, DomainError> {
        // No double in this module is reached through the reverse direction:
        // `MeterReverseResolver` has its own fixtures in `meter_reverse_tests`.
        unimplemented!("fetch_by_uuid({type_uuid}) is not reached by this double")
    }
}

/// A `Service` over `plugin` whose Type Resolver resolves only the meters
/// named in `resolvable`, each to its paired reference, and fails closed
/// with a definite not-found on every other meter.
///
/// Built for the feed read path's all-or-nothing resolve: a subscription
/// naming one resolvable meter and one unresolvable must fail the whole read
/// before `plugin` is ever dispatched.
#[must_use]
pub(crate) fn service_resolving_only(
    plugin: Arc<dyn UsageCollectorPluginV1>,
    resolvable: &[(&MeterTypeId, Uuid)],
) -> Arc<Service> {
    let source: Arc<dyn DeclarationSource> = Arc::new(ResolvingOnlyDeclarationSource {
        resolvable: resolvable
            .iter()
            .map(|(id, type_uuid)| ((*id).clone(), *type_uuid))
            .collect(),
    });
    ServiceFixture::default()
        .with_source(source)
        .with_resolver(recording_plugin_resolver())
        .build(plugin, RECORDING_PLUGIN_SUFFIX)
}

/// A [`DeclarationSource`] that answers a DIFFERENT fold per identifier,
/// keyed by `MeterTypeId` -- unlike [`FakeDeclarationSource`], which answers
/// the same fold regardless of which id it is asked to `fetch`.
///
/// Built for `docs/features/usage-type-resolution.md` §6 criterion 13's third
/// clause: "two meters sharing a name prefix but declaring different folds
/// each resolve to their own".
struct PerIdDeclarationSource {
    folds: std::collections::HashMap<MeterTypeId, String>,
}

#[async_trait]
impl DeclarationSource for PerIdDeclarationSource {
    async fn fetch(&self, id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError> {
        let fold = self
            .folds
            .get(id)
            .unwrap_or_else(|| panic!("PerIdDeclarationSource has no fold scripted for {id}"));
        Ok(fake_meter_schema(id, fold, &[]))
    }

    async fn fetch_by_uuid(&self, type_uuid: uuid::Uuid) -> Result<GtsTypeSchema, DomainError> {
        // No double in this module is reached through the reverse direction:
        // `MeterReverseResolver` has its own fixtures in `meter_reverse_tests`.
        unimplemented!("fetch_by_uuid({type_uuid}) is not reached by this double")
    }
}

/// A `DeclarationSource` answering a different fold per identifier -- see
/// [`PerIdDeclarationSource`]'s own doc.
#[must_use]
pub(crate) fn fake_declaration_source_with_fold_by_id(
    folds: &[(MeterTypeId, &str)],
) -> Arc<dyn DeclarationSource> {
    Arc::new(PerIdDeclarationSource {
        folds: folds
            .iter()
            .map(|(id, fold)| (id.clone(), (*fold).to_owned()))
            .collect(),
    })
}

/// A [`DeclarationSource`] that always answers a definite not-found —
/// [`DomainError::is_declaration_not_found`] reports `true` for it, so the
/// Type Resolver fails closed immediately rather than treating it as a
/// possibly-transient miss.
struct NotFoundDeclarationSource;

#[async_trait]
impl DeclarationSource for NotFoundDeclarationSource {
    async fn fetch(&self, id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError> {
        Err(DomainError::declaration_not_found(id))
    }

    async fn fetch_by_uuid(&self, type_uuid: uuid::Uuid) -> Result<GtsTypeSchema, DomainError> {
        // No double in this module is reached through the reverse direction:
        // `MeterReverseResolver` has its own fixtures in `meter_reverse_tests`.
        unimplemented!("fetch_by_uuid({type_uuid}) is not reached by this double")
    }
}

/// A `DeclarationSource` that always answers a definite not-found.
#[must_use]
pub(crate) fn fake_declaration_source_not_found() -> Arc<dyn DeclarationSource> {
    Arc::new(NotFoundDeclarationSource)
}

/// A [`DeclarationSource`] counting its `fetch` calls (and recording the id
/// each one resolved), so a batch test can assert one resolution per distinct
/// meter rather than one per record. Resolves every `id` the way
/// [`FakeDeclarationSource`] does (fold `SUM`, unit `bytes`, no metadata
/// properties) — except `unresolvable`, when set, which always answers a
/// definite not-found, for a test driving "one distinct type resolves,
/// another does not" in the same batch.
pub(crate) struct CountingDeclarationSource {
    inner: FakeDeclarationSource,
    calls: AtomicUsize,
    inputs: Mutex<Vec<MeterTypeId>>,
    unresolvable: Option<MeterTypeId>,
}

impl CountingDeclarationSource {
    /// Total number of `fetch` calls observed so far.
    #[must_use]
    pub(crate) fn fetch_calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Every id passed to `fetch`, in call order.
    #[must_use]
    pub(crate) fn fetch_inputs(&self) -> Vec<MeterTypeId> {
        self.inputs.lock().expect("mutex").clone()
    }
}

#[async_trait]
impl DeclarationSource for CountingDeclarationSource {
    async fn fetch(&self, id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inputs.lock().expect("mutex").push(id.clone());
        if self.unresolvable.as_ref() == Some(id) {
            return Err(DomainError::declaration_not_found(id));
        }
        self.inner.fetch(id).await
    }

    async fn fetch_by_uuid(&self, type_uuid: uuid::Uuid) -> Result<GtsTypeSchema, DomainError> {
        // No double in this module is reached through the reverse direction:
        // `MeterReverseResolver` has its own fixtures in `meter_reverse_tests`.
        unimplemented!("fetch_by_uuid({type_uuid}) is not reached by this double")
    }
}

/// A `DeclarationSource` resolving every id to a meter declaring `SUM` /
/// `bytes` / no metadata properties, except `unresolvable`, which always
/// answers a definite not-found.
#[must_use]
pub(crate) fn fake_declaration_source_with_one_unresolvable(
    unresolvable: MeterTypeId,
) -> Arc<CountingDeclarationSource> {
    Arc::new(CountingDeclarationSource {
        inner: FakeDeclarationSource {
            fold: "SUM".to_owned(),
            metadata_keys: Vec::new(),
            type_uuid: None,
        },
        calls: AtomicUsize::new(0),
        inputs: Mutex::new(Vec::new()),
        unresolvable: Some(unresolvable),
    })
}

/// A `DeclarationSource` counting its calls, so a batch test can assert one
/// resolution per distinct meter rather than one per record.
#[must_use]
pub(crate) fn fake_declaration_source_counting() -> Arc<CountingDeclarationSource> {
    Arc::new(CountingDeclarationSource {
        inner: FakeDeclarationSource {
            fold: "SUM".to_owned(),
            metadata_keys: Vec::new(),
            type_uuid: None,
        },
        calls: AtomicUsize::new(0),
        inputs: Mutex::new(Vec::new()),
        unresolvable: None,
    })
}

/// Registration suffix every `RecordingPlugin`-backed test builds its
/// [`Service`] under. Fixed rather than caller-supplied — each test wires
/// its own fresh [`ClientHub`] (see [`hub_with_plugin`]), so a shared
/// instance id across tests never collides.
pub(crate) const RECORDING_PLUGIN_SUFFIX: &str = "test.usage_collector.recording.plugin.v1";

/// The same, for a [`FoldingPlugin`]-backed [`Service`]. Distinct from
/// [`RECORDING_PLUGIN_SUFFIX`] only so a reader of a hub registration can
/// tell which double is behind it.
pub(crate) const FOLDING_PLUGIN_SUFFIX: &str = "test.usage_collector.folding.plugin.v1";

/// The PDP fake a read-path [`ServiceFixture`] must use instead of the
/// default [`CountingTenantPermitResolver`], whichever double backs it.
///
/// A read-path PDP request carries no per-instance resource properties (it
/// authorizes pre-row), so the enforcer needs [`CountingPermitResolver`]'s
/// fixed `OWNER_TENANT_ID` constraint. [`CountingTenantPermitResolver`] reads
/// the constraint back out of the request, finds no tenant key here, and
/// falls back to an allow-all permit the gate then denies.
#[must_use]
pub(crate) fn recording_plugin_resolver() -> Arc<dyn AuthZResolverApi> {
    CountingPermitResolver::new(
        pep_properties::OWNER_TENANT_ID,
        Uuid::from_u128(2).to_string(),
    )
}

// ── Doubles for the re-attachment guards ───────────────────────────────────
//
// Every fixture above answers under the meter it was dispatched with, which
// is what an ordinary test needs: the gear re-attaches and the identifier
// round-trips. The four below exist for the cases that must *not* round-trip
// — a plugin answering under a meter the gear did not ask about — plus the
// two the point read needs, which carries no meter at all.

/// A plugin whose every read answers one entry under a reference the caller
/// never named.
///
/// Programmed with the foreign reference rather than deriving one, so a
/// failing test shows the value the gear refused. The entry is well formed
/// otherwise: only the meter it claims is wrong.
pub(crate) struct ForeignMeterPlugin {
    foreign: Uuid,
}

impl ForeignMeterPlugin {
    #[must_use]
    pub(crate) fn new(foreign: Uuid) -> Arc<Self> {
        Arc::new(Self { foreign })
    }

    /// One entry under the foreign reference.
    fn foreign_entry(&self) -> StoredUsageRecord {
        guard_fixture_record().into_stored(self.foreign)
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for ForeignMeterPlugin {
    async fn create_usage_records(
        &self,
        records: Vec<(MeterRef, StoredUsageRecord)>,
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        Ok(records
            .into_iter()
            .map(|_| Ok(self.foreign_entry()))
            .collect())
    }

    async fn get_usage_record(
        &self,
        _id: Uuid,
        _scope: &ast::Expr,
        _converged_only: bool,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        Ok(self.foreign_entry())
    }

    async fn query_aggregated_usage_records(
        &self,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _fold: AggregationFold,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: ForeignMeterPlugin answers no fold",
        ))
    }

    async fn list_usage_records(
        &self,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _keyset: Option<&Keyset>,
    ) -> Result<RecordPage, UsageCollectorPluginError> {
        Ok(RecordPage {
            items: vec![self.foreign_entry()],
            next: None,
        })
    }

    async fn read_feed_page(
        &self,
        _subscription: &[MeterRef],
        _scope: &ast::Expr,
        _start: FeedStart<FeedPosition>,
        _until: Option<FeedPosition>,
        _limit: u64,
    ) -> Result<FeedPage<FeedPosition, StoredUsageRecord>, UsageCollectorPluginError> {
        Ok(FeedPage {
            entries: vec![self.foreign_entry()],
            next: None,
        })
    }

    async fn get_reconciliation_metadata(
        &self,
        _tenant_id: Uuid,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _fold: AggregationFold,
        _scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: ForeignMeterPlugin answers no reconciliation",
        ))
    }
}

/// A plugin whose every create refuses with an `IdempotencyConflict` whose
/// `existing` names a meter the caller never asked about.
///
/// The error arm reaches re-attachment by a different path than the `Ok`
/// arm, so a double that only mis-answered `Ok` would leave it unexercised.
pub(crate) struct ConflictingPlugin {
    existing_reference: Uuid,
}

impl ConflictingPlugin {
    #[must_use]
    pub(crate) fn with_existing_reference(existing_reference: Uuid) -> Arc<Self> {
        Arc::new(Self { existing_reference })
    }

    fn conflict(&self) -> UsageCollectorPluginError {
        UsageCollectorPluginError::idempotency_conflict(
            "idem-1",
            guard_fixture_record().into_stored(self.existing_reference),
        )
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for ConflictingPlugin {
    async fn create_usage_records(
        &self,
        records: Vec<(MeterRef, StoredUsageRecord)>,
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        Ok(records.into_iter().map(|_| Err(self.conflict())).collect())
    }

    async fn get_usage_record(
        &self,
        id: Uuid,
        _scope: &ast::Expr,
        _converged_only: bool,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::UsageRecordNotFound { id })
    }

    async fn query_aggregated_usage_records(
        &self,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _fold: AggregationFold,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: ConflictingPlugin answers no fold",
        ))
    }

    async fn list_usage_records(
        &self,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _keyset: Option<&Keyset>,
    ) -> Result<RecordPage, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: ConflictingPlugin answers no page",
        ))
    }

    async fn read_feed_page(
        &self,
        _subscription: &[MeterRef],
        _scope: &ast::Expr,
        _start: FeedStart<FeedPosition>,
        _until: Option<FeedPosition>,
        _limit: u64,
    ) -> Result<FeedPage<FeedPosition, StoredUsageRecord>, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: ConflictingPlugin answers no feed page",
        ))
    }

    async fn get_reconciliation_metadata(
        &self,
        _tenant_id: Uuid,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _fold: AggregationFold,
        _scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: ConflictingPlugin answers no reconciliation",
        ))
    }
}

/// A plugin holding exactly one entry, answered by `id` alone.
///
/// Built for the point read, the one SPI method carrying no meter: the gear
/// reverse-resolves whatever reference comes back, so the double must answer
/// *a* reference rather than the queried meter's.
pub(crate) struct SingleRecordPlugin {
    gts_type_uuid: Uuid,
}

impl SingleRecordPlugin {
    /// The `id` the one held entry carries, and the one a test asks for.
    pub(crate) const ID: Uuid = Uuid::from_u128(0x5_1_1_6);

    #[must_use]
    pub(crate) fn new(gts_type_uuid: Uuid) -> Arc<Self> {
        Arc::new(Self { gts_type_uuid })
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for SingleRecordPlugin {
    async fn get_usage_record(
        &self,
        id: Uuid,
        _scope: &ast::Expr,
        _converged_only: bool,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        if id != Self::ID {
            return Err(UsageCollectorPluginError::UsageRecordNotFound { id });
        }
        Ok(StoredUsageRecord {
            id: Self::ID,
            ..guard_fixture_record().into_stored(self.gts_type_uuid)
        })
    }

    async fn create_usage_records(
        &self,
        _records: Vec<(MeterRef, StoredUsageRecord)>,
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        Err(UsageCollectorPluginError::internal(
            "test_fake: SingleRecordPlugin admits no create",
        ))
    }

    async fn query_aggregated_usage_records(
        &self,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _fold: AggregationFold,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: SingleRecordPlugin answers no fold",
        ))
    }

    async fn list_usage_records(
        &self,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _keyset: Option<&Keyset>,
    ) -> Result<RecordPage, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: SingleRecordPlugin answers no page",
        ))
    }

    async fn read_feed_page(
        &self,
        _subscription: &[MeterRef],
        _scope: &ast::Expr,
        _start: FeedStart<FeedPosition>,
        _until: Option<FeedPosition>,
        _limit: u64,
    ) -> Result<FeedPage<FeedPosition, StoredUsageRecord>, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: SingleRecordPlugin answers no feed page",
        ))
    }

    async fn get_reconciliation_metadata(
        &self,
        _tenant_id: Uuid,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _fold: AggregationFold,
        _scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: SingleRecordPlugin answers no reconciliation",
        ))
    }
}

/// A conforming plugin that holds whatever a withdrawal's target lookup asks
/// for, under one reference, and echoes what it is told to persist.
///
/// The one double a withdrawal can be driven through end to end without a
/// programmed response per step, for the guard test whose subject is that the
/// ingestion path reaches no reverse resolver at all.
pub(crate) struct TargetHoldingPlugin {
    gts_type_uuid: Uuid,
}

impl TargetHoldingPlugin {
    #[must_use]
    pub(crate) fn new(gts_type_uuid: Uuid) -> Arc<Self> {
        Arc::new(Self { gts_type_uuid })
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for TargetHoldingPlugin {
    async fn get_usage_record(
        &self,
        id: Uuid,
        _scope: &ast::Expr,
        _converged_only: bool,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        // Answered under the identifier asked for: the gear refuses a row
        // whose `id` differs, so answering its own would fail for the wrong
        // reason.
        Ok(StoredUsageRecord {
            id,
            ..guard_fixture_record().into_stored(self.gts_type_uuid)
        })
    }

    async fn create_usage_records(
        &self,
        records: Vec<(MeterRef, StoredUsageRecord)>,
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        Ok(records
            .into_iter()
            .map(|(_meter, record)| Ok(record))
            .collect())
    }

    async fn query_aggregated_usage_records(
        &self,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _fold: AggregationFold,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: TargetHoldingPlugin answers no fold",
        ))
    }

    async fn list_usage_records(
        &self,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _keyset: Option<&Keyset>,
    ) -> Result<RecordPage, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: TargetHoldingPlugin answers no page",
        ))
    }

    async fn read_feed_page(
        &self,
        _subscription: &[MeterRef],
        _scope: &ast::Expr,
        _start: FeedStart<FeedPosition>,
        _until: Option<FeedPosition>,
        _limit: u64,
    ) -> Result<FeedPage<FeedPosition, StoredUsageRecord>, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: TargetHoldingPlugin answers no feed page",
        ))
    }

    async fn get_reconciliation_metadata(
        &self,
        _tenant_id: Uuid,
        _meter: &MeterRef,
        _time_range: TimeRange,
        _fold: AggregationFold,
        _scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: TargetHoldingPlugin answers no reconciliation",
        ))
    }
}

/// The entry the four doubles above answer with, before its reference is
/// attached.
///
/// A faithful copy of what `service_reference_tests`' `sample_create_record`
/// projects to: the guards are about the meter an entry claims, and a
/// withdrawal's faithful-copy check runs against this very row.
fn guard_fixture_record() -> UsageRecord {
    UsageRecord {
        id: Uuid::from_u128(0x1234),
        gts_type_id: MeterTypeId::new(toolkit_gts::gts_id!(
            "cf.core.uc.usage_record.v1~example.usage._.bytes_in.v1~"
        ))
        .expect("valid gts_type_id"),
        tenant_id: Uuid::from_u128(2),
        resource_ref: usage_collector_sdk::ResourceRef::new("rsc-1", "compute.vm")
            .expect("valid resource ref"),
        subject_ref: None,
        metadata: std::collections::BTreeMap::new(),
        quantity: qty("1"),
        idempotency_key: usage_collector_sdk::IdempotencyKey::new("idem-1")
            .expect("valid idempotency key"),
        accepted_at: recent_window_end(),
        origin: RecordOrigin::Live,
        invalidation: None,
        window_start: recent_window_start(),
        window_end: recent_window_end(),
    }
}

// ── `Service` builders for the point read and the ingestion-path guard ─────

/// A [`DeclarationSource`] answering the reverse direction from a table, and
/// refusing the forward one outright.
///
/// The forward half panics: every test built on this double drives the point
/// read, which resolves nothing forward.
struct ReverseOnlyDeclarationSource {
    by_reference: std::collections::HashMap<Uuid, MeterTypeId>,
}

#[async_trait]
impl DeclarationSource for ReverseOnlyDeclarationSource {
    async fn fetch(&self, id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError> {
        panic!("ReverseOnlyDeclarationSource::fetch({id}) must not be reached");
    }

    async fn fetch_by_uuid(&self, type_uuid: Uuid) -> Result<GtsTypeSchema, DomainError> {
        match self.by_reference.get(&type_uuid) {
            Some(id) => Ok(fake_meter_schema(id, "SUM", &[])),
            // A definite not-found: the reference names no declaration at
            // all, which is spec §3.4's first arm and must not be confused
            // with the registry being unreachable.
            None => Err(DomainError::DeclarationNotFound {
                gts_type_id: type_uuid.to_string(),
                reason: "is not declared under this registry reference".to_owned(),
            }),
        }
    }
}

/// A `Service` over `plugin` whose reverse resolver answers exactly the
/// references in `mapping`, and answers every other with a definite
/// not-found.
///
/// The point read's two outcomes both come from here: a reference the table
/// names resolves to its meter, and one it does not is the gear-side
/// corruption spec §3.4 answers `500` for.
#[must_use]
pub(crate) fn service_with_reverse_mapping(
    plugin: Arc<dyn UsageCollectorPluginV1>,
    mapping: &[(Uuid, &MeterTypeId)],
) -> Arc<Service> {
    let source: Arc<dyn DeclarationSource> = Arc::new(ReverseOnlyDeclarationSource {
        by_reference: mapping
            .iter()
            .map(|(type_uuid, id)| (*type_uuid, (*id).clone()))
            .collect(),
    });
    let metrics: Arc<dyn UsageCollectorMetrics> = Arc::new(NoopMetrics);
    ServiceFixture::default()
        .with_resolver(recording_plugin_resolver())
        .with_reverse_resolver(Arc::new(MeterReverseResolver::new(
            source,
            Arc::new(NoopDeclarationMirror),
            metrics,
        )))
        .build(plugin, RECORDING_PLUGIN_SUFFIX)
}

/// A `Service` over `plugin` whose reverse resolver cannot reach
/// `types-registry` at all.
///
/// Spec §3.4's second arm: a transport failure is transient and answers
/// `503`, where a definite not-found is the gear-side corruption above. This
/// is `Service::new`'s own inert default made explicit.
#[must_use]
pub(crate) fn service_with_unreachable_registry(
    plugin: Arc<dyn UsageCollectorPluginV1>,
) -> Arc<Service> {
    ServiceFixture::default()
        .with_resolver(recording_plugin_resolver())
        .with_reverse_resolver(inert_reverse_resolver(Arc::new(NoopMetrics)))
        .build(plugin, RECORDING_PLUGIN_SUFFIX)
}

/// A [`DeclarationMirror`] every method of which panics.
///
/// Half of a "no reverse resolution happened" assertion: a resolver over a
/// panicking source and a panicking mirror cannot answer at all, so a path
/// that reached it fails loudly.
struct PanickingDeclarationMirror;

#[async_trait]
impl DeclarationMirror for PanickingDeclarationMirror {
    async fn upsert(
        &self,
        _id: &MeterTypeId,
        _type_uuid: Uuid,
        _document: &serde_json::Value,
    ) -> Result<(), MirrorError> {
        panic!("the declaration mirror must not be reached")
    }

    async fn read(&self, _id: &MeterTypeId) -> Result<Option<serde_json::Value>, MirrorError> {
        panic!("the declaration mirror must not be reached")
    }

    async fn resolve_id(&self, _type_uuid: Uuid) -> Result<Option<MeterTypeId>, MirrorError> {
        panic!("the declaration mirror must not be reached")
    }
}

/// The other half: a [`DeclarationSource`] whose reverse direction panics.
///
/// The forward direction answers normally, because the paths these fixtures
/// drive resolve forward on their way to the plugin — what they must not do
/// is resolve *back*.
struct PanickingReverseDeclarationSource {
    type_uuid: Uuid,
}

#[async_trait]
impl DeclarationSource for PanickingReverseDeclarationSource {
    async fn fetch(&self, id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError> {
        Ok(GtsTypeSchema {
            type_uuid: self.type_uuid,
            ..fake_meter_schema(id, "SUM", &[])
        })
    }

    async fn fetch_by_uuid(&self, type_uuid: Uuid) -> Result<GtsTypeSchema, DomainError> {
        panic!("no reverse resolution may happen on this path; one was attempted for {type_uuid}")
    }
}

/// A reverse resolver that cannot answer without panicking.
///
/// Both tiers are poisoned, so "no reverse resolution happened" is asserted
/// by the absence of a panic rather than by a counter a regression could
/// leave at zero for a second reason.
fn panicking_reverse_resolver(source: Arc<dyn DeclarationSource>) -> Arc<MeterReverseResolver> {
    Arc::new(MeterReverseResolver::new(
        source,
        Arc::new(PanickingDeclarationMirror),
        Arc::new(NoopMetrics),
    ))
}

/// A `Service` whose PDP denies every request and whose reverse resolver
/// panics if it is reached.
///
/// The point read collapses a deny to not-found *before* it dispatches, so
/// neither the plugin nor the resolver may be touched: `plugin` is the
/// caller's panicking double and the resolver is this module's.
#[must_use]
pub(crate) fn service_denying_with_panicking_resolver(
    plugin: Arc<dyn UsageCollectorPluginV1>,
) -> Arc<Service> {
    ServiceFixture::default()
        .with_resolver(Arc::new(DenyAllResolver))
        .with_reverse_resolver(panicking_reverse_resolver(Arc::new(
            PanickingReverseDeclarationSource {
                type_uuid: Uuid::nil(),
            },
        )))
        .build(plugin, RECORDING_PLUGIN_SUFFIX)
}

/// A `Service` that resolves `meter` forward to `type_uuid` and panics on
/// any reverse resolution.
///
/// The ingestion path's own guard: a withdrawal resolves its target and
/// compares references, and must never reach the reverse resolver — that
/// would put a `types-registry` dependency on ingestion.
#[must_use]
pub(crate) fn service_with_panicking_reverse_resolver(
    plugin: Arc<dyn UsageCollectorPluginV1>,
    _meter: &MeterTypeId,
    type_uuid: Uuid,
) -> Arc<Service> {
    let source: Arc<dyn DeclarationSource> =
        Arc::new(PanickingReverseDeclarationSource { type_uuid });
    ServiceFixture::default()
        .with_source(Arc::clone(&source))
        .with_resolver(recording_plugin_resolver())
        .with_reverse_resolver(panicking_reverse_resolver(source))
        .build(plugin, RECORDING_PLUGIN_SUFFIX)
}
