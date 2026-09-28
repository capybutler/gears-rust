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
//! than stopping at the first. Under [`run_all`] the suite writes entries
//! and never removes them, so a backend under test starts each run from
//! whatever state the previous one left; the fixtures are keyed so that a
//! repeated run resubmits identical entries rather than colliding with
//! different ones. Under [`run_all_with_retention`] a driven check may
//! remove its own entries, and is then responsible for putting them back —
//! see that entry point's caution.
//!
//! # All sixteen of DESIGN's, seventeen checks in all
//!
//! DESIGN §3.3 tabulates sixteen checks. [`run_all`] now runs all sixteen,
//! which are [`IMPLEMENTED_CHECKS`], and **an empty violation list is still
//! not a statement about every assertion of every one of them** — see
//! [`RETENTION_DRIVEN_CHECKS`] below.
//!
//! **Two counts run through this file, and both are right.** Sixteen is
//! what [`run_all`] covers of DESIGN's sixteen; seventeen is how many checks
//! it runs, the seventeenth being the one in [`ADDITIONAL_CHECKS`] that
//! DESIGN does not tabulate. A count about coverage of DESIGN is therefore
//! sixteen and a count about what a run executed is seventeen, and neither
//! substitutes for the other. The two parted company again the moment the
//! last DESIGN check landed, which is what they do whenever either grows:
//! one is a count over DESIGN's table and the other a count over this
//! suite's dispatch list, and nothing keeps them together. Nothing DESIGN
//! tabulates is left out:
//!
//! * [`UNWRITTEN_CHECKS`] — expressible against the seven methods this
//!   gear's SPI declares, not yet written. **Empty.**
//! * [`BLOCKED_CHECKS`] — out of the SPI's reach, each with what unblocks
//!   it. Empty too: no check DESIGN tabulates is beyond the current trait.
//!
//! **And a check [`run_all`] does dispatch can still be covered only in
//! part.** [`RETENTION_DRIVEN_CHECKS`] names the checks with assertions
//! that need a backend's retention to have swept; [`run_all`] cannot drive
//! one and skips them, and [`run_all_with_retention`] runs them whole. A
//! caller reporting coverage reports that constant too: it is the one way a
//! green, dispatched, implemented check can still be short of its row.
//!
//! The three constants are asserted to partition DESIGN's sixteen exactly,
//! so the split is a fact the test suite keeps rather than a paragraph that
//! drifts: a check that half-lands fails the partition, and a check that
//! stops being accounted for fails it too. A caller reporting coverage
//! should report all three alongside the violations — which matters
//! because "run this suite" is the acceptance criterion for porting a
//! backend, and a suite that runs some of the sixteen must not read as a
//! suite that ran all of them. The count is deliberately not repeated here:
//! it is stated once above and asserted by the partition test, and a second
//! copy is a number the next author has to remember.
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
    at_most_one_invalidation, converged_target_lookup, dedup_concurrent, dedup_floor,
    dedup_identity_over_window, feed_bootstrap_position, feed_completeness, feed_position_bounded,
    feed_retention_refusal, feed_snapshot_and_replay, invalidation_excluded_from_fold,
    latest_tie_break, quantity_round_trip, record_and_invalidation_distinct_identity,
    scope_is_a_filter_on_every_read_path, server_field_round_trip, window_end_selection,
};
use retention::ContractRetention;

mod checks;
mod feed_walk;
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
/// scope, projected into a `toolkit_odata` filter. A row outside it is
/// absent"*, and §3.2 has the Query Gateway compose the PDP constraints
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
/// the reconciliation read take a separate `scope: &ast::Expr`, and this
/// check reaches neither.
///
/// **The feed page is no longer uncovered, though it is covered by other
/// checks and for other reasons.** Two of them narrow, and each does it
/// incidentally. [`FEED_SNAPSHOT_AND_REPLAY`] stores entries under two
/// tenants on a meter of its own and walks them under a grant naming one,
/// so a feed that ignored its `scope` argument outright now delivers
/// entries that check reports as never written under the grant it read.
/// [`FEED_RETENTION_REFUSAL`] narrows the same way, and needs to: its
/// sharpest clause is that a cursor is refused *"whether or not the
/// caller's scope admitted that entry"*, which is unaskable without an
/// entry the read's own grant withholds, so it stores one under the
/// suite's excluded tenant too.
///
/// Neither is an assertion about authorization — each is a consequence of
/// what its own check needs, a withheld entry between every pair of
/// admitted ones in the first case and a withheld removal in the second —
/// and each is reported under its own check's name, which is what a porter
/// debugging a scope bug on the feed path will see first. A porter who
/// wants the enforcement obligation stated should still read the SPI doc:
/// *"`scope` is the compiled PDP scope. An entry outside it is absent."*
///
/// **The reconciliation read is still reached by nothing.** Its obligation
/// is in its SPI doc — a tenant the scope excludes answers exactly as one
/// holding no entries — and asserted nowhere in this suite. A porter
/// reading a green run should not read it as that path being exercised.
///
/// Further checks dispatch a feed read — `server-field-round-trip`,
/// `feed-completeness`, `feed-bootstrap-position` and
/// `feed-position-bounded` always, and `dedup-concurrent` under an
/// `Eventual` declaration — and none of them narrows anything: every entry
/// any of them stores is inside the scope it sends, so those reads buy
/// shape coverage on the path — a plugin that chokes on a compiled scope
/// there meets one — and no scope *enforcement* whatever, for the reason
/// the suite's shared single-tenant filter buys none either.
/// `feed-position-bounded` sends the widest of those scopes, a disjunction
/// naming ten tenants, and it is the widest for a reason that is not about
/// enforcement: a grant that withholds nothing is what keeps the difference
/// between two positions attributable to the subscription.
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
    CONVERGED_TARGET_LOOKUP,
    DEDUP_CONCURRENT,
    LATEST_TIE_BREAK,
    FEED_SNAPSHOT_AND_REPLAY,
    FEED_COMPLETENESS,
    FEED_BOOTSTRAP_POSITION,
    FEED_RETENTION_REFUSAL,
    FEED_POSITION_BOUNDED,
];

/// The checks [`run_all`] runs that DESIGN §3.3 does not tabulate.
///
/// A fourth constant rather than one more entry in [`IMPLEMENTED_CHECKS`],
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
/// **Empty.** Every check DESIGN §3.3 tabulates is written, named in
/// [`IMPLEMENTED_CHECKS`] and dispatched by [`run_all`]. The constant stands
/// rather than being deleted for the reason [`BLOCKED_CHECKS`] does: it is
/// one of the three the partition test holds against DESIGN's sixteen, so a
/// check the table grows has somewhere to be named on the commit that adds
/// it, before anyone writes it. The alternative to a name here is a DESIGN
/// check nothing accounts for, which is the failure the partition exists to
/// catch.
///
/// **Covered only in part is a different claim from unwritten.** Some of
/// what [`run_all`] dispatches has an assertion that needs a backend's
/// retention to have swept, and skips it; those checks are written all the
/// same, and [`run_all_with_retention`] runs them whole.
/// [`RETENTION_DRIVEN_CHECKS`] is where they are named, rather than named
/// again here: a list repeated in two places goes stale in one of them the
/// commit a check joins it, which is one commit before anything fails.
///
/// **A caller reporting coverage still reports this constant.** It says
/// today that no DESIGN check is missing from a run, which is a claim worth
/// making explicitly; a reader who infers it from the constant's absence is
/// inferring it from nothing.
// @cpt-dod:cpt-cf-usage-collector-dod-plugin-conformance-suite:p1
pub const UNWRITTEN_CHECKS: &[&str] = &[];

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
/// Both went the whole way: each is written, each is in
/// [`IMPLEMENTED_CHECKS`], and [`run_all`] dispatches both. So neither
/// entry's justification was merely false — each was false about a check
/// the suite now runs. Keeping the constant matters
/// because it is one of the three the partition test holds against
/// DESIGN's sixteen: a check a later SPI change puts out of reach needs
/// somewhere to be named, and the alternative to naming it is a check that
/// quietly stops being accounted for. See
/// `the_blocked_checks_are_the_ones_the_spi_cannot_express` for what a
/// human must establish before adding an entry, and for why no test can
/// establish it for them.
pub const BLOCKED_CHECKS: &[(&str, &str)] = &[];

/// The checks [`run_all`] covers **only in part**, because at least one
/// assertion of each needs a backend's retention to have actually swept.
///
/// How much of a check is missing differs, and the constant deliberately does
/// not say: `feed-bootstrap-position` loses one assertion of four and
/// `feed-retention-refusal` three of four. A caller reporting coverage learns
/// from this constant that a green dispatch is short of the row, and from the
/// check's own docs by how much.
///
/// No method on [`UsageCollectorPluginV1`] removes anything — DESIGN
/// declares none — so the drive sits beside the SPI as
/// [`retention::ContractRetention`], and it reaches the checks through
/// [`run_all_with_retention`] rather than through [`run_all`]. A check named
/// here runs under both entry points and asserts strictly more under the
/// second.
///
/// **It is not a fourth coverage constant** and it does not partition
/// anything: every name here is also in [`IMPLEMENTED_CHECKS`], which is
/// asserted rather than stated — a check `run_all` does not dispatch at all
/// is not a check `run_all` covers in part. What this constant adds is the
/// one thing the other four cannot say: that a check can be *listed as
/// implemented, dispatched, and green* and still have an assertion that did
/// not run. A caller reporting coverage from a [`run_all`] result has to
/// report it, for the same reason it has to report [`UNWRITTEN_CHECKS`].
///
/// The claim is held mechanically as well as written down.
/// `contract_tests`' `RETENTION_DRIVEN_MATRIX` carries one row per subject
/// whose only defect is one a driven assertion reaches, and
/// `each_driven_check_fails_against_its_own_defect_and_no_other` requires
/// every one of them to pass under [`run_all`] and fail under
/// [`run_all_with_retention`]. A check that grew a half needing no drive
/// would start failing the first half of that test; one whose drive stopped
/// reaching its defect would start passing the second. That matrix is also
/// asserted to name exactly the checks this constant names, so a check
/// listed here with no such subject fails the run rather than reading as
/// covered.
pub const RETENTION_DRIVEN_CHECKS: &[&str] = &[FEED_BOOTSTRAP_POSITION, FEED_RETENTION_REFUSAL];

/// Run every implemented check, returning one entry per violation.
///
/// An empty result means the plugin conforms as far as this suite reaches.
/// The checks are run in sequence rather than concurrently: several store
/// entries and then read them back, and interleaving them would let one
/// check observe another's rows.
///
/// # Some of the checks this runs are covered only in part
///
/// **An empty result is not a statement about every assertion of every
/// check it dispatched.** [`RETENTION_DRIVEN_CHECKS`] names the checks with
/// an assertion that needs a backend's retention to have swept, which
/// nothing on the SPI performs and this entry point therefore cannot drive.
/// Those checks run here and skip that assertion.
/// [`run_all_with_retention`] takes the drive and runs them whole.
///
/// Which checks those are is left to the constant rather than counted here,
/// for the reason every other count in this file is: a name joining it would
/// leave the sentence stale one commit before anything failed.
///
/// # The rest of the signature
///
/// The plugin arrives behind `&dyn` rather than a generic parameter. The
/// suite is dispatched once per backend and its cost is entirely in the
/// awaits, so monomorphising it buys nothing; erasing it means the host's
/// own handle — `ClientHub` hands out a `dyn UsageCollectorPluginV1` —
/// passes straight in.
///
/// `level` is the dedup level the plugin declares. The checks that read it
/// today are `at-most-one-invalidation`, whose post-convergence half runs
/// only under an `Eventual` declaration; `converged-target-lookup`, whose first
/// probe waits out the declared bound before it requires a decided answer;
/// and `dedup-concurrent`, which asserts a raced divergent pair's outcomes
/// only under `Linearizable` and the three post-bound read surfaces only
/// under `Eventual`.
pub async fn run_all(
    plugin: &dyn UsageCollectorPluginV1,
    level: DedupLevel,
) -> Vec<ContractViolation> {
    run_every_check(plugin, level, None).await
}

/// The same suite, with the checks in [`RETENTION_DRIVEN_CHECKS`] run
/// whole.
///
/// `retention` is the capability a backend under test exposes beside the
/// SPI: [`retention::ContractRetention`] runs that backend's own retention
/// sweep at a moment a check chooses, which is the one thing the seven SPI
/// methods cannot be asked for. Its module docs say why that is a
/// conforming capability rather than a test hook, and what a porter
/// implements it against.
///
/// **Prefer this entry point.** [`run_all`] keeps its signature and its
/// meaning, and a backend that cannot be driven is handed to it and reported
/// as covering less; a backend that can be driven and is handed to
/// [`run_all`] anyway is reported as covering less for no reason.
///
/// # One caution a caller has to read
///
/// A driven check may **remove** entries, which no other check in this suite
/// does, and the suite's premise that a repeated run re-delivers identical
/// entries rather than colliding with different ones rests on nothing ever
/// being removed. Each driven check is therefore responsible for putting its
/// own meter back, and
/// `the_reference_backend_conforms_to_a_repeated_run_under_a_retention_drive`
/// is what holds them to it. What "back" means is the check's own to decide:
/// `feed-bootstrap-position` re-delivers its two entries in its own order,
/// because what it asserts is which of them a read begins at, and its
/// `restore_this_checks_own_meter` says why; `feed-retention-refusal` leaves
/// its swept meter **empty**, because what it asserts is what lies after a
/// cursor it issued and the only ledger from which that is the same on every
/// run is one built in front of the cursor from nothing. Both restorations are
/// driven over a meter the suite's `check_meter` derived for one check's
/// exclusive use, and [`retention::ContractRetention::drop_before`] is keyed
/// on a GTS type, so neither can reach the other's fixtures.
pub async fn run_all_with_retention(
    plugin: &dyn UsageCollectorPluginV1,
    level: DedupLevel,
    retention: &dyn ContractRetention,
) -> Vec<ContractViolation> {
    run_every_check(plugin, level, Some(retention)).await
}

/// The suite both entry points dispatch, with the retention drive optional.
///
/// One body rather than [`run_all_with_retention`] calling [`run_all`] and
/// then running the driven checks itself. That shape would dispatch every
/// driven check **twice** — once undriven from the inner call and once
/// driven — and a check that writes fixtures and reads them back is not
/// idempotent under a second dispatch inside one run: the undriven pass of
/// `feed-bootstrap-position` would leave its meter stocked and the driven
/// pass would purge it, so the two passes would report on two different
/// ledgers and every violation would be doubled. Here each check is
/// dispatched once, and the option is the only thing that differs.
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
