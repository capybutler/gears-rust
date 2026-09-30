#![cfg(feature = "postgres")]

//! The DESIGN section 3.3 plugin contract suite, run against a live
//! `TimescaleDB` backend. This is the acceptance criterion for the port.
//!
//! **A green run here is not a conformance certificate**, and the assertion
//! message says so rather than leaving it to this comment:
//!
//! * [`contract::run_all`] runs the DESIGN checks in
//!   [`contract::IMPLEMENTED_CHECKS`] plus everything in
//!   [`contract::ADDITIONAL_CHECKS`] (today one, which DESIGN obliges
//!   without tabulating). What it does not run is in
//!   [`contract::UNWRITTEN_CHECKS`] and [`contract::BLOCKED_CHECKS`], the
//!   latter with what unblocks each. Deliberately no counts here: a check
//!   moving from blocked to implemented would leave the assertion correct
//!   and a tallied sentence stale.
//! * Nothing in the suite exercises the SPI's keyset obligations: the
//!   reference backend it was validated against serves the canonical order,
//!   ignores `query.order` and mints no `next_cursor`, and this plugin owes
//!   all three. What covers them here is `record_store_tests`'
//!   cursor-fingerprint pair over `build_list_page`
//!   (`a_page_minted_without_a_fingerprint_is_refused_rather_than_shipped`,
//!   `a_cursor_is_refused_when_the_query_carries_no_fingerprint`) and the
//!   other pg suites. Not `keyset`, which carries no tests of its own.
//! * This run is **driven**: it calls
//!   [`contract::run_all_with_retention`], not `contract::run_all`, passing a
//!   `common::SweepDrive` over this same backend. Every check in
//!   [`contract::RETENTION_DRIVEN_CHECKS`] — `feed-bootstrap-position` and
//!   `feed-retention-refusal` — therefore runs **whole** rather than skipping
//!   its `retention.is_some()` branch: `feed-bootstrap-position`'s
//!   purge-then-resume assertion and three of `feed-retention-refusal`'s four
//!   assertions run against this backend here, which no undriven run of this
//!   suite has ever done.
//!
//!   The drive is not a second deletion mechanism built for the suite. It
//!   asks this plugin's own production `PgRetentionSweeper` to sweep, under a
//!   stub `RetentionSource` that answers the one floor a check asks for
//!   (`tests/common/mod.rs`'s `SweepDrive`) — ruling D3's point, and the
//!   reason the mark a feed page then refuses a cursor against is written by
//!   exactly the interlock a deployment's own timer would produce, not by a
//!   shortcut that only resembles it. The chunk interval this container runs
//!   at is `common::CONTRACT_CHUNK_INTERVAL_SECS` rather than the 7-day
//!   default every other pg suite takes, for the reason that constant's own
//!   doc gives: only at an hour do the fixtures' chunk boundaries land where
//!   `drop_before`'s exclusive bound needs them to.
//! * A green run no longer means "everything that ran passed". It means
//!   exactly the non-conformances [`NOT_YET_CONFORMING`] declares failed,
//!   no more and no fewer — that list names what this backend cannot pass
//!   yet and the slice that closes each, and the test asserts the failing
//!   set against it exactly. The list is expected to shrink and never to
//!   grow: a check that starts passing takes its row with it, and a check
//!   that fails without a row is a regression in this plugin. An empty list
//!   is not an exception to that: it means no *dispatched, run* assertion
//!   fails today, not that every assertion DESIGN states has been run (the
//!   bullet above is where that residual gap lives).
//!
//! See `contract.rs`'s module header and DIVERGENCES section F.
//!
//! The plugin declares `linearizable` in its README, so the suite runs at that level.

use std::collections::BTreeSet;

use usage_collector_sdk::contract;

mod common;

/// The DESIGN section 3.3 checks this backend does **not** yet pass, each
/// paired with the roadmap slice that closes it.
///
/// Not a suppression list. Every row is a non-conformance this backend is
/// known to hold today, and the test below asserts the failing set is
/// exactly these — so a row that stops being true fails the run rather
/// than sitting here unnoticed.
///
/// Spelled with the exported [`contract`] constants rather than string
/// literals. A literal would go on matching a constant that had been
/// respelled, and the row would then assert nothing about the check it
/// names.
///
/// **A row may name a check `run_all` does not yet dispatch.** The
/// assertion intersects this list with what actually runs, so a row lands
/// before its check is written and activates itself when the check joins
/// `run_all`. No count of how many rows are in that state is given here,
/// for the reason the module header gives for the coverage split: it would
/// go stale one check before the assertion did.
// Empty as of slice 3. `read_feed_page` answers, so every check `run_all`
// dispatches passes what it dispatches — the module header above is where
// this run's undriven residue (two checks whose driven quarter to
// three-quarters `run_all` skips rather than fails) is recorded instead, since
// neither is a failure this list could ever have named.
const NOT_YET_CONFORMING: &[(&str, &str)] = &[];

/// Run every implemented check against the `TimescaleDB` backend, driven, and
/// hold the set that failed to [`NOT_YET_CONFORMING`] exactly.
///
/// One test rather than one per check: the checks write entries and read
/// them back, [`contract::run_all_with_retention`] runs them in sequence so
/// one cannot observe another's rows, and it returns every violation rather
/// than stopping at the first — so a single failure reports the whole run.
/// The exact set wants that same whole run: it is a comparison over every
/// check the driven entry point dispatched, not a verdict reachable one
/// check at a time.
#[tokio::test]
async fn the_timescale_backend_fails_exactly_the_declared_checks() {
    let (_harness, backend, drive) = common::start_backend_with_retention_drive().await;

    let violations =
        contract::run_all_with_retention(&backend, contract::DedupLevel::Linearizable, &drive)
            .await;

    let failed: BTreeSet<&str> = violations.iter().map(|v| v.check).collect();

    // What `run_all` dispatches today. `NOT_YET_CONFORMING` is intersected
    // with it before the comparison: a row naming a check still in
    // `contract::UNWRITTEN_CHECKS` names one `run_all` never runs, and a
    // check that never runs cannot fail, so it must not be expected to.
    // That intersection is what let the whole list land before any of the
    // checks it names existed — without it the assertion would have been
    // wrong at every commit until the last one. It is inert now that
    // `contract::UNWRITTEN_CHECKS` is empty, and it is kept rather than
    // simplified away: it costs nothing and it re-arms the moment DESIGN
    // §3.3's table grows a row the suite has not written yet.
    let running: BTreeSet<&str> = contract::IMPLEMENTED_CHECKS
        .iter()
        .chain(contract::ADDITIONAL_CHECKS)
        .copied()
        .collect();

    let expected: BTreeSet<&str> = NOT_YET_CONFORMING
        .iter()
        .map(|(check, _)| *check)
        .filter(|check| running.contains(check))
        .collect();

    // Names only. `BLOCKED_CHECKS` pairs each name with its blocker, and
    // rendering the pairs puts ~700 characters between the coverage summary
    // and the violation list a reader came here for. The blockers are one
    // `contract::BLOCKED_CHECKS` away and do not change per run.
    let blocked: Vec<&str> = contract::BLOCKED_CHECKS
        .iter()
        .map(|(name, _)| *name)
        .collect();

    // Rendered as pairs, unlike `blocked`, and placed ahead of the coverage
    // summary so it does not come between that summary and the violations.
    // The slice is the whole point of a row: a reader who sees a name in
    // the set difference needs it without opening this file.
    let pending = NOT_YET_CONFORMING
        .iter()
        .fold(String::new(), |mut out, (check, closes)| {
            out.push_str("\n  ");
            out.push_str(check);
            out.push_str(": ");
            out.push_str(closes);
            out
        });

    assert_eq!(
        failed,
        expected,
        "the failing checks are not the declared ones. Left is what failed; \
         right is `NOT_YET_CONFORMING` narrowed to the checks \
         `contract::run_all` dispatches today.\n\
         In the left set only: the check failed and no row claims it. That \
         is a regression in this plugin, not a row to add.\n\
         In the right set only: the check now passes, so its row has been \
         paid off and must be deleted from `NOT_YET_CONFORMING`. This test \
         failing is how anyone finds that out.\n\
         An entry whose check is `{}` is the suite's own failure, not this \
         plugin's.\n\
         declared not yet conforming, and the slice that closes each:\
         {pending}\n\
         ran: {:?} plus {:?}\n\
         not run: {:?} unwritten, {blocked:?} blocked \
         (`contract::BLOCKED_CHECKS` carries what unblocks each)\n\
         {violations:#?}",
        contract::HARNESS_FAULT,
        contract::IMPLEMENTED_CHECKS,
        contract::ADDITIONAL_CHECKS,
        contract::UNWRITTEN_CHECKS,
    );
}
