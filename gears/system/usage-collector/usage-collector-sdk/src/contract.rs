//! The DESIGN §3.3 plugin contract suite.
//!
//! DESIGN §3.3 requires every conforming plugin to pass this suite. The
//! obligation is the plugin's, so the suite is a public feature-gated module
//! rather than a `tests/` target no other crate could reach.
//!
//! The checks are behavioural: they submit entries through
//! [`UsageCollectorPluginV1`] and read them back through the same trait.
//! Nothing inspects a backend's storage, so SQL-backed, in-memory and remote
//! plugins are all subject to the same assertions.
//!
//! # Running it against your plugin
//!
//! Enable the `contract` feature on the dev-dependency:
//!
//! ```toml
//! [dev-dependencies]
//! cf-gears-usage-collector-sdk = { workspace = true, features = ["contract"] }
//! ```
//!
//! then call [`run_all`] from an ordinary async test:
//!
//! ```rust,ignore
//! use usage_collector_sdk::contract;
//!
//! #[tokio::test]
//! async fn conforms_to_the_plugin_contract() {
//!     let plugin = MyBackend::connect(&test_database_url()).await;
//!     let violations = contract::run_all(&plugin, contract::DedupLevel::Linearizable).await;
//!     assert!(violations.is_empty(), "plugin contract violations: {violations:#?}");
//! }
//! ```
//!
//! Each entry names the check that produced it ([`ContractViolation::check`]),
//! so one run reports every failure rather than stopping at the first. Under
//! [`run_all`] the suite writes entries and never removes them, and fixtures
//! are keyed so a repeated run resubmits identical entries rather than
//! colliding. Under [`run_all_with_retention`] a driven check may remove its
//! own entries and must put them back — see that entry point's caution.
//!
//! # Coverage accounting
//!
//! Asserted rather than described. [`IMPLEMENTED_CHECKS`],
//! [`UNWRITTEN_CHECKS`] and [`BLOCKED_CHECKS`] partition DESIGN's tabulated
//! checks exactly, so a half-landed check fails the partition test:
//!
//! * [`IMPLEMENTED_CHECKS`] — written and dispatched by [`run_all`].
//! * [`UNWRITTEN_CHECKS`] — expressible against the SPI, not yet written.
//!   **Empty.**
//! * [`BLOCKED_CHECKS`] — out of the SPI's reach, each with what unblocks it.
//!   **Empty.**
//!
//! [`ADDITIONAL_CHECKS`] is held disjoint from those: checks [`run_all`] runs
//! that DESIGN does not tabulate, each covering an obligation DESIGN states
//! without giving it a row. A caller reporting coverage reports every
//! constant — [`IMPLEMENTED_CHECKS`] alone under-reports what a run executed.
//!
//! **A dispatched, green check can still be short of its row.**
//! [`RETENTION_DRIVEN_CHECKS`] names the checks with assertions that need a
//! backend's retention to have swept. [`run_all`] cannot drive one and skips
//! those assertions; [`run_all_with_retention`] runs them whole.
//!
//! # The reference backend
//!
//! `reference::InMemoryReferencePlugin` is the subject this suite is validated
//! against — a conforming backend, not a production one and not a template.
//! It is `cfg(test)`-only, so it is part of this crate's test tree rather than
//! its API, and the names here are plain code spans because the module is not
//! in a doc build. See its module docs, including why the noop plugin cannot
//! serve in its place.

use crate::plugin_api::UsageCollectorPluginV1;
use checks::{
    at_most_one_invalidation, converged_target_lookup, dedup_concurrent, dedup_floor,
    dedup_identity_over_window, feed_bootstrap_position, feed_completeness, feed_position_bounded,
    feed_retention_refusal, feed_snapshot_and_replay, invalidation_excluded_from_fold,
    latest_tie_break, quantity_round_trip, raw_page_caller_order, raw_page_keyset_walk,
    reconciliation_figures, record_and_invalidation_distinct_identity,
    scope_is_a_filter_on_every_read_path, server_field_round_trip, window_end_selection,
};
use retention::ContractRetention;

mod checks;
mod feed_walk;
mod fixtures;
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod reference;
pub mod retention;

/// A check that failed, naming the check and what was observed.
///
/// Carries the check's own name so a caller can report a whole run without
/// re-deriving which assertion produced which failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractViolation {
    /// The DESIGN §3.3 check name, spelled as the table spells it — or
    /// [`HARNESS_FAULT`] when the suite itself failed rather than the
    /// plugin.
    pub check: &'static str,
    /// What was observed, and what the check required instead.
    pub detail: String,
}

impl std::fmt::Display for ContractViolation {
    /// One line per violation, `"<check>: <detail>"`.
    ///
    /// The `{violations:#?}` in this module's example is right for an
    /// `assert!` message and wrong for anything that reports per line, so
    /// the per-line rendering is the type's own rather than each caller's.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.check, self.detail)
    }
}

/// The dedup level a plugin declares in its deployment guide (DESIGN §3.1
/// "Dedup level", §3.10 item 9), handed to [`run_all`] so a check can hold the
/// plugin to the outcomes that level promises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupLevel {
    /// Every write is decided as it commits; the convergence bound is zero.
    Linearizable,
    /// A write can be acknowledged and later discarded; an identity converges
    /// within `convergence_bound`.
    Eventual {
        /// The declared convergence bound.
        convergence_bound: std::time::Duration,
    },
}

// The DESIGN §3.3 check names, spelled as the table spells them. Exported so
// a caller can tell this suite's check names apart from the
// [`UNWRITTEN_CHECKS`] and [`BLOCKED_CHECKS`] entries without matching a
// string literal.

/// The DESIGN §3.3 `quantity-round-trip` check.
pub const QUANTITY_ROUND_TRIP: &str = "quantity-round-trip";

/// The DESIGN §3.3 `window-end-selection` check.
pub const WINDOW_END_SELECTION: &str = "window-end-selection";

/// The DESIGN §3.3 `dedup-identity-over-window` check.
pub const DEDUP_IDENTITY_OVER_WINDOW: &str = "dedup-identity-over-window";

/// The DESIGN §3.3 `invalidation-excluded-from-fold` check.
pub const INVALIDATION_EXCLUDED_FROM_FOLD: &str = "invalidation-excluded-from-fold";

/// The DESIGN §3.3 `at-most-one-invalidation` check.
pub const AT_MOST_ONE_INVALIDATION: &str = "at-most-one-invalidation";

/// The DESIGN §3.3 `record-and-invalidation-distinct-identity` check.
pub const RECORD_AND_INVALIDATION_DISTINCT_IDENTITY: &str =
    "record-and-invalidation-distinct-identity";

/// The DESIGN §3.3 `converged-target-lookup` check.
pub const CONVERGED_TARGET_LOOKUP: &str = "converged-target-lookup";

/// The DESIGN §3.3 `dedup-floor` check.
pub const DEDUP_FLOOR: &str = "dedup-floor";

/// The DESIGN §3.3 `dedup-concurrent` check.
pub const DEDUP_CONCURRENT: &str = "dedup-concurrent";

/// The DESIGN §3.3 `server-field-round-trip` check.
pub const SERVER_FIELD_ROUND_TRIP: &str = "server-field-round-trip";

/// The DESIGN §3.3 `feed-snapshot-and-replay` check.
pub const FEED_SNAPSHOT_AND_REPLAY: &str = "feed-snapshot-and-replay";

/// The DESIGN §3.3 `feed-completeness` check.
pub const FEED_COMPLETENESS: &str = "feed-completeness";

/// The DESIGN §3.3 `feed-bootstrap-position` check.
pub const FEED_BOOTSTRAP_POSITION: &str = "feed-bootstrap-position";

/// The DESIGN §3.3 `feed-retention-refusal` check.
pub const FEED_RETENTION_REFUSAL: &str = "feed-retention-refusal";

/// The DESIGN §3.3 `feed-position-bounded` check.
pub const FEED_POSITION_BOUNDED: &str = "feed-position-bounded";

/// The DESIGN §3.3 `latest-tie-break` check.
pub const LATEST_TIE_BREAK: &str = "latest-tie-break";

/// The DESIGN §3.3 `reconciliation-figures` check.
pub const RECONCILIATION_FIGURES: &str = "reconciliation-figures";

/// `scope-is-a-filter-on-every-read-path` — a row outside the compiled PDP
/// scope is absent, and the surface is not an existence oracle.
///
/// DESIGN states the obligation without tabulating a check for it: §3.3 has
/// `get_usage_record` withhold a row outside the scope, and §3.2 has the Query
/// Gateway compose PDP constraints into caller filters *"so the result can
/// only narrow"*. Nothing above the SPI re-checks the rows a plugin answers
/// with, so the whole of "exists but not yours reads as `NotFound`" is the
/// plugin's to keep.
///
/// **The name states the obligation, not the coverage.** The obligation is on
/// every SPI read path; [`run_all`] dispatches this check against those taking
/// the scope inside `query.filter` or as `get_usage_record`'s `scope` — the
/// point lookup, the list path and the fold. The feed page and the
/// reconciliation read take a separate `scope: &ast::Expr` and are covered
/// elsewhere: the feed page incidentally, by [`FEED_SNAPSHOT_AND_REPLAY`] and
/// [`FEED_RETENTION_REFUSAL`] (each stores an entry the read's grant withholds
/// because its own assertion needs one); the reconciliation read deliberately,
/// by [`RECONCILIATION_FIGURES`]'s `excluded` case.
///
/// Other checks dispatch a feed read without narrowing anything — every entry
/// they store is inside the scope they send — so they buy shape coverage on
/// the path and no scope *enforcement*.
pub const SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH: &str = "scope-is-a-filter-on-every-read-path";

/// `raw-page-keyset-walk` — a paginated walk of a range returns every entry
/// exactly once, seeks rather than skips, and trims its look-ahead.
///
/// Covers `cpt-cf-usage-collector-dod-gateway-owned-cursor` on the gear side
/// and `cpt-cf-uc-plugin-dod-keyset-seek-pagination` plus
/// `cpt-cf-uc-plugin-dod-next-page-detection` on the plugin's. Only
/// expressible since the SPI began returning a structured
/// [`crate::keyset::Keyset`] rather than an encoded token.
pub const RAW_PAGE_KEYSET_WALK: &str = "raw-page-keyset-walk";

/// `raw-page-caller-order` — a caller-supplied order is honoured, survives
/// a continuation, and shapes the returned keyset.
///
/// Covers the SPI's "a plugin MUST read the order it is given rather than
/// assume a position for either key".
pub const RAW_PAGE_CALLER_ORDER: &str = "raw-page-caller-order";

/// The [`ContractViolation::check`] value a violation carries when the
/// **suite itself** failed — it could not build a fixture, say — rather
/// than the plugin.
///
/// Deliberately not a DESIGN §3.3 check name: a report that attributes the
/// harness's own fault to a check tells a plugin author their plugin broke a
/// contract it never got to touch.
pub const HARNESS_FAULT: &str = "contract-suite-harness-fault";

/// The DESIGN §3.3 checks [`run_all`] actually runs.
///
/// Part of the partition over DESIGN's tabulated checks, so a check DESIGN
/// does not name belongs in [`ADDITIONAL_CHECKS`] instead. Adding a DESIGN
/// check means adding it here as well as to [`run_all`] and removing it from
/// [`UNWRITTEN_CHECKS`]; the partition test refuses a half-landed change.
pub const IMPLEMENTED_CHECKS: &[&str] = &[
    QUANTITY_ROUND_TRIP,
    WINDOW_END_SELECTION,
    DEDUP_IDENTITY_OVER_WINDOW,
    INVALIDATION_EXCLUDED_FROM_FOLD,
    AT_MOST_ONE_INVALIDATION,
    RECORD_AND_INVALIDATION_DISTINCT_IDENTITY,
    SERVER_FIELD_ROUND_TRIP,
    DEDUP_FLOOR,
    CONVERGED_TARGET_LOOKUP,
    DEDUP_CONCURRENT,
    LATEST_TIE_BREAK,
    FEED_SNAPSHOT_AND_REPLAY,
    FEED_COMPLETENESS,
    FEED_BOOTSTRAP_POSITION,
    FEED_RETENTION_REFUSAL,
    FEED_POSITION_BOUNDED,
    RECONCILIATION_FIGURES,
];

/// The checks [`run_all`] runs that DESIGN §3.3 does not tabulate.
///
/// A constant of its own rather than extra entries in [`IMPLEMENTED_CHECKS`],
/// so the partition can be asserted to equal DESIGN's table exactly rather
/// than merely be a subset of it — a subset check cannot catch a DESIGN check
/// that half-lands. It is asserted disjoint from the partition, so nothing
/// DESIGN names can hide here either.
pub const ADDITIONAL_CHECKS: &[&str] = &[
    SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH,
    RAW_PAGE_KEYSET_WALK,
    RAW_PAGE_CALLER_ORDER,
];

/// The DESIGN §3.3 checks that are writable against the current SPI and are
/// not yet written.
///
/// **Empty** — every check DESIGN tabulates is written and dispatched. The
/// constant stands rather than being deleted because the partition test holds
/// it against DESIGN's table: a check the table grows has somewhere to be
/// named on the commit that adds it, before anyone writes it.
///
/// Covered-only-in-part is a different claim; see [`RETENTION_DRIVEN_CHECKS`].
// @cpt-dod:cpt-cf-usage-collector-dod-plugin-conformance-suite:p1
pub const UNWRITTEN_CHECKS: &[&str] = &[];

/// The DESIGN §3.3 checks the current SPI cannot express, each paired with
/// what unblocks it.
///
/// **Empty.** Kept for the same reason as [`UNWRITTEN_CHECKS`]: a check a
/// later SPI change puts out of reach needs somewhere to be named rather than
/// quietly dropping out of the accounting. See
/// `the_blocked_checks_are_the_ones_the_spi_cannot_express` for what a human
/// must establish before adding an entry, and why no test can establish it for
/// them.
pub const BLOCKED_CHECKS: &[(&str, &str)] = &[];

/// The checks [`run_all`] covers **only in part**, because at least one
/// assertion of each needs a backend's retention to have actually swept.
///
/// No SPI method removes anything, so the drive sits beside the SPI as
/// [`retention::ContractRetention`] and reaches the checks through
/// [`run_all_with_retention`]. A check named here runs under both entry points
/// and asserts strictly more under the second; how much more is in each
/// check's own docs.
///
/// Not a coverage constant and it partitions nothing — every name here is also
/// in [`IMPLEMENTED_CHECKS`], which is asserted. What it adds is the one thing
/// the others cannot say: a check can be listed as implemented, dispatched and
/// green while an assertion of it did not run.
///
/// Held mechanically too. `contract_tests`' `RETENTION_DRIVEN_MATRIX` carries
/// one subject per driven check whose only defect a driven assertion reaches,
/// and `each_driven_check_fails_against_its_own_defect_and_no_other` requires
/// each to pass under [`run_all`] and fail under [`run_all_with_retention`].
/// The matrix is asserted to name exactly the checks this constant names.
pub const RETENTION_DRIVEN_CHECKS: &[&str] = &[FEED_BOOTSTRAP_POSITION, FEED_RETENTION_REFUSAL];

/// Run every implemented check, returning one entry per violation.
///
/// An empty result means the plugin conforms as far as this suite reaches.
/// Checks run in sequence rather than concurrently: several store entries and
/// read them back, and interleaving would let one check observe another's rows.
///
/// **An empty result is not a statement about every assertion of every check
/// it dispatched.** The checks in [`RETENTION_DRIVEN_CHECKS`] skip the
/// assertions needing a retention sweep, which nothing on the SPI performs;
/// [`run_all_with_retention`] takes the drive and runs them whole.
///
/// The plugin arrives behind `&dyn` because the suite is dispatched once per
/// backend and its cost is entirely in the awaits — monomorphising buys
/// nothing, and the host's own `ClientHub` handle passes straight in.
///
/// `level` is the dedup level the plugin declares, read by
/// `at-most-one-invalidation` (its post-convergence half runs only under
/// `Eventual`), `converged-target-lookup` (waits out the declared bound) and
/// `dedup-concurrent` (the raced divergent pair is asserted only under
/// `Linearizable`, the post-bound read only under `Eventual`).
pub async fn run_all(
    plugin: &dyn UsageCollectorPluginV1,
    level: DedupLevel,
) -> Vec<ContractViolation> {
    run_every_check(plugin, level, None).await
}

/// The same suite, with the checks in [`RETENTION_DRIVEN_CHECKS`] run
/// whole.
///
/// `retention` is the capability a backend exposes beside the SPI:
/// [`retention::ContractRetention`] runs that backend's own retention sweep at
/// a moment a check chooses, the one thing the SPI methods cannot be asked
/// for. Its module docs say why that is a conforming capability rather than a
/// test hook.
///
/// **Prefer this entry point.** A backend that can be driven and is handed to
/// [`run_all`] anyway is reported as covering less for no reason.
///
/// # Caution
///
/// A driven check may **remove** entries, which no other check does, and the
/// suite's premise that a repeated run re-delivers identical entries rests on
/// nothing being removed. Each driven check must therefore restore its own
/// meter, held by
/// `the_reference_backend_conforms_to_a_repeated_run_under_a_retention_drive`.
/// What "restored" means is the check's own to decide:
/// `feed-bootstrap-position` re-delivers its entries in its own order (it
/// asserts which one a read begins at); `feed-retention-refusal` leaves its
/// swept meter **empty** (it asserts what lies after a cursor it issued, only
/// reproducible from a ledger built in front of that cursor from nothing).
/// Each restoration runs over a meter derived for one check's exclusive use,
/// and [`retention::ContractRetention::drop_before`] is keyed on a GTS type,
/// so neither can reach the other's fixtures.
pub async fn run_all_with_retention(
    plugin: &dyn UsageCollectorPluginV1,
    level: DedupLevel,
    retention: &dyn ContractRetention,
) -> Vec<ContractViolation> {
    run_every_check(plugin, level, Some(retention)).await
}

/// The suite both entry points dispatch, with the retention drive optional.
///
/// One body rather than [`run_all_with_retention`] delegating to [`run_all`]:
/// that shape would dispatch every driven check twice, and a check that writes
/// fixtures and reads them back is not idempotent within one run — the
/// undriven pass would leave `feed-bootstrap-position`'s meter stocked and the
/// driven pass would purge it, so the two passes would report on two different
/// ledgers and double every violation.
#[allow(
    clippy::cognitive_complexity,
    reason = "a flat sequence of `violations.extend(check(plugin).await)` calls, one per \
              dispatched check, with no branching of its own; the metric counts the straight-line \
              length of the dispatch list rather than any real complexity, and it grows by one \
              line every time a check is added, which is the point of this function"
)]
async fn run_every_check(
    plugin: &dyn UsageCollectorPluginV1,
    level: DedupLevel,
    retention: Option<&dyn ContractRetention>,
) -> Vec<ContractViolation> {
    let mut violations = quantity_round_trip(plugin).await;
    violations.extend(window_end_selection(plugin).await);
    violations.extend(dedup_identity_over_window(plugin).await);
    violations.extend(invalidation_excluded_from_fold(plugin).await);
    violations.extend(at_most_one_invalidation(plugin, level).await);
    violations.extend(record_and_invalidation_distinct_identity(plugin).await);
    violations.extend(server_field_round_trip(plugin).await);
    violations.extend(dedup_floor(plugin).await);
    violations.extend(converged_target_lookup(plugin, level).await);
    violations.extend(dedup_concurrent(plugin, level).await);
    violations.extend(latest_tie_break(plugin).await);
    violations.extend(feed_snapshot_and_replay(plugin).await);
    violations.extend(feed_completeness(plugin).await);
    violations.extend(feed_bootstrap_position(plugin, retention).await);
    violations.extend(feed_retention_refusal(plugin, retention).await);
    violations.extend(feed_position_bounded(plugin).await);
    violations.extend(reconciliation_figures(plugin).await);
    violations.extend(scope_is_a_filter_on_every_read_path(plugin).await);
    violations.extend(raw_page_keyset_walk(plugin).await);
    violations.extend(raw_page_caller_order(plugin).await);
    violations
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "contract_mutants.rs"]
mod contract_mutants;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "contract_tests.rs"]
mod contract_tests;
