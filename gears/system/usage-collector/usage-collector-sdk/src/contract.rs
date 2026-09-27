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
//!     let violations = contract::run_all(&plugin, contract::DedupLevel::Linearizable).await;
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
//! # Eight of DESIGN's sixteen, nine checks in all
//!
//! DESIGN §3.3 tabulates sixteen checks. [`run_all`] currently runs the
//! eight in [`IMPLEMENTED_CHECKS`], and **an empty violation list is not a
//! statement about the rest**.
//!
//! **Two counts run through this file, and both are right.** Eight is what
//! [`run_all`] covers of DESIGN's sixteen; nine is how many checks it
//! runs, the ninth being the one in [`ADDITIONAL_CHECKS`] that DESIGN
//! does not tabulate. A count about coverage of DESIGN is therefore eight
//! and a count about what a run executed is nine, and neither substitutes
//! for the other. The other eight of the sixteen are named, not omitted:
//!
//! * [`UNWRITTEN_CHECKS`] — expressible against the seven methods this
//!   gear's SPI declares, not yet written. All eight.
//! * [`BLOCKED_CHECKS`] — out of the SPI's reach, each with what unblocks
//!   it. Now empty: no check DESIGN tabulates is beyond the current trait.
//!
//! The three constants are asserted to partition DESIGN's sixteen exactly,
//! so the split is a fact the test suite keeps rather than a paragraph that
//! drifts: a check that half-lands fails the partition, and a check that
//! stops being accounted for fails it too. A caller reporting coverage
//! should report all three alongside the violations — which matters
//! because "run this suite" is the acceptance criterion for porting a
//! backend, and a suite that runs nine checks must not read as a suite
//! that ran sixteen.
//!
//! # A check DESIGN does not tabulate
//!
//! [`run_all`] also runs [`SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH`], which is
//! **not** one of DESIGN §3.3's sixteen names. Do not look for it there and
//! do not read its absence as drift in either direction: DESIGN states the
//! obligation without tabulating a check for it — §3.3 requires a plugin's
//! `get_usage_record` to withhold a row outside the compiled scope and
//! obliges the surface not to be an existence oracle, and §3.2 has the
//! Query Gateway compose the PDP constraints into the collection paths'
//! filters *"so the result can only narrow"*. Since the point lookup's
//! in-process per-record attribution check was retired, that guarantee is
//! the plugin's alone on every one of the SPI's five read paths, and a
//! suite that never stores a row outside the scope it dispatches cannot
//! see it kept or broken. The check covers three of the five — see
//! [`SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH`] for which two it leaves to the
//! SPI's docs alone.
//!
//! It is named in [`ADDITIONAL_CHECKS`] rather than in
//! [`IMPLEMENTED_CHECKS`], so the three-way partition over DESIGN's
//! sixteen keeps meaning exactly what it meant, and a fourth assertion
//! holds this constant disjoint from all three — a DESIGN check cannot be
//! smuggled in here to escape the partition. A caller reporting coverage
//! should report it alongside them: [`IMPLEMENTED_CHECKS`] alone
//! under-reports what [`run_all`] ran.
//!
//! # The reference backend
//!
//! [`reference::InMemoryReferencePlugin`] is the subject this suite is
//! validated against. It is a conforming backend, not a production one and
//! not a template — see its module docs, which also say why the noop plugin
//! cannot serve in its place.

use crate::plugin_api::UsageCollectorPluginV1;
use checks::{
    at_most_one_invalidation, dedup_floor, dedup_identity_over_window,
    invalidation_excluded_from_fold, quantity_round_trip,
    record_and_invalidation_distinct_identity, scope_is_a_filter_on_every_read_path,
    server_field_round_trip, window_end_selection,
};

mod checks;
mod fixtures;
pub mod reference;
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

/// The DESIGN §3.3 `quantity-round-trip` check, spelled as the table spells
/// it. Exported so a caller can tell this suite's own check names apart
/// from the [`UNWRITTEN_CHECKS`] and [`BLOCKED_CHECKS`] entries without
/// matching a string literal.
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

/// The DESIGN §3.3 `record-and-invalidation-distinct-identity` check,
/// spelled as the table spells it. Exported for the same reason as
/// [`QUANTITY_ROUND_TRIP`].
pub const RECORD_AND_INVALIDATION_DISTINCT_IDENTITY: &str =
    "record-and-invalidation-distinct-identity";

/// The DESIGN §3.3 `converged-target-lookup` check, spelled as the table
/// spells it. Exported for the same reason as [`QUANTITY_ROUND_TRIP`].
pub const CONVERGED_TARGET_LOOKUP: &str = "converged-target-lookup";

/// The DESIGN §3.3 `dedup-floor` check, spelled as the table spells it.
/// Exported for the same reason as [`QUANTITY_ROUND_TRIP`].
pub const DEDUP_FLOOR: &str = "dedup-floor";

/// The DESIGN §3.3 `dedup-concurrent` check, spelled as the table spells
/// it. Exported for the same reason as [`QUANTITY_ROUND_TRIP`].
pub const DEDUP_CONCURRENT: &str = "dedup-concurrent";

/// The DESIGN §3.3 `server-field-round-trip` check, spelled as the table
/// spells it. Exported for the same reason as [`QUANTITY_ROUND_TRIP`].
pub const SERVER_FIELD_ROUND_TRIP: &str = "server-field-round-trip";

/// The DESIGN §3.3 `feed-snapshot-and-replay` check, spelled as the table
/// spells it. Exported for the same reason as [`QUANTITY_ROUND_TRIP`].
pub const FEED_SNAPSHOT_AND_REPLAY: &str = "feed-snapshot-and-replay";

/// The DESIGN §3.3 `feed-completeness` check, spelled as the table spells
/// it. Exported for the same reason as [`QUANTITY_ROUND_TRIP`].
pub const FEED_COMPLETENESS: &str = "feed-completeness";

/// The DESIGN §3.3 `feed-bootstrap-position` check, spelled as the table
/// spells it. Exported for the same reason as [`QUANTITY_ROUND_TRIP`].
pub const FEED_BOOTSTRAP_POSITION: &str = "feed-bootstrap-position";

/// The DESIGN §3.3 `feed-retention-refusal` check, spelled as the table
/// spells it. Exported for the same reason as [`QUANTITY_ROUND_TRIP`].
pub const FEED_RETENTION_REFUSAL: &str = "feed-retention-refusal";

/// The DESIGN §3.3 `feed-position-bounded` check, spelled as the table
/// spells it. Exported for the same reason as [`QUANTITY_ROUND_TRIP`].
pub const FEED_POSITION_BOUNDED: &str = "feed-position-bounded";

/// The DESIGN §3.3 `latest-tie-break` check, spelled as the table spells
/// it. Exported for the same reason as [`QUANTITY_ROUND_TRIP`].
pub const LATEST_TIE_BREAK: &str = "latest-tie-break";

/// The `scope-is-a-filter-on-every-read-path` check, which DESIGN §3.3 does
/// **not** tabulate.
///
/// Spelled in DESIGN's own style so a report reads uniformly, and named in
/// [`ADDITIONAL_CHECKS`] rather than [`IMPLEMENTED_CHECKS`] so the
/// three-way partition over DESIGN's sixteen stays exact.
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
///
/// **The name overstates what the check reaches, and deliberately: it
/// names the obligation rather than the coverage.** The SPI has five read
/// paths and the obligation is on all five. [`run_all`] dispatches this
/// check against three of them — the point lookup, the list path and the
/// fold — which are the three that take the compiled scope inside
/// `query.filter` or as `get_usage_record`'s `scope`. The feed page and
/// the reconciliation read take a separate `scope: &ast::Expr` and are
/// covered by no check here: their obligation is stated in their SPI docs
/// (an entry outside the scope is absent from a feed page; a tenant the
/// scope excludes answers exactly as one holding no entries) and asserted
/// nowhere in this suite. A porter reading a green run should not read it
/// as those two paths being exercised.
///
/// `server-field-round-trip` does dispatch a feed read, and it narrows
/// nothing here: every entry that check stores is inside the scope it
/// sends, so the read buys shape coverage on the path — a plugin that
/// chokes on a compiled scope there meets one — and no scope *enforcement*
/// whatever, for the reason the suite's shared single-tenant filter buys
/// none either. A feed that ignored its `scope` argument outright passes
/// every check this suite runs today.
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
/// checks that are not among DESIGN's sixteen. This constant is one of the
/// three that partition those sixteen, so a check DESIGN does not name has
/// no business here.
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
    RECORD_AND_INVALIDATION_DISTINCT_IDENTITY,
    SERVER_FIELD_ROUND_TRIP,
    DEDUP_FLOOR,
];

/// The checks [`run_all`] runs that DESIGN §3.3 does not tabulate.
///
/// A fourth constant rather than a sixth entry in [`IMPLEMENTED_CHECKS`],
/// and the choice is what keeps the partition test meaningful. The other
/// three are asserted to be exactly DESIGN's sixteen, disjoint; folding a
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
/// **Eight of DESIGN's sixteen**, which is every check [`run_all`] does
/// not run. Each is expressible against the seven methods this gear's SPI
/// declares — nothing here waits on the SPI to grow, and
/// [`BLOCKED_CHECKS`] is empty. Being unwritten is a statement about this
/// crate's progress, not about the SPI's reach, and the two are different
/// claims: one is closed by writing a check, the other only by changing
/// the trait.
///
/// **[`FEED_RETENTION_REFUSAL`] needed one thing more than an author, and
/// it now has it.** Its refusal half asserts that a cursor whose
/// continuation retention has truncated is refused, and a backend only
/// reaches that state once retention has purged an entry. No SPI method
/// purges, which is why the drive sits beside the SPI:
/// [`retention::ContractRetention`] is the optional capability a backend
/// under test exposes, and [`reference::InMemoryReferencePlugin`]
/// implements it, so `CursorBeyondRetention` is reachable there. The check
/// itself is still unwritten, and so is the entry point that would hand
/// [`run_all`] a driver — both land together, because an entry point taking
/// a driver it cannot yet use would be a claim about a check that does not
/// exist. This was a gap in the harness rather than in the SPI, which is
/// why the name stayed here and never went back into [`BLOCKED_CHECKS`].
///
/// **A caller reporting coverage has to report this constant.**
/// [`run_all`] returning no violations says nothing whatever about a check
/// it never ran, so a green run read against [`IMPLEMENTED_CHECKS`] alone
/// reports eight checks' worth of evidence as sixteen.
// @cpt-dod:cpt-cf-usage-collector-dod-plugin-conformance-suite:p1
pub const UNWRITTEN_CHECKS: &[&str] = &[
    CONVERGED_TARGET_LOOKUP,
    DEDUP_CONCURRENT,
    FEED_SNAPSHOT_AND_REPLAY,
    FEED_COMPLETENESS,
    FEED_BOOTSTRAP_POSITION,
    FEED_RETENTION_REFUSAL,
    FEED_POSITION_BOUNDED,
    LATEST_TIE_BREAK,
];

/// The DESIGN §3.3 checks the current SPI cannot express, each paired with
/// what unblocks it.
///
/// **Empty**, and the two entries that used to be here left without being
/// written — which is why the constant stands rather than being deleted.
/// Both justifications had become provably false:
///
/// * `feed-snapshot-and-replay` was blocked on an SPI with no feed method.
///   [`UsageCollectorPluginV1`] now declares `read_feed_page`, so the check
///   is expressible and merely unwritten.
/// * `latest-tie-break` was blocked on a rule that read an
///   `acceptance_sequence` field [`UsageRecord`](crate::models::UsageRecord)
///   does not carry. The rule DESIGN §3.1 settled on does not read it:
///   greatest `window_end`, then greatest `accepted_at`, then greatest `id`
///   in byte order, three fields the record does carry.
///
/// Both are in [`UNWRITTEN_CHECKS`] now. Keeping the constant matters
/// because it is one of the three the partition test holds against
/// DESIGN's sixteen: a check a later SPI change puts out of reach needs
/// somewhere to be named, and the alternative to naming it is a check that
/// quietly stops being accounted for. See
/// `the_blocked_checks_are_the_ones_the_spi_cannot_express` for what a
/// human must establish before adding an entry, and for why no test can
/// establish it for them.
pub const BLOCKED_CHECKS: &[(&str, &str)] = &[];

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
///
/// `level` is the dedup level the plugin declares. Only `at-most-one-invalidation` reads it today.
pub async fn run_all(
    plugin: &dyn UsageCollectorPluginV1,
    level: DedupLevel,
) -> Vec<ContractViolation> {
    let mut violations = quantity_round_trip(plugin).await;
    violations.extend(window_end_selection(plugin).await);
    violations.extend(dedup_identity_over_window(plugin).await);
    violations.extend(invalidation_excluded_from_fold(plugin).await);
    violations.extend(at_most_one_invalidation(plugin, level).await);
    violations.extend(record_and_invalidation_distinct_identity(plugin).await);
    violations.extend(server_field_round_trip(plugin).await);
    violations.extend(dedup_floor(plugin).await);
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
