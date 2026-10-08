//! The usage-collector batch ingestion pipeline.
//!
//! `Service::run_ingestion_pipeline` is the whole of it: one batch in, one
//! `Result` per input index out. `domain/service.rs` keeps the request-level
//! orchestration around it — the structural cap, the quota charge, the
//! plugin binding and the completion telemetry — and nothing of the
//! per-entry stages.
//!
//! **One value per input index.** The pipeline carries a single
//! `Vec<Slot>` whose position *is* the input index, and each stage replaces
//! the slots it owns with their next [`Slot`] state. "Which stage is this
//! entry at" therefore has exactly one representation, and no two passes can
//! disagree about whether an entry is already finished.
//!
//! # One module per stage
//!
//! This module holds only what every stage shares — the [`Slot`] state
//! machine and the shapes the three fan-outs cache — plus the pipeline that
//! sequences them. Each stage owns a module, and they run in the per-record
//! error-precedence order and never in another:
//!
//! 1. [`projection`] — covered-period projection and admission, per
//!    submission at its own input index.
//! 2. [`authorization`] — one PDP decision per distinct attribution tuple.
//! 3. [`declarations`] — one declaration per distinct `gts_type_id`.
//! 4. [`targets`] — one read per distinct invalidation reference, then the
//!    withdrawal semantics and the deferred metadata check.
//! 5. [`dispatch`] — one SPI call per dedup identity, projected onto every
//!    input index sharing it (DESIGN §3.1 "Collision resolution").
//!
//! Each stage takes the slots and returns them, so the pipeline below reads
//! as the five-line sequence it is, and a reader following one stage opens
//! one module rather than scrolling past the other four.
//!
//! `PDP_CONCURRENCY` / `TYPE_RESOLUTION_FANOUT_CONCURRENCY` /
//! `TARGET_LOOKUP_FANOUT_CONCURRENCY` bound the three request-local fan-outs
//! (PDP by attribution tuple, declaration by `gts_type_id`, invalidation
//! target by `(target, lookup_scope_id)`); the [`PdpGroupDecision`] /
//! [`DeclarationCache`] / [`InvalidationTargetCache`] aliases name the shapes
//! those fan-outs produce and cache.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use time::OffsetDateTime;
use toolkit_odata::ast;
use toolkit_security::{AccessScope, SecurityContext};
use usage_collector_sdk::{
    CreateUsageRecord, EntryType, Invalidation, MeterRef, MeterTypeId, RecordOrigin,
    StoredUsageRecord, UsageCollectorError, UsageCollectorPluginV1, UsageRecord,
};
use uuid::Uuid;

use crate::domain::error;
use crate::domain::error::DomainError;
use crate::domain::service::Service;
use crate::domain::type_resolver::ResolvedDeclaration;

mod authorization;
mod declarations;
mod dispatch;
mod projection;
mod targets;

pub use authorization::pdp_op_for;
pub use projection::acceptance_instant;
use projection::project_and_admit;

use crate::domain::validation::validate_submit_record_metadata;

/// Concurrency cap for the per-distinct-attribution-tuple PDP fan-out in
/// `create_usage_records`. Sized to match the platform's established
/// external-call posture (8) so a worst-case all-distinct cap-sized batch
/// takes `ceil(cap / 8) × PDP_RTT` wall-clock without overwhelming the PDP
/// transport pool. Bounds the `inst-auth-ingest-pdp` step of
/// `cpt-cf-usage-collector-flow-authorize-ingestion`.
pub const PDP_CONCURRENCY: usize = 8;

/// Concurrency cap for the per-distinct-`gts_type_id` Type Resolver fan-out in
/// `create_usage_records`. Bounds request-local pressure on
/// [`TypeResolver::resolve`] (itself single-flighted per key, and normally
/// served from cache — see [`crate::domain::type_resolver`]) for the
/// type-resolution pre-pass; sized identically to [`PDP_CONCURRENCY`] (the
/// three request-local fan-outs — this one, PDP above and the
/// invalidation-target one below — run sequentially, not concurrently, so
/// the effective in-flight ceiling stays at 8).
pub const TYPE_RESOLUTION_FANOUT_CONCURRENCY: usize = 8;

/// Concurrency cap for the per-distinct-target `get_usage_record` fan-out
/// that resolves a batch's invalidation references in
/// `create_usage_records`. Bounds plugin-side pressure for the target
/// pre-check, and is 8 for the platform's established external-call posture
/// — stated rather than delegated to the two caps above, so the value
/// survives either of them changing. Bounds the
/// `inst-algo-semantics-l1-bounded-fanout` step of
/// `cpt-cf-usage-collector-algo-target-resolution`.
///
/// **A deliberate default, not a pinned one**, as are the two caps above:
/// no test constrains the number, because pinning it would take a
/// timing-dependent test that observes concurrency.
pub const TARGET_LOOKUP_FANOUT_CONCURRENCY: usize = 8;

/// One PDP fan-out outcome: the input indices that share an attribution
/// tuple plus the `Result<AccessScope, DomainError>` returned for that
/// tuple's representative call. Decision projection (success / deny /
/// unavailable → per-index `results[index]` slot) reads this shape; a
/// permitted group's granted [`AccessScope`] is kept so the group's
/// invalidation entries (if any) can compile it into their target lookup
/// scope.
pub type PdpGroupDecision = (Vec<usize>, Result<AccessScope, DomainError>);

/// Cached resolution per distinct meter, lifted into [`DomainError`] so one
/// resolution outcome projects to every record sharing that type without
/// re-resolving it.
pub type DeclarationCache = HashMap<MeterTypeId, Result<Arc<ResolvedDeclaration>, DomainError>>;

/// Cached target lookup per distinct `(derived target, lookup scope id)`,
/// lifted into [`DomainError`] so the variant identity of
/// `UsageRecordNotFound { id }` survives the cache. The scope id keys the
/// read to the permit that authorized it, so two entries naming the same
/// target under different permits are not served from one another's read.
pub type InvalidationTargetCache = HashMap<(Uuid, usize), Result<StoredUsageRecord, DomainError>>;

/// A permitted PDP group's target-lookup scope: `Ok((scope id, compiled
/// filter))` once the group's `create` permit compiled into an `OData`
/// filter, `Err` when the compilation itself failed.
///
/// Compiled **once** per group and shared by every invalidation entry the
/// group covers — the entries already share one PDP decision, so they can
/// share one compiled scope too. A group that authorizes no invalidation
/// never compiles one: an ordinary measurement triggers no target lookup.
type LookupScope = Result<(usize, Arc<ast::Expr>), DomainError>;

/// What an entry declaring `entry_type: invalidation` carries beside its
/// projected record until the target stage consumes it.
///
/// Present on a slot exactly when the projected record carries an
/// [`Invalidation`] — both halves are read off that one fact in the
/// projection pass, so a slot can never hold a withdrawal's submission
/// without its reason, or the reverse.
struct Withdrawal {
    /// The submission as the caller sent it. The faithful-copy comparator
    /// runs against this rather than against the projected record: the
    /// submission has no identity yet, which is the honest reason `id` is
    /// not compared, and destructuring the *submission* shape is what makes
    /// a field added to [`CreateUsageRecord`] alone a compile error there.
    submission: CreateUsageRecord,
    /// The withdrawal the projected record carries: the reason the
    /// submission stated, paired with the target the gateway derived from
    /// the submission's own identity inputs. Unwrapped once here so the
    /// verification cannot be reached for an entry that has none. Its
    /// `target` is the fan-out key and the identifier the rejection echoes.
    invalidation: Invalidation,
}

/// One entry whose target check was deferred to the target stage. Entries
/// reach it only after passing PDP and the declaration-resolution stage, and
/// only when they declare `entry_type: invalidation`.
///
/// A named struct rather than a tuple, and a correctness choice: two of its
/// record-shaped members and the fan-out key are where a positional
/// alignment slip would hide, and a slip in either direction rejects the
/// wrong submission with someone else's identifier. The input index is **not**
/// among its fields — the entry rides in its own `Vec<Slot>` position, and
/// that position is the index.
struct PendingTarget {
    /// The submission and the reason code this entry withdraws under, moved
    /// in whole from the [`Slot::Authorized`] state.
    withdrawal: Withdrawal,
    /// The meter this entry resolved to, taken off the declaration the
    /// declaration stage already fetched. Carried here rather than looked up
    /// again so the reference the faithful-copy comparison reads and the
    /// reference the dispatch is keyed on are one value.
    meter: MeterRef,
    /// The projected entry, dispatched to the plugin once verified.
    record: UsageRecord,
    /// The compiled scope of the `create` permit that authorized this
    /// submission. The target is read under it.
    lookup_scope: Arc<ast::Expr>,
    /// Identifies [`Self::lookup_scope`] within the request: one id per PDP
    /// tuple group, so entries sharing a permit share a lookup.
    lookup_scope_id: usize,
}

/// How far one input index of a batch has got.
///
/// **One value per input index, and the index is the position.** A stage
/// replaces a slot with its next state rather than recording progress in a
/// second index-aligned vector, so an entry's stage has exactly one
/// representation and two passes cannot disagree about whether it is
/// finished. Each stage reads the variants it owns, writes each slot it
/// touches exactly once, and leaves every other slot untouched.
#[allow(
    clippy::large_enum_variant,
    reason = "the variants carry the entry itself rather than an allocation-hiding indirection; one Vec<Slot> lives for the span of one batch and is never stored or cloned in bulk"
)]
enum Slot {
    /// Projected and admitted by [`project_and_admit`], awaiting the PDP
    /// decision for its attribution tuple.
    Admitted {
        record: UsageRecord,
        withdrawal: Option<Withdrawal>,
    },
    /// Permitted by the PDP, awaiting its declaration. A withdrawal carries
    /// the compiled scope of the permit that authorized it in the same
    /// payload: a permitted group compiles a scope exactly when it covers a
    /// withdrawal, so neither half can go missing without the other.
    Authorized {
        record: UsageRecord,
        withdrawal: Option<(Withdrawal, LookupScope)>,
    },
    /// A withdrawal whose declaration resolved, awaiting the read of the
    /// target its `invalidates` names.
    AwaitingTarget(PendingTarget),
    /// Validated whole; dispatched to the storage plugin, one call per dedup
    /// identity.
    Eligible {
        meter: MeterRef,
        record: UsageRecord,
    },
    /// Finished: the outcome this input index returns.
    Resolved(Result<UsageRecord, UsageCollectorError>),
}

impl Slot {
    /// Whether this slot still awaits a PDP decision **and** carries a
    /// withdrawal — the one condition that makes a permitted tuple group
    /// compile its lookup scope.
    const fn awaits_target_lookup(&self) -> bool {
        matches!(
            self,
            Self::Admitted {
                withdrawal: Some(_),
                ..
            }
        )
    }
}

impl Service {
    /// Create a batch of `UsageRecord`s through the ingestion path per
    /// `cpt-cf-usage-collector-flow-emit-usage-record`.
    ///
    /// Per-record stages — identity derivation, PDP authorization, and SPI
    /// dispatch — run independently for each input; eligible records carry
    /// their caller-supplied covered period through to persistence
    /// UTC-normalized but otherwise unchanged, nothing quantized or truncated,
    /// and are dispatched together. Per-record validation / SPI failures
    /// surface in the result vector at their input index — including a
    /// rejected covered period, which
    /// `cpt-cf-usage-collector-adr-record-identity-derivation` makes a
    /// per-submission precondition of the identity derivation. The outer `Err`
    /// is reserved for batch-level failures (here, the outer SPI dispatch; the
    /// caller adds plugin-handle resolution and the structural batch-size cap
    /// ahead of this call).
    ///
    /// **One `Vec<Slot>`, one stage per pass.** Each stage advances the slots it
    /// owns and leaves the rest; an entry's progress is its slot and nothing
    /// else, so no two passes can disagree about whether it is finished. The
    /// passes run in the per-record error-precedence order — covered-period
    /// projection, PDP, declaration, invalidation-target semantics / metadata,
    /// dispatch — and a slot resolved by an earlier one is never revisited.
    ///
    /// `plugin` is the caller's resolved storage-plugin handle: resolution is
    /// request-level work and happens once, before this is called.
    ///
    /// # Errors
    ///
    /// * [`UsageCollectorError::ServiceUnavailable`] when the outer SPI dispatch
    ///   fails.
    /// * Any other [`UsageCollectorError`] variant lifted from a batch-level
    ///   plugin transport / persistence failure.
    ///
    /// Per-record failures (a rejected covered period, authorization denial,
    /// an unresolvable declaration, malformed metadata, SPI errors against
    /// individual records) surface in the per-index `Result` entries of the
    /// returned vector rather than the outer `Err`. "A rejected covered
    /// period" covers both of [`project_and_admit`]'s refusals, and
    /// every entry of one batch is judged against a single `now`, so two
    /// entries carrying the same period cannot be decided differently.
    ///
    /// `origin` is the caller's route, not a caller's value: the wrapper that
    /// *is* a route passes its own. One value for the whole batch — a batch is
    /// admitted by one route.
    ///
    /// # Post-condition
    ///
    /// On `Ok`, the returned vector has length equal to `records.len()` and
    /// preserves input order: index `i` of the output corresponds to index
    /// `i` of the input batch.
    //
    // Realizes the batch flow `cpt-cf-usage-collector-flow-emit-usage-record`,
    // the only ingestion pipeline in the gear.
    //
    // The workload-isolation DoD below is the write-vs-read isolation
    // obligation, which this body satisfies: the gateway is the sole write
    // entry point and shares no state with the query gateway. The
    // backfill-vs-live gap is a DIFFERENT obligation, owned by no feature file
    // and tracked at `Service::backfill_usage_records`; this marker does not
    // cover it.
    // @cpt-dod:cpt-cf-usage-collector-nfr-workload-isolation:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-ingestion:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-record-metadata:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-resource-attribution:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-subject-attribution:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-ingestion-authorization:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-usage-type-resolution:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-tenant-attribution:p1
    // @cpt-dod:cpt-cf-usage-collector-principle-fail-closed:p1
    // @cpt-dod:cpt-cf-usage-collector-principle-pluggable-storage:p1
    // @cpt-dod:cpt-cf-usage-collector-constraint-no-business-logic:p1
    // @cpt-dod:cpt-cf-usage-collector-component-ingestion-gateway:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-idempotency:p1
    // @cpt-dod:cpt-cf-usage-collector-principle-idempotency-by-key:p1
    // @cpt-dod:cpt-cf-usage-collector-adr-mandatory-idempotency:p1
    // @cpt-dod:cpt-cf-usage-collector-adr-caller-supplied-attribution:p1
    // @cpt-dod:cpt-cf-usage-collector-seq-emit-usage:p1
    // @cpt-dod:cpt-cf-usage-collector-entity-model:p1
    // @cpt-begin:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-algo-attrib-receive-ctx
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    pub(crate) async fn run_ingestion_pipeline(
        &self,
        ctx: &SecurityContext,
        records: Vec<CreateUsageRecord>,
        origin: RecordOrigin,
        now: OffsetDateTime,
        plugin: &dyn UsageCollectorPluginV1,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorError>>, UsageCollectorError> {
        let covered_period_bounds = &self.covered_period_bounds;
        // @cpt-end:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-algo-attrib-receive-ctx
        // The `1..=max_batch_records` structural cap (the configured cap) is
        // enforced by the caller (before the batch-size observation), so a
        // batch that reaches this pipeline is already in range.

        // The service is the guaranteed choke point for every caller (REST +
        // in-process). The create surface is identity-free
        // (`CreateUsageRecord`); each entry acquires its deterministic
        // dedup-identity-derived `id` HERE, before authorization or
        // dispatch — the single point of derivation.
        //
        // The derivation is per-submission and fallible (the covered-period
        // preconditions of
        // `cpt-cf-usage-collector-adr-record-identity-derivation`), so a bad
        // period surfaces at its own input index instead of failing the
        // batch.
        //
        // `now` arrives as a parameter, captured once for the whole batch by
        // the caller: two entries carrying the same covered period must not be
        // judged differently because the clock crossed the bound between them,
        // and the quota charge and the admission must read one instant.
        let mut slots: Vec<Slot> = Vec::with_capacity(records.len());
        // Every derived id in this request, so a withdrawal naming another entry
        // of the same batch is told its target has not converged rather than
        // that it does not exist.
        //
        // Built in this pass, from every entry that survived *projection* and
        // before any slot moves past `Admitted`: a withdrawal naming a sibling
        // the PDP later denies still names this batch, and must be answered as
        // such.
        let mut batch_ids: HashSet<Uuid> = HashSet::new();
        for submission in records {
            // The submission each withdrawal was built from, kept because the
            // projection consumes the submission the faithful-copy comparator
            // runs against. The withdrawal's other half — the reason paired with
            // the derived target — is read back off the projected entry instead,
            // so there is no second copy to build inconsistently.
            let kept = matches!(submission.entry_type(), EntryType::Invalidation)
                .then(|| submission.clone());
            // Per-submission, at its own input index — never a batch-level
            // failure, or one out-of-bounds entry would discard a whole
            // import.
            slots.push(
            match project_and_admit(submission, origin, now, covered_period_bounds) {
                Ok(record) => {
                    batch_ids.insert(record.id);
                    // Both halves answer to one fact — the declared
                    // `entry_type`, which is also what decides whether the
                    // projection stamps an `Invalidation` — so the pair is
                    // present or absent together. Checkable rather than only
                    // stated, as the SDK's own `project()` states the same
                    // pairing from the other side. Unreachable today: the
                    // SDK's `try_into_invalidation_record` always stamps
                    // `Some(Invalidation)` on `Ok`, and
                    // `try_into_usage_record` always passes `None`. Kept as a
                    // canary against that guarantee changing underneath this
                    // pipeline — the match below is what actually keeps the
                    // pairing total in release builds, where this compiles
                    // out.
                    debug_assert_eq!(
                        kept.is_some(),
                        record.invalidation.is_some(),
                        "the declared entry_type and the projected withdrawal must agree",
                    );
                    // Keyed on the projected entry, not on the kept
                    // submission: the entry that gets dispatched is this
                    // one, so an absent submission is a breach refused here,
                    // for this slot alone, rather than a fall-through to
                    // `Slot::Eligible` with nothing having checked the
                    // target.
                    match (kept, record.invalidation.as_ref()) {
                        (Some(submission), Some(invalidation)) => Slot::Admitted {
                            withdrawal: Some(Withdrawal {
                                submission,
                                invalidation: invalidation.clone(),
                            }),
                            record,
                        },
                        // A record with no invalidation is an ordinary
                        // measurement; an unused kept submission here is not
                        // a breach (see the canary above for the direction
                        // that matters).
                        (_, None) => Slot::Admitted {
                            withdrawal: None,
                            record,
                        },
                        (None, Some(_)) => Slot::Resolved(Err(error::invariant_breach(format!(
                            "the projected invalidation at input {} has no submission kept for it",
                            slots.len(),
                        )))),
                    }
                }
                Err(e) => Slot::Resolved(Err(e)),
            },
        );
        }

        let slots = self.authorize_slots(slots, ctx, origin, now).await;

        let (slots, declaration_cache) = self.resolve_declarations(slots).await;

        let slots = self
            .resolve_invalidation_targets(slots, plugin, &declaration_cache, &batch_ids)
            .await;

        // One scan by position collects the dispatch set. `Vec<Slot>` is in
        // input order by construction — the target stage settles a withdrawal in
        // the slot it already occupied rather than appending it — so the
        // input-order guarantee `dispatch_eligible_entries` relies on holds
        // without a sort between the stages and the dispatch.
        //
        // `results` is the shape that function settles into; building it here
        // rather than round-tripping through the slots keeps one representation
        // of each index's outcome at every point.
        let mut eligible: Vec<(usize, MeterRef, UsageRecord)> = Vec::new();
        let mut results: Vec<Option<Result<UsageRecord, UsageCollectorError>>> = slots
            .into_iter()
            .enumerate()
            .map(|(index, slot)| match slot {
                Slot::Resolved(outcome) => Some(outcome),
                Slot::Eligible { meter, record } => {
                    eligible.push((index, meter, record));
                    None
                }
                // A stage left this slot un-advanced; the fallback below answers
                // for it.
                Slot::Admitted { .. } | Slot::Authorized { .. } | Slot::AwaitingTarget(_) => None,
            })
            .collect();

        // One dispatch per dedup identity (DESIGN §3.1 "Collision
        // resolution"); see `dispatch_eligible_entries`.
        self.dispatch_eligible_entries(plugin, eligible, &mut results)
            .await?;

        // @cpt-begin:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-return
        // Every slot is advanced to `Resolved` by the period / PDP / catalog /
        // semantics / metadata passes above, or dispatched by the SPI fan-out;
        // an unanswered index here means a stage forgot to advance its slot.
        // Yield a typed `Internal` for that slot so the request thread cannot
        // panic.
        Ok(results
            .into_iter()
            .enumerate()
            .map(|(index, outcome)| {
                outcome.unwrap_or_else(|| {
                    Err(error::invariant_breach(format!(
                        "slot {index} was not resolved before return"
                    )))
                })
            })
            .collect())
        // @cpt-end:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-return
    }
}

#[cfg(test)]
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "../ingestion_tests.rs"]
mod ingestion_tests;
