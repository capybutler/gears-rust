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
//! * A green run no longer means "everything that ran passed". It means
//!   exactly the non-conformances [`NOT_YET_CONFORMING`] declares failed,
//!   no more and no fewer — that list names what this backend cannot pass
//!   yet and the slice that closes each, and the test asserts the failing
//!   set against it exactly. The list is expected to shrink and never to
//!   grow: a check that starts passing takes its row with it, and a check
//!   that fails without a row is a regression in this plugin.
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
const NOT_YET_CONFORMING: &[(&str, &str)] = &[
    (
        contract::FEED_SNAPSHOT_AND_REPLAY,
        "slice 3, plugin feed page and retention interlock: `read_feed_page` \
         is stubbed `Internal` in `src/domain/adapter.rs`, so no paginated \
         scan can be observed at all",
    ),
    (
        contract::FEED_COMPLETENESS,
        "slice 3, plugin feed page and retention interlock: the feed order \
         this check holds invariant does not exist either. \
         `usage_acceptance_sequence` keys its counter on \
         (tenant_id, gts_type_id) and so orders nothing across a \
         subscription; slice 2 replaces it with an `xid8` feed-order column",
    ),
    (
        contract::FEED_BOOTSTRAP_POSITION,
        "slice 3, plugin feed page and retention interlock: `FeedStart::Oldest` \
         needs a position to answer with, and this backend issues none",
    ),
    (
        contract::FEED_RETENTION_REFUSAL,
        "slice 3, plugin feed page and retention interlock: the refusal is \
         decided against `usage_feed_retention_marks`, a table slice 2 adds \
         and slice 3 reads. Neither exists yet",
    ),
    (
        contract::FEED_POSITION_BOUNDED,
        "slice 3, plugin feed page and retention interlock: this backend \
         issues no `FeedPosition` at all, so nothing here holds to a size \
         bound. What one would encode is the `xid8` feed-order column slice \
         2 adds, which does not grow with a subscription's breadth",
    ),
    (
        contract::LATEST_TIE_BREAK,
        "slice 7, query rules: `LATEST_SELECT_EXPR` in \
         `src/infra/storage/query/aggregate.rs` orders by \
         `window_end DESC, acceptance_sequence DESC`, and DESIGN section 3.1 \
         fixes the order as greatest `window_end`, then greatest \
         `accepted_at`, then greatest `id` in byte order. \
         `acceptance_sequence` is monotonic per (tenant_id, gts_type_id) only",
    ),
];

/// Run every implemented check against the `TimescaleDB` backend, and hold
/// the set that failed to [`NOT_YET_CONFORMING`] exactly.
///
/// One test rather than one per check: the checks write entries and read
/// them back, [`contract::run_all`] runs them in sequence so one cannot
/// observe another's rows, and it returns every violation rather than
/// stopping at the first — so a single failure reports the whole run. The
/// exact set wants that same whole run: it is a comparison over every
/// check `run_all` dispatched, not a verdict reachable one check at a time.
#[tokio::test]
async fn the_timescale_backend_fails_exactly_the_declared_checks() {
    let (_harness, backend) = common::start_backend().await;

    let violations = contract::run_all(&backend, contract::DedupLevel::Linearizable).await;

    let failed: BTreeSet<&str> = violations.iter().map(|v| v.check).collect();

    // What `run_all` dispatches today. `NOT_YET_CONFORMING` is intersected
    // with it before the comparison: a row naming a check still in
    // `contract::UNWRITTEN_CHECKS` names one `run_all` never runs, and a
    // check that never runs cannot fail, so it must not be expected to.
    // That intersection is what lets the whole list land before any of the
    // checks it names exists — without it the assertion would be wrong at
    // every commit until the last one.
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
