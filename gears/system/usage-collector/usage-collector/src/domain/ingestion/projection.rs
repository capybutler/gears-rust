//! The pipeline's first stage: project each submission into a stamped
//! [`UsageRecord`] and admit it, or settle its slot with the refusal.
//!
//! Per-submission and fallible at its own input index — a bad covered
//! period refuses one entry rather than the batch. [`acceptance_instant`] is
//! the batch-wide clock read the caller captures once and hands in.

use time::OffsetDateTime;
use usage_collector_sdk::{
    CreateUsageRecord, EntryType, RecordOrigin, UsageCollectorError, UsageRecord,
};

use crate::domain::covered_period::{CoveredPeriodBounds, enforce_covered_period_bounds};
use crate::domain::invalidation::derive_invalidation_target;
use crate::domain::ports::metrics::UsageCollectorMetrics;

/// The acceptance instant of one request: now, truncated to the microsecond.
///
/// Truncated because every conforming store keeps microseconds (Postgres
/// `timestamptz` does), so a finer stamp would read back different from what
/// was acknowledged. The same instant judges the covered-period bounds, so
/// the stamp and the late-arrival decision cannot disagree.
pub fn acceptance_instant() -> OffsetDateTime {
    let now = OffsetDateTime::now_utc();
    now.replace_microsecond(now.microsecond()).unwrap_or(now)
}

/// Observe `uc_record_metadata_bytes` for a record that carries metadata,
/// measured as the serialized JSON size (the canonical on-the-wire
/// representation the Plugin SPI persists). Records with empty metadata record
/// nothing, matching `inst-algo-metadata-observe-bytes`.
pub(super) fn observe_metadata_bytes(
    metrics: &dyn UsageCollectorMetrics,
    metadata: &std::collections::BTreeMap<usage_collector_sdk::MetadataKey, String>,
) {
    if metadata.is_empty() {
        return;
    }
    if let Ok(bytes) = serde_json::to_vec(metadata) {
        metrics.observe_record_metadata_bytes(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    }
}

/// The first two steps of both ingestion paths: project the submission
/// into its persisted shape, then admit or refuse its covered period.
///
/// One function rather than a copy per path, because the two are obliged to
/// agree. The batch path judges every entry of one submission against a
/// single `now`, and taking that instant as a parameter is what makes that
/// structural rather than conventional. Both failures are per-submission:
/// the projection's own period preconditions
/// (`cpt-cf-usage-collector-adr-record-identity-derivation` — a bound finer
/// than the microsecond, or an inverted period) and the path's tolerances
/// alike surface at one entry, never at the batch.
///
/// **Choosing the projection is this gateway's job, on the submission's
/// declared `entry_type`.** The two projections differ in exactly what only
/// a gateway can supply: a withdrawal's `invalidates`, derived here from
/// the submission's own identity inputs ([`derive_invalidation_target`])
/// rather than taken from the caller. Reading the declared kind is also
/// what keeps the SDK's `Internal` guards unreachable in correct operation.
///
/// The derivation runs before the projection, so a withdrawal's target
/// identifier is computed before the covered period is validated. Nothing
/// escapes: the value is an argument to the projection that rejects a bad
/// period, so a refused submission discards it unread — see
/// [`derive_invalidation_target`].
///
/// # Errors
///
/// * [`UsageCollectorError::InvalidArgument`] from the projection, on a
///   sub-microsecond or inverted covered period, on a missing
///   idempotency key, or when the submission's `entry_type` and
///   `reason_code` disagree.
/// * [`UsageCollectorError::InvalidArgument`] with reason
///   `FUTURE_WINDOW` / `PAST_WINDOW` when the period ends outside this
///   path's tolerances — see [`enforce_covered_period_bounds`].
/// * [`UsageCollectorError::Internal`] when a projection refuses a
///   self-consistent submission as not its own. That is a host-contract
///   breach rather than an emitter fault, and the match below is what
///   makes it unreachable; it is listed because the type admits it, not
///   because a caller can provoke it.
///
/// `now` is also the entry's `accepted_at`.
///
/// **This is the one identity-derivation choke point
/// (`cpt-cf-usage-collector-algo-entry-identity-derivation`,
/// `inst-identity-choke-point`):** every entry of every route passes
/// through here, and this function supplies the two gear-controlled inputs
/// — `origin` (hardcoded per route, never derived from the entry) and `now`
/// (seeded once per request). The identity itself is derived in the SDK —
/// see [`usage_collector_sdk::derive_usage_record_id`].
///
/// The same `project()` call enforces the key/entry-type/reason-code
/// structural rules
/// (`cpt-cf-usage-collector-algo-entry-structural-validation`,
/// `cpt-cf-usage-collector-dod-mandatory-idempotency-key`) and stamps the
/// server-assigned `id`, `accepted_at` and `origin`
/// (`cpt-cf-usage-collector-algo-server-field-stamping`,
/// `cpt-cf-usage-collector-dod-server-assigned-fields`). Neither this
/// function nor `project()` branches on the sign of `quantity` or on the
/// declared fold, which is what
/// `cpt-cf-usage-collector-flow-emit-negative-measurement` requires. Since
/// the SDK exports the derivation publicly, an external caller reproducing
/// it offline against the same six inputs obtains the same identifier by
/// construction (`cpt-cf-usage-collector-flow-reproduce-identity-offline`).
///
/// **This is also the withdrawal-linkage stamping site**
/// (`cpt-cf-usage-collector-algo-withdrawal-linkage-stamping`,
/// `cpt-cf-usage-collector-dod-withdrawal-linkage`). The `Invalidation` arm
/// is the only place in the gear supplying a `target`, and the wire can
/// supply none: both ingestion shapes carry `deny_unknown_fields` and
/// publish no `invalidates` property. The stamped value is not an identity
/// input, and the plugin persists what it is handed rather than deriving
/// one.
// @cpt-algo:cpt-cf-usage-collector-algo-entry-structural-validation:p1
// @cpt-algo:cpt-cf-usage-collector-algo-withdrawal-linkage-stamping:p1
// @cpt-dod:cpt-cf-usage-collector-dod-withdrawal-linkage:p1
// @cpt-algo:cpt-cf-usage-collector-algo-entry-identity-derivation:p1
// @cpt-algo:cpt-cf-usage-collector-algo-server-field-stamping:p1
// @cpt-dod:cpt-cf-usage-collector-dod-mandatory-idempotency-key:p1
// @cpt-dod:cpt-cf-usage-collector-dod-dedup-identity-derivation:p1
// @cpt-dod:cpt-cf-usage-collector-dod-server-assigned-fields:p1
// @cpt-flow:cpt-cf-usage-collector-flow-emit-negative-measurement:p1
// @cpt-flow:cpt-cf-usage-collector-flow-reproduce-identity-offline:p1
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub fn project_and_admit(
    submission: CreateUsageRecord,
    origin: RecordOrigin,
    now: OffsetDateTime,
    covered_period_bounds: &CoveredPeriodBounds,
) -> Result<UsageRecord, UsageCollectorError> {
    let record = match submission.entry_type() {
        EntryType::Record => submission.try_into_usage_record(origin, now)?,
        EntryType::Invalidation => {
            let target = derive_invalidation_target(&submission)?;
            submission.try_into_invalidation_record(origin, now, target)?
        }
    };
    enforce_covered_period_bounds(covered_period_bounds, origin, now, record.window_end)?;
    Ok(record)
}
