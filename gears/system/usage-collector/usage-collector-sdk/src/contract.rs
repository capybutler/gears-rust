//! The DESIGN §3.3 plugin contract suite.
//!
//! DESIGN §3.3 "Plugin SPI" says: *"Every conforming plugin MUST pass the
//! suite in `usage-collector-sdk`. The tests are behavioural and MUST pass
//! on any backend."* That obligation is on the plugin, so the suite cannot
//! live in this crate's `tests/` directory — an integration-test target is
//! private to its own crate and no plugin crate can reach into it. It is a
//! public, feature-gated module instead, and this crate's own tests are one
//! caller of it rather than its home.
//!
//! The checks are behavioural: they submit entries through
//! [`UsageCollectorPluginV1`] and read them back through the same trait.
//! Nothing here inspects a backend's storage, so a SQL-backed plugin, an
//! in-memory one and a remote one are all subject to the same assertions.
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
//!     let violations = contract::run_all(&plugin).await;
//!     assert!(violations.is_empty(), "plugin contract violations: {violations:#?}");
//! }
//! ```
//!
//! Every entry of the returned vector names the check that produced it
//! ([`ContractViolation::check`]), so one run reports every failure rather
//! than stopping at the first. The suite writes entries and never removes
//! them, so a backend under test starts each run from whatever state the
//! previous one left; the fixtures are keyed so that a repeated run
//! resubmits identical entries rather than colliding with different ones.
//!
//! # Five of seven, and where the other two are
//!
//! DESIGN §3.3 tabulates seven checks. [`run_all`] currently runs the ones
//! in [`IMPLEMENTED_CHECKS`], and **an empty violation list is not a
//! statement about the rest**. The other two are named, not omitted:
//!
//! * [`BLOCKED_CHECKS`] — cannot be written against the SPI this gear
//!   declares, each with what unblocks it.
//! * [`UNWRITTEN_CHECKS`] — writable today, not yet written. Now empty:
//!   every check the current SPI can express is written.
//!
//! The three constants are asserted to partition DESIGN's seven exactly, so
//! the split is a fact the test suite keeps rather than a paragraph that
//! drifts: [`UNWRITTEN_CHECKS`] emptied itself as the work landed, and a
//! check that half-lands fails the partition. A caller reporting coverage
//! should report all three alongside the violations — which matters
//! because "run this suite" is the acceptance criterion for porting a
//! backend, and a suite that runs five checks must not read as a suite
//! that ran seven.
//!
//! # A check DESIGN does not tabulate
//!
//! [`run_all`] also runs [`SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH`], which is
//! **not** one of DESIGN §3.3's seven names. Do not look for it there and
//! do not read its absence as drift in either direction: DESIGN states the
//! obligation without tabulating a check for it — §3.3 requires a plugin's
//! `get_usage_record` to withhold a row outside the compiled scope and
//! obliges the surface not to be an existence oracle, and §3.2 has the
//! Query Gateway compose the PDP constraints into the collection paths'
//! filters *"so the result can only narrow"*. Since the point lookup's
//! in-process per-record attribution check was retired, that guarantee is
//! the plugin's alone on all three read paths, and a suite that never
//! stores a row outside the scope it dispatches cannot see it kept or
//! broken.
//!
//! It is named in [`ADDITIONAL_CHECKS`] rather than in
//! [`IMPLEMENTED_CHECKS`], so the three-way partition over DESIGN's seven
//! keeps meaning exactly what it meant, and a fourth assertion holds this
//! constant disjoint from all three — a DESIGN check cannot be smuggled in
//! here to escape the partition. A caller reporting coverage should report
//! it alongside them: [`IMPLEMENTED_CHECKS`] alone under-reports what
//! [`run_all`] ran.
//!
//! # The reference backend
//!
//! [`reference::InMemoryReferencePlugin`] is the subject this suite is
//! validated against. It is a conforming backend, not a production one and
//! not a template — see its module docs, which also say why the noop plugin
//! cannot serve in its place.

use crate::plugin_api::UsageCollectorPluginV1;
use checks::{
    at_most_one_invalidation, dedup_identity_over_window, invalidation_excluded_from_fold,
    quantity_round_trip, scope_is_a_filter_on_every_read_path, window_end_selection,
};

mod checks;
mod fixtures;
pub mod reference;

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

/// The DESIGN §3.3 `quantity-round-trip` check, spelled as the table spells
/// it. Exported so a caller can tell this suite's own check names apart
/// from the [`BLOCKED_CHECKS`] entries without matching a string literal.
pub const QUANTITY_ROUND_TRIP: &str = "quantity-round-trip";

/// The DESIGN §3.3 `window-end-selection` check, spelled as the table
/// spells it. Exported for the same reason as [`QUANTITY_ROUND_TRIP`].
pub const WINDOW_END_SELECTION: &str = "window-end-selection";

/// The DESIGN §3.3 `dedup-identity-over-window` check, spelled as the table
/// spells it. Exported for the same reason as [`QUANTITY_ROUND_TRIP`].
pub const DEDUP_IDENTITY_OVER_WINDOW: &str = "dedup-identity-over-window";

/// The DESIGN §3.3 `invalidation-excluded-from-fold` check, spelled as the
/// table spells it. Exported for the same reason as [`QUANTITY_ROUND_TRIP`].
pub const INVALIDATION_EXCLUDED_FROM_FOLD: &str = "invalidation-excluded-from-fold";

/// The DESIGN §3.3 `at-most-one-invalidation` check, spelled as the table
/// spells it. Exported for the same reason as [`QUANTITY_ROUND_TRIP`].
pub const AT_MOST_ONE_INVALIDATION: &str = "at-most-one-invalidation";

/// The `scope-is-a-filter-on-every-read-path` check, which DESIGN §3.3 does
/// **not** tabulate.
///
/// Spelled in DESIGN's own style so a report reads uniformly, and named in
/// [`ADDITIONAL_CHECKS`] rather than [`IMPLEMENTED_CHECKS`] so the
/// three-way partition over DESIGN's seven stays exact.
///
/// DESIGN states the obligation without giving it a row in the table: §3.3
/// gives the SPI's `get_usage_record` the doc *"`scope` is the compiled PDP
/// scope, projected into a `toolkit_odata` filter. A row outside it is not
/// returned"*, and §3.2 has the Query Gateway compose the PDP constraints
/// with caller filters *"so the result can only narrow"*. With the point
/// lookup's in-process per-record attribution check retired, every read
/// path carries the scope as a filter and nothing above the SPI re-checks
/// the rows a plugin answers with, so the whole of "exists but not yours
/// reads as `NotFound`" is the plugin's to keep.
pub const SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH: &str = "scope-is-a-filter-on-every-read-path";

/// The [`ContractViolation::check`] value a violation carries when the
/// **suite itself** failed — it could not build a fixture, say — rather
/// than the plugin.
///
/// Deliberately not a DESIGN §3.3 check name, and deliberately not
/// [`QUANTITY_ROUND_TRIP`]: a report that attributes the harness's own
/// fault to a check tells a plugin author their plugin broke a contract it
/// never got to touch.
pub const HARNESS_FAULT: &str = "contract-suite-harness-fault";

/// The DESIGN §3.3 checks [`run_all`] actually runs.
///
/// **Not everything [`run_all`] runs** — see [`ADDITIONAL_CHECKS`] for the
/// checks that are not among DESIGN's seven. This constant is one of the
/// three that partition those seven, so a check DESIGN does not name has no
/// business here.
///
/// Adding a DESIGN check means adding it here as well as to [`run_all`] and
/// removing it from [`UNWRITTEN_CHECKS`]; the partition test refuses a
/// half-landed change.
pub const IMPLEMENTED_CHECKS: &[&str] = &[
    QUANTITY_ROUND_TRIP,
    WINDOW_END_SELECTION,
    DEDUP_IDENTITY_OVER_WINDOW,
    INVALIDATION_EXCLUDED_FROM_FOLD,
    AT_MOST_ONE_INVALIDATION,
];

/// The checks [`run_all`] runs that DESIGN §3.3 does not tabulate.
///
/// A fourth constant rather than a sixth entry in [`IMPLEMENTED_CHECKS`],
/// and the choice is what keeps the partition test meaningful. The other
/// three are asserted to be exactly DESIGN's seven, disjoint; folding a
/// name DESIGN never wrote into one of them would force that assertion to
/// be relaxed to a subset check, and a subset check cannot catch the thing
/// the partition exists to catch — a DESIGN check that half-lands, or one
/// silently dropped from the accounting. This constant is instead asserted
/// disjoint from all three, so nothing DESIGN names can hide here either.
///
/// A caller reporting coverage should report it alongside the other three.
/// [`IMPLEMENTED_CHECKS`] on its own under-reports what a run covered,
/// which is the mirror of the failure the coverage constants exist to
/// prevent.
pub const ADDITIONAL_CHECKS: &[&str] = &[SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH];

/// The DESIGN §3.3 checks that are writable against the current SPI and are
/// not yet written.
///
/// **Empty.** Every check expressible with the five methods this gear
/// declares is written and run by [`run_all`]; what remains unimplemented
/// is in [`BLOCKED_CHECKS`], which needs the SPI to grow. The constant
/// stands rather than being deleted: it is one of the three the partition
/// test holds against DESIGN's seven, so a check that becomes writable and
/// is not yet written has a place to be named, and a check that half-lands
/// still fails the partition.
pub const UNWRITTEN_CHECKS: &[&str] = &[];

/// The two DESIGN §3.3 checks the current SPI cannot express, and why.
pub const BLOCKED_CHECKS: &[(&str, &str)] = &[
    (
        "feed-snapshot-and-replay",
        "the gear's SPI declares no feed method: DESIGN section 3.3 gives \
         `UsageCollectorPluginV1` a `read_feed_page`, and this gear \
         implements five methods, none of which reads a feed. Unblocked by \
         the usage feed.",
    ),
    (
        "latest-tie-break",
        "asserts `greatest window_end, then greatest acceptance_sequence`, \
         and `UsageRecord` carries no `acceptance_sequence` field. DESIGN \
         section 1.2 has the plugin assign it strictly monotonic per \
         `(tenant_id, gts_type_id)`, restated as a storage obligation in \
         section 3.7; until the field exists there is nothing for a plugin \
         to assign or a fold to read.",
    ),
];

/// Run every implemented check, returning one entry per violation.
///
/// An empty result means the plugin conforms as far as this suite reaches.
/// The checks are run in sequence rather than concurrently: several store
/// entries and then read them back, and interleaving them would let one
/// check observe another's rows.
///
/// The plugin arrives behind `&dyn` rather than a generic parameter. The
/// suite is dispatched once per backend and its cost is entirely in the
/// awaits, so monomorphising it buys nothing; erasing it means the host's
/// own handle — `ClientHub` hands out a `dyn UsageCollectorPluginV1` —
/// passes straight in.
pub async fn run_all(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    let mut violations = quantity_round_trip(plugin).await;
    violations.extend(window_end_selection(plugin).await);
    violations.extend(dedup_identity_over_window(plugin).await);
    violations.extend(invalidation_excluded_from_fold(plugin).await);
    violations.extend(at_most_one_invalidation(plugin).await);
    violations.extend(scope_is_a_filter_on_every_read_path(plugin).await);
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
