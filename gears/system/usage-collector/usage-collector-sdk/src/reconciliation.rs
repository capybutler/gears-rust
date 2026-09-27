//! What one `(tenant, GTS type)` scope holds, for reconciliation.
//!
//! DESIGN §3.1 defines both types. Reconciliation is the fourth read path, and
//! an operator surface: it reports ingestion activity and two watermarks so a
//! consumer's gap can be detected without folding the meter.

use bigdecimal::BigDecimal;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::models::MeterTypeId;

/// Accepted count, a fold summary, and two watermarks, for one scope.
///
/// The two figures answer different questions and deliberately disagree.
/// `accepted_count` counts every accepted entry the range selects,
/// invalidations included, because it reports **ingestion activity**.
/// `quantity_summary` applies the type's declared fold and excludes withdrawn
/// pairs, because it reports the **meter**. See `quantity_summary`'s own doc
/// for how it tells a defined zero apart from an undefined fold.
///
/// Both watermarks are the scope's `max(accepted_at)` and `max(window_end)`,
/// **unbounded by the request's range**, and absent when the scope holds no
/// entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconciliationMetadata {
    /// Every accepted entry the range selects, invalidations included.
    pub accepted_count: u64,
    /// The declared fold over the range, excluding withdrawn pairs.
    ///
    /// The fold arrives as a parameter to the SPI call, so this field carries
    /// its value without naming which fold produced it. `None` where the fold
    /// is undefined over an empty selection — `MAX`, `MIN`, `LATEST` — and
    /// `Some(0)` where it is defined — `SUM`, `COUNT`. The distinction is
    /// load-bearing: a range holding nothing but a withdrawn pair reports a
    /// defined zero under `SUM` and an absent fold under `MAX`.
    pub quantity_summary: Option<BigDecimal>,
    /// The scope's greatest acceptance instant, unbounded by the range.
    pub max_accepted_at: Option<OffsetDateTime>,
    /// The scope's greatest covered-period end, unbounded by the range.
    pub max_window_end: Option<OffsetDateTime>,
}

impl ReconciliationMetadata {
    /// The base a backend fills in: zero accepted, no watermarks, and no
    /// fold.
    ///
    /// Shared so that every backend with nothing to report answers
    /// identically, rather than each inventing its own spelling of "nothing
    /// here". A plugin that has a count and watermarks starts from this and
    /// overrides what differs.
    ///
    /// **It is not, on its own, the answer for a scope holding no entries.**
    /// It cannot be: it is built without the fold, and the fold decides
    /// `quantity_summary` even over an empty selection — `SUM` and `COUNT`
    /// report a defined zero there, `MAX`, `MIN` and `LATEST` report absent.
    /// A caller that knows its fold therefore has to set the field
    /// accordingly, which is exactly why this constructor leaves it `None`
    /// rather than guessing: absent is the right answer for three of the five
    /// folds and the wrong answer for the other two, and a constructor that
    /// cannot see the fold cannot tell which it is being used under.
    ///
    /// The convention is not a guarantee, the same as `UsageRecord`'s
    /// (`models.rs:1192-1197`): fields are public and the type is not
    /// `#[non_exhaustive]`, so nothing stops a field being left at its
    /// `empty()` default by omission. `quantity_summary` is the field where
    /// that is most costly — a backend that fills in a count and both
    /// watermarks but forgets it compiles with no diagnostic and silently
    /// reports an absent fold rather than the defined value it meant to send.
    /// Under `SUM` that is a wrong answer rather than a missing one: a
    /// consumer reading it cannot tell a meter that summed to zero from one
    /// whose fold could not be taken.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            accepted_count: 0,
            quantity_summary: None,
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
