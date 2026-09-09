//! The in-memory conforming backend the contract suite is validated
//! against.
//!
//! [`InMemoryReferencePlugin`] implements every method of
//! [`UsageCollectorPluginV1`] against a `Vec<UsageRecord>` behind a mutex,
//! honouring the DESIGN §3.1 invariants the SPI puts on the plugin:
//! idempotent re-admission, at-most-one invalidation checked atomically,
//! scope-gated point lookup, period-end selection, and the withdrawal
//! exclusion inside a fold.
//!
//! **It is not a production backend, and it is not an example of how to
//! write one.** It holds everything in process memory, it scans linearly,
//! it mints no pagination cursor, and it loses every entry when the process
//! exits. What it is for is the suite's own subject: a backend known to
//! conform, so that a check reporting a violation against it is a bug in
//! the check rather than an open question about the backend. A real plugin
//! projects the same obligations into its storage engine — a `WHERE` clause,
//! a unique index, a transaction — and this module deliberately does not
//! model any of that.
//!
//! # Why not the noop plugin
//!
//! `noop-usage-collector-plugin` already implements the SPI, and it cannot
//! serve here: it persists nothing. `create_usage_record` echoes its input,
//! `get_usage_record` always answers `UsageRecordNotFound`,
//! `list_usage_records` always answers an empty page, and every fold
//! returns no buckets. It therefore fails all five behavioural checks by
//! construction — not by defect. A null backend exists so the plugin-host
//! binding resolves end-to-end in development, and answering a well-formed
//! default to every call is exactly the right behaviour for that job.
//! Nothing about the contract suite is a reason to change it; a suite whose
//! subject persists nothing has nothing to assert against, which is why
//! this second backend exists.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use bigdecimal::BigDecimal;
use rust_decimal::Decimal;
use toolkit_odata::{ODataQuery, Page as ODataPage, PageInfo, ast};
use uuid::Uuid;

use crate::error::UsageCollectorPluginError;
use crate::models::{
    AggregationBucket, AggregationDimension, AggregationFold, AggregationResult,
    MAX_AGGREGATION_BUCKETS, MetadataFilter, MeterTypeId, UsageRecord,
};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::time_range::TimeRange;

/// An in-memory storage backend that satisfies the Plugin SPI's stated
/// obligations. See the module docs for what it is and is not for.
#[derive(Debug, Default)]
pub struct InMemoryReferencePlugin {
    entries: Mutex<Vec<UsageRecord>>,
}

impl InMemoryReferencePlugin {
    /// Creates an empty backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrows the ledger, lifting a poisoned lock to a plugin error.
    ///
    /// A poisoned mutex means an earlier call panicked while holding the
    /// ledger, so its contents may be half-written. That is
    /// [`UsageCollectorPluginError::Internal`] rather than
    /// [`UsageCollectorPluginError::Transient`]: retrying cannot unpoison
    /// it.
    fn ledger(&self) -> Result<MutexGuard<'_, Vec<UsageRecord>>, UsageCollectorPluginError> {
        self.entries.lock().map_err(|_| {
            UsageCollectorPluginError::internal(
                "the reference backend's ledger lock is poisoned: an earlier call panicked while \
                 holding it",
            )
        })
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for InMemoryReferencePlugin {
    /// Admits one entry, or reports why it was refused.
    ///
    /// Both refusals are decided under one lock acquisition, which is the
    /// point of the at-most-one-invalidation rule rather than an
    /// optimisation: the SPI requires the check to be atomic with the entry
    /// it admits, because a read followed by a write lets two concurrent
    /// withdrawals of one record both observe an un-invalidated target.
    async fn create_usage_record(
        &self,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        let mut ledger = self.ledger()?;
        admit(&mut ledger, record)
    }

    /// Admits a batch, per-entry outcomes aligned to input order.
    ///
    /// The whole batch is decided under one lock acquisition, so two
    /// withdrawals of one record arriving in the same call are ordered
    /// against each other: the first is admitted and the second sees it.
    /// Admitting entries one at a time against the state each of them read
    /// would let both through.
    ///
    /// An empty batch is [`UsageCollectorPluginError::Internal`] — a
    /// host-contract breach rather than an empty result — matching the noop
    /// plugin, so a gear-side bug that dispatches nothing surfaces at the
    /// same place whichever backend is bound.
    async fn create_usage_records(
        &self,
        records: Vec<UsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        if records.is_empty() {
            return Err(UsageCollectorPluginError::internal(
                "create_usage_records called with an empty batch (host-contract breach)",
            ));
        }
        let mut ledger = self.ledger()?;
        Ok(records
            .into_iter()
            .map(|record| admit(&mut ledger, record))
            .collect())
    }

    /// Point lookup under the compiled scope.
    ///
    /// A row that exists but does not satisfy `scope` is
    /// [`UsageCollectorPluginError::UsageRecordNotFound`], the same answer
    /// an absent `id` gets. There is deliberately no distinguishable
    /// "denied": telling the two apart would make this surface an existence
    /// oracle for other tenants' records.
    ///
    /// A withdrawn pair is returned as persisted. This is a ledger path, and
    /// the exclusion belongs to the fold.
    async fn get_usage_record(
        &self,
        id: Uuid,
        scope: &ast::Expr,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        let ledger = self.ledger()?;
        ledger
            .iter()
            .find(|entry| entry.id == id && expr_admits(entry, scope))
            .cloned()
            .ok_or(UsageCollectorPluginError::UsageRecordNotFound { id })
    }

    /// Folds over the selected set, with both halves of every withdrawn
    /// pair left out.
    ///
    /// Selection is the read paths' shared one: the meter, the period end,
    /// `query.filter` and `metadata_filter`. Exclusion then removes every
    /// invalidation entry and every entry an accepted invalidation names.
    /// The second set is collected from the whole ledger rather than from
    /// the selection, because an invalidation the selection or the scope
    /// leaves out has still been accepted and still withdraws its target.
    async fn query_aggregated_usage_records(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        let ledger = self.ledger()?;
        let withdrawn = withdrawn_targets(&ledger);
        let rows: Vec<&UsageRecord> = ledger
            .iter()
            .filter(|entry| select(entry, &gts_type_id, time_range, query, metadata_filter))
            .filter(|entry| entry.invalidation.is_none() && !withdrawn.contains(&entry.id))
            .collect();
        fold_rows(fold, &rows, group_by)
    }

    /// Keyset-ordered ledger read over the selected set.
    ///
    /// A withdrawn pair is returned as persisted, both halves, under the
    /// same ledger obligation as the point lookup.
    ///
    /// The page is served in the canonical `(window_end, id)` ascending
    /// order and mints no `next_cursor`; `query.order` and continuation are
    /// not honoured. That is a stated limit of this backend rather than an
    /// oversight — no implemented check reads a second page or a
    /// caller-chosen order, and a cursor minted here would have to carry a
    /// fingerprint and a signed token set no check verifies. A real plugin
    /// owes both, and the SPI's `list_usage_records` doc is normative for
    /// it.
    async fn list_usage_records(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
        let ledger = self.ledger()?;
        let mut items: Vec<UsageRecord> = ledger
            .iter()
            .filter(|entry| select(entry, &gts_type_id, time_range, query, metadata_filter))
            .cloned()
            .collect();
        items.sort_by(|left, right| {
            left.window_end
                .cmp(&right.window_end)
                .then_with(|| left.id.cmp(&right.id))
        });
        let limit = query
            .limit
            .unwrap_or_else(|| u64::try_from(items.len()).unwrap_or(u64::MAX));
        items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(ODataPage::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit,
            },
        ))
    }
}

// ---------------------------------------------------------------------------
// Admission
// ---------------------------------------------------------------------------

/// Decides one entry against the ledger it is being admitted to.
///
/// The duplicate-`id` branch runs first, and the order matters: an
/// invalidation resubmitted verbatim collides on its own `id`, and that is
/// an idempotent replay of an accepted entry — not a second withdrawal of
/// its target. Checking at-most-one first would refuse an emitter's retry
/// with [`UsageCollectorPluginError::AlreadyInvalidated`], naming the
/// entry's own id as the offender.
fn admit(
    ledger: &mut Vec<UsageRecord>,
    record: UsageRecord,
) -> Result<UsageRecord, UsageCollectorPluginError> {
    if let Some(stored) = ledger.iter().find(|entry| entry.id == record.id) {
        // The `id` is the `UUIDv5` of the five dedup-identity attributes, so
        // a collision already means those five agree; what is compared here
        // is everything else the entry carries. `UsageRecord`'s own
        // `PartialEq` is the comparison rather than a hand-listed field set,
        // so a field added to the model is covered without an edit here.
        if *stored == record {
            return Ok(stored.clone());
        }
        return Err(UsageCollectorPluginError::IdempotencyConflict {
            idempotency_key: record.idempotency_key.as_str().to_owned(),
            existing_id: stored.id,
        });
    }

    if let Some(invalidation) = record.invalidation.as_ref()
        && let Some(existing) = ledger.iter().find(|entry| {
            entry
                .invalidation
                .as_ref()
                .is_some_and(|other| other.target == invalidation.target)
        })
    {
        return Err(UsageCollectorPluginError::AlreadyInvalidated {
            id: invalidation.target,
            invalidated_by: existing.id,
        });
    }

    ledger.push(record.clone());
    Ok(record)
}

/// Every `UsageRecord.id` an accepted invalidation names.
fn withdrawn_targets(ledger: &[UsageRecord]) -> std::collections::BTreeSet<Uuid> {
    ledger
        .iter()
        .filter_map(|entry| entry.invalidation.as_ref().map(|inv| inv.target))
        .collect()
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

/// Whether one entry is inside a read path's selection.
///
/// Period selection is [`TimeRange::contains_window_end`] — the reference
/// spelling of `from <= window_end < to`. Nothing here reads
/// `window_start`, and nothing matches by overlap or containment.
fn select(
    entry: &UsageRecord,
    gts_type_id: &MeterTypeId,
    time_range: TimeRange,
    query: &ODataQuery,
    metadata_filter: &[MetadataFilter],
) -> bool {
    entry.gts_type_id == *gts_type_id
        && time_range.contains_window_end(entry.window_end)
        && query
            .filter()
            .is_none_or(|filter| expr_admits(entry, filter))
        && metadata_admits(entry, metadata_filter)
}

/// Whether an entry satisfies every metadata filter in the slice.
///
/// AND across the slice, including two entries naming the same key; OR
/// within one entry's values; an empty slice imposes nothing. Merging
/// same-key entries would widen the result set, which is why this iterates
/// them rather than grouping them.
fn metadata_admits(entry: &UsageRecord, filters: &[MetadataFilter]) -> bool {
    filters.iter().all(|filter| {
        entry
            .metadata
            .get(filter.key())
            .is_some_and(|value| filter.values().iter().any(|candidate| candidate == value))
    })
}

/// One record attribute, in a form comparable against an [`ast::Value`].
#[derive(Debug, Clone, Copy)]
enum FieldValue<'a> {
    Uuid(Uuid),
    Str(&'a str),
}

/// Whether an entry satisfies a filter expression.
///
/// The expression is the gear's compiled PDP scope — a disjunction of
/// conjunctions over the five attribution identifiers — AND-ed with
/// whatever the caller's `$filter` contributed, so this evaluator serves
/// both.
///
/// **An unrecognised node evaluates to `false`, never `true`.** A scope
/// this backend cannot interpret must exclude the row: the alternative is a
/// filter that silently widens, and a widened scope filter is a cross-tenant
/// read. A plugin projecting to SQL owes the same posture — an
/// untranslatable predicate is a refused query, never a dropped conjunct.
///
/// [`ast::Expr::Not`] is deliberately among the unrecognised nodes. It is
/// not hard to evaluate; it is unsafe to, because negating a subtree that
/// returned `false` only because it was unrecognised turns a fail-closed
/// answer into `true` — which is the widening this rule exists to prevent.
/// Nothing in the gear emits one: `scope_to_odata_filter` builds `And`,
/// `Or`, `Compare(_, Eq, _)` and `In` and nothing else.
fn expr_admits(entry: &UsageRecord, expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::And(left, right) => expr_admits(entry, left) && expr_admits(entry, right),
        ast::Expr::Or(left, right) => expr_admits(entry, left) || expr_admits(entry, right),
        ast::Expr::Compare(left, op, right) => compare_admits(entry, left, *op, right),
        ast::Expr::In(left, candidates) => in_admits(entry, left, candidates),
        _ => false,
    }
}

/// Evaluates `<identifier> eq|ne <value>`.
///
/// An attribute the entry does not carry — no `subject_ref`, no
/// `invalidates` on an ordinary measurement — fails both operators, matching
/// SQL three-valued logic, where a comparison against NULL is NULL and the
/// row is not selected. `ne` against an absent attribute is therefore
/// `false` rather than `true`, which is again the fail-closed reading.
///
/// Every other operator (`gt`, `ge`, `lt`, `le`) and every other operand
/// shape is unrecognised, so it excludes the row.
fn compare_admits(
    entry: &UsageRecord,
    left: &ast::Expr,
    op: ast::CompareOperator,
    right: &ast::Expr,
) -> bool {
    let (ast::Expr::Identifier(name), ast::Expr::Value(value)) = (left, right) else {
        return false;
    };
    let Some(field) = record_field(entry, name) else {
        return false;
    };
    match op {
        ast::CompareOperator::Eq => value_matches(field, value),
        ast::CompareOperator::Ne => !value_matches(field, value),
        ast::CompareOperator::Gt
        | ast::CompareOperator::Ge
        | ast::CompareOperator::Lt
        | ast::CompareOperator::Le => false,
    }
}

/// Evaluates `<identifier> in (<value>, …)`.
///
/// A candidate list carrying anything but literal values excludes the row
/// outright rather than being evaluated over the values it does carry:
/// dropping the uninterpretable element would narrow the disjunction, which
/// is safe, but leaving the rest to match would silently answer a different
/// question than the one asked.
fn in_admits(entry: &UsageRecord, left: &ast::Expr, candidates: &[ast::Expr]) -> bool {
    let ast::Expr::Identifier(name) = left else {
        return false;
    };
    let Some(field) = record_field(entry, name) else {
        return false;
    };
    if candidates
        .iter()
        .any(|candidate| !matches!(candidate, ast::Expr::Value(_)))
    {
        return false;
    }
    candidates.iter().any(|candidate| match candidate {
        ast::Expr::Value(value) => value_matches(field, value),
        _ => false,
    })
}

/// Resolves a filter identifier to the entry's value for it.
///
/// The vocabulary is `UsageRecordQuery`'s, minus the two covered-period
/// bounds. Those are reserved on the `$filter` surface — the range travels
/// as a typed parameter and the gear refuses a predicate naming either — so
/// refusing them here fails closed on a shape no conforming caller sends,
/// and it keeps a `chrono` comparison out of a crate that carries no
/// `chrono` dependency.
///
/// `None` for any other name, which excludes the row.
fn record_field<'a>(entry: &'a UsageRecord, name: &str) -> Option<FieldValue<'a>> {
    match name {
        "id" => Some(FieldValue::Uuid(entry.id)),
        "tenant_id" => Some(FieldValue::Uuid(entry.tenant_id)),
        "resource_id" => Some(FieldValue::Str(entry.resource_ref.resource_id())),
        "resource_type" => Some(FieldValue::Str(entry.resource_ref.resource_type())),
        "subject_id" => entry
            .subject_ref
            .as_ref()
            .map(|subject| FieldValue::Str(subject.subject_id())),
        "subject_type" => entry
            .subject_ref
            .as_ref()
            .and_then(|subject| subject.subject_type().map(FieldValue::Str)),
        "invalidates" => entry
            .invalidation
            .as_ref()
            .map(|inv| FieldValue::Uuid(inv.target)),
        "entry_type" => Some(FieldValue::Str(entry.entry_type().as_str())),
        "origin" => Some(FieldValue::Str(entry.origin.as_str())),
        _ => None,
    }
}

/// Compares a record attribute against a literal from the filter.
///
/// **Never route this through [`ast::Value`]'s `Display`.** That impl
/// prints the *type name* — `"uuid"`, `"string"`, `"number"` — not the
/// value, so a comparison built on it would find every UUID equal to every
/// other and produce a filter matching every row. `tenant_id` compiles to
/// [`ast::Value::Uuid`] while the other four identifiers compile to
/// [`ast::Value::String`], so both spellings have to be matched explicitly.
///
/// The cross-typed pairs mirror the gear's own coercion policy
/// (`authz::coerce_scope_value`): a UUID-typed field accepts a UUID-shaped
/// string, and a string-typed field accepts a UUID rendered canonically, so
/// a `resource_id` that happens to be a UUID matches however the PEP
/// compiler typed it. Any other pairing is a type mismatch and excludes the
/// row.
fn value_matches(field: FieldValue<'_>, value: &ast::Value) -> bool {
    match (field, value) {
        (FieldValue::Uuid(left), ast::Value::Uuid(right)) => left == *right,
        (FieldValue::Uuid(left), ast::Value::String(right)) => {
            Uuid::parse_str(right).is_ok_and(|right| left == right)
        }
        (FieldValue::Str(left), ast::Value::String(right)) => left == right,
        (FieldValue::Str(left), ast::Value::Uuid(right)) => left == right.to_string(),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

/// Groups the selected rows and folds each group.
///
/// An empty `group_by` is the no-grouping case: exactly one bucket carrying
/// an empty key, emitted even over an empty selection — `COUNT` answers
/// zero there and every other fold answers absent, the split
/// `SELECT COUNT(*)` makes against `SELECT MIN(v)` over no rows.
///
/// The bucket count is capped at [`MAX_AGGREGATION_BUCKETS`] `+ 1`. The SDK
/// puts that bound on the plugin rather than on the gateway so an
/// unbounded grouping cannot materialise in plugin memory; one bucket past
/// the cap is what lets the gateway tell "at the cap" from "over it" and
/// refuse the latter.
fn fold_rows(
    fold: AggregationFold,
    rows: &[&UsageRecord],
    group_by: &[AggregationDimension],
) -> Result<AggregationResult, UsageCollectorPluginError> {
    if group_by.is_empty() {
        return Ok(AggregationResult {
            buckets: vec![AggregationBucket {
                key: Vec::new(),
                value: fold_value(fold, rows)?,
            }],
        });
    }

    let mut groups: BTreeMap<Vec<String>, Vec<&UsageRecord>> = BTreeMap::new();
    for row in rows {
        if let Some(key) = bucket_key(row, group_by) {
            groups.entry(key).or_default().push(row);
        }
    }

    let mut buckets = Vec::with_capacity(groups.len().min(MAX_AGGREGATION_BUCKETS + 1));
    for (key, group) in groups.into_iter().take(MAX_AGGREGATION_BUCKETS + 1) {
        buckets.push(AggregationBucket {
            key,
            value: fold_value(fold, &group)?,
        });
    }
    Ok(AggregationResult { buckets })
}

/// The key one row contributes, or `None` when a dimension is absent on it.
///
/// A row with no subject is excluded from a `subject_id` grouping rather
/// than bucketed under an empty string, and the same holds for
/// `subject_type` and for a metadata key the row does not carry.
fn bucket_key(row: &UsageRecord, group_by: &[AggregationDimension]) -> Option<Vec<String>> {
    group_by
        .iter()
        .map(|dimension| match dimension {
            // `Uuid::to_string()` — lowercase and hyphenated — is the key
            // encoding DESIGN §3.11 fixes for this dimension.
            AggregationDimension::TenantId => Some(row.tenant_id.to_string()),
            AggregationDimension::ResourceId => Some(row.resource_ref.resource_id().to_owned()),
            AggregationDimension::ResourceType => Some(row.resource_ref.resource_type().to_owned()),
            AggregationDimension::SubjectId => row
                .subject_ref
                .as_ref()
                .map(|subject| subject.subject_id().to_owned()),
            AggregationDimension::SubjectType => row
                .subject_ref
                .as_ref()
                .and_then(|subject| subject.subject_type().map(ToOwned::to_owned)),
            AggregationDimension::Metadata(key) => row.metadata.get(key).cloned(),
        })
        .collect()
}

/// Applies one fold to one bucket's rows.
///
/// Accumulation is in [`BigDecimal`] rather than [`Decimal`]: a wide `SUM`
/// of in-range quantities can exceed `Decimal`'s ~7.9×10²⁸ ceiling, and the
/// aggregate surface carries `BigDecimal` for exactly that reason.
///
/// `LATEST` breaks a `window_end` tie on the greatest `id`. The declared
/// tie-break is the greatest `acceptance_sequence`, which `UsageRecord` does
/// not carry — see `BLOCKED_CHECKS` in the parent module. A total order is
/// still needed here or the answer would depend on ledger insertion order,
/// so `id` stands in; it is deterministic and it is not the declared rule,
/// and no check asserts either way.
fn fold_value(
    fold: AggregationFold,
    rows: &[&UsageRecord],
) -> Result<Option<BigDecimal>, UsageCollectorPluginError> {
    if matches!(fold, AggregationFold::Count) {
        let count = u64::try_from(rows.len()).map_err(|_| {
            UsageCollectorPluginError::internal("bucket cardinality does not fit a count")
        })?;
        return Ok(Some(BigDecimal::from(count)));
    }
    let winner = match fold {
        AggregationFold::Sum => {
            let mut total = BigDecimal::from(0);
            for row in rows {
                total += to_big_decimal(row.value)?;
            }
            return Ok(if rows.is_empty() { None } else { Some(total) });
        }
        AggregationFold::Max => rows.iter().max_by_key(|row| row.value),
        AggregationFold::Min => rows.iter().min_by_key(|row| row.value),
        AggregationFold::Latest => rows.iter().max_by_key(|row| (row.window_end, row.id)),
        AggregationFold::Count => None,
    };
    winner.map(|row| to_big_decimal(row.value)).transpose()
}

/// Widens a record's quantity to the aggregate surface's carrier.
///
/// The conversion goes through the decimal rendering, which is exact for
/// every `Decimal`: both types are base-ten, so no scale is invented and
/// none is lost.
fn to_big_decimal(value: Decimal) -> Result<BigDecimal, UsageCollectorPluginError> {
    BigDecimal::from_str(&value.to_string()).map_err(|err| {
        UsageCollectorPluginError::internal(format!(
            "a persisted quantity could not be widened to the aggregate carrier: {err}"
        ))
    })
}
