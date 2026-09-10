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
//!   `a_cursor_is_refused_when_the_query_carries_no_fingerprint`) and, once
//!   Task 15 rewrites them, the other pg suites — which today do not
//!   compile, so they cover nothing yet. Not `keyset`, which carries no
//!   tests of its own.
//!
//! See `contract.rs`'s module header and DIVERGENCES section F.

use usage_collector_sdk::contract;

mod common;

/// Run every implemented check against the `TimescaleDB` backend.
///
/// One test rather than one per check: the checks write entries and read
/// them back, [`contract::run_all`] runs them in sequence so one cannot
/// observe another's rows, and it returns every violation rather than
/// stopping at the first — so a single failure reports the whole run.
#[tokio::test]
async fn the_timescale_backend_conforms() {
    let (_harness, backend) = common::start_backend().await;

    let violations = contract::run_all(&backend).await;

    // Names only. `BLOCKED_CHECKS` pairs each name with its blocker, and
    // rendering the pairs puts ~700 characters between the coverage summary
    // and the violation list a reader came here for. The blockers are one
    // `contract::BLOCKED_CHECKS` away and do not change per run.
    let blocked: Vec<&str> = contract::BLOCKED_CHECKS
        .iter()
        .map(|(name, _)| *name)
        .collect();

    assert!(
        violations.is_empty(),
        "plugin contract violations (an entry whose check is `{}` is the \
         suite's own failure, not this plugin's).\n\
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
