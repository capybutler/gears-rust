//! What one `(tenant, GTS type)` scope holds, for reconciliation.
//!
//! DESIGN §3.1 defines both types. Reconciliation is the fourth read path, and
//! an operator surface: it reports ingestion activity and two watermarks so a
//! consumer's gap can be detected without folding the meter. The quantity
//! summary itself is one of two branches — an accrued total, or an
//! observation count with the latest observation — decided by the declared
//! fold; see [`QuantitySummary`].

use core::num::NonZeroU64;

use bigdecimal::BigDecimal;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::models::{AggregationFold, MeterTypeId};
use crate::quantity::UsageQuantity;

/// The fold-appropriate quantity summary, in one of two branches.
///
/// Which branch a scope reports is decided by the **declared fold** the SPI
/// call carries, never by the caller and never by the plugin: a summing meter
/// reports an accrued sum, and every other meter reports how many
/// observations the range selected together with the latest of them
/// (`usage-collector-v1.yaml`'s `ReconciliationMetadata.quantity_summary`,
/// this gear's `dod-reconciliation-quantity-summary`, and the `TimescaleDB`
/// plugin's `DESIGN.md` §3.6 `cpt-cf-uc-plugin-seq-reconciliation`).
///
/// **Quantities under a non-accruing fold are not summable**, which is why
/// the second branch reports a count and one observation rather than a fold
/// result: a `MAX` over a range answers a question about the meter, while
/// reconciliation answers a question about how much arrived.
///
/// Not `#[non_exhaustive]`, deliberately. Out-of-crate plugins **construct**
/// this value as part of their SPI return, and the attribute forbids external
/// construction outright — the same reason [`crate::FeedPage`] does not carry
/// it while [`crate::FeedStart`] does. Additive evolution of a type external
/// code constructs is inherently breaking; the constraint binds types
/// external code matches on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuantitySummary {
    /// An accruing fold's total. Zero over an empty selection, never absent:
    /// an accrual over an empty set is defined.
    Accrued(BigDecimal),
    /// Every non-accruing fold. Absent when the range selected no entry,
    /// because an observation over an empty set is not defined.
    Observations(Option<ObservedQuantity>),
}

/// A non-empty [`QuantitySummary::Observations`]: the count and the latest
/// of the records a range selected.
///
/// `count` and `latest` are folded into one type, rather than carried as two
/// independent fields, so the zero/non-zero invariant between them —
/// `count == 0` iff no `latest` — is enforced by construction instead of by
/// discipline: an empty selection is `QuantitySummary::Observations(None)`,
/// never a zero [`Self::count`] paired with a [`Self::latest`], or a
/// non-zero `count` paired with none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedQuantity {
    /// Non-withdrawn records the requested range selected. Never zero: an
    /// empty selection is the outer `Option`'s `None`, not a zero here.
    pub count: NonZeroU64,
    /// The latest of the selected records by the `LATEST` total order of
    /// DESIGN §3.1 — greatest `window_end`, then greatest `accepted_at`,
    /// then greatest `id`.
    pub latest: UsageQuantity,
}

impl QuantitySummary {
    /// Whether `fold` takes the [`Self::Accrued`] branch.
    ///
    /// Exhaustive rather than `matches!(fold, Sum)`, so a fold admitted later
    /// needs a deliberate decision about which branch it falls in instead of
    /// being swept into the observation branch by a catch-all.
    #[must_use]
    pub const fn accrues(fold: AggregationFold) -> bool {
        match fold {
            AggregationFold::Sum => true,
            AggregationFold::Count
            | AggregationFold::Max
            | AggregationFold::Min
            | AggregationFold::Latest => false,
        }
    }

    /// The summary for a selection holding nothing, under a known fold.
    #[must_use]
    pub fn empty_for(fold: AggregationFold) -> Self {
        if Self::accrues(fold) {
            Self::Accrued(BigDecimal::from(0))
        } else {
            Self::Observations(None)
        }
    }
}

/// Accepted count, a fold-appropriate summary, and two watermarks, for one
/// `(tenant, GTS type)` scope.
///
/// The first two figures answer different questions and deliberately
/// disagree. `accepted_count` counts every accepted entry the range selects,
/// invalidations included, because it reports **ingestion activity**.
/// `quantity_summary` excludes both halves of every withdrawn pair, because
/// it reports the **meter**. A range holding nothing but a withdrawn pair is
/// where the two are furthest apart, and it is an ordinary case rather than
/// an edge one.
///
/// Both watermarks are the scope's `max(accepted_at)` and `max(window_end)`,
/// **unbounded by the request's range**, and absent when the scope holds no
/// entries. A range selecting nothing therefore leaves them populated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconciliationMetadata {
    /// Every accepted entry the range selects, invalidations included.
    pub accepted_count: u64,
    /// The summary, in the branch the declared fold selects.
    pub quantity_summary: QuantitySummary,
    /// The scope's greatest acceptance instant, unbounded by the range.
    pub max_accepted_at: Option<OffsetDateTime>,
    /// The scope's greatest covered-period end, unbounded by the range.
    pub max_window_end: Option<OffsetDateTime>,
}

impl ReconciliationMetadata {
    /// The answer for a scope holding no entries at all, under a known fold.
    ///
    /// **This is a complete answer, not a base to fill in**, which is what
    /// distinguishes it from the constructor it replaces. That one could not
    /// be complete: it was built without the fold, and the fold decides
    /// `quantity_summary` even over an empty selection, so it left the field
    /// at a value that was right for three folds and wrong for two — and a
    /// backend that filled in a count and both watermarks but forgot the
    /// field compiled with no diagnostic. Taking the fold removes the
    /// forgettable field rather than documenting it.
    ///
    /// A tenant the compiled scope excludes answers exactly this, per the SPI
    /// contract — never an error.
    #[must_use]
    pub fn empty_for(fold: AggregationFold) -> Self {
        Self {
            accepted_count: 0,
            quantity_summary: QuantitySummary::empty_for(fold),
            max_accepted_at: None,
            max_window_end: None,
        }
    }
}

/// The reporting granularity — a REST-level concept only.
///
/// DESIGN §3.1: v1 admits `(tenant, gts_type)` alone, carried as the required
/// `tenant_id` and `gts_type_id` query parameters, "so no SPI type corresponds
/// to it". The SPI takes the two values as separate arguments; this type is
/// what REST parses them into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconciliationScope {
    /// The tenant reported on.
    pub tenant_id: Uuid,
    /// The meter reported on.
    pub gts_type_id: MeterTypeId,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "reconciliation_tests.rs"]
mod reconciliation_tests;
