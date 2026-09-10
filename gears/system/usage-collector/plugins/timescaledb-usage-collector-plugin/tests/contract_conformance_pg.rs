#![cfg(feature = "postgres")]

//! The DESIGN section 3.3 plugin contract suite, run against a live
//! `TimescaleDB` backend. This is the acceptance criterion for the port.
//!
//! **A green run here is not a conformance certificate**, and the assertion
//! message says so rather than leaving it to this comment:
//!
//! * [`contract::run_all`] runs the five DESIGN checks in
//!   [`contract::IMPLEMENTED_CHECKS`] plus [`contract::ADDITIONAL_CHECKS`]'s
//!   `scope-is-a-filter-on-every-read-path`, which DESIGN obliges without
//!   tabulating. The other two of DESIGN's seven are in
//!   [`contract::BLOCKED_CHECKS`], each with what unblocks it.
//! * Nothing in the suite exercises the SPI's keyset obligations: the
//!   reference backend it was validated against serves the canonical order,
//!   ignores `query.order` and mints no `next_cursor`, and this plugin owes
//!   all three. `keyset`'s cursor-fingerprint unit tests and the other pg
//!   suites are what cover them here.
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

    assert!(
        violations.is_empty(),
        "plugin contract violations.\n\
         ran: {:?} plus {:?}\n\
         not run: {:?} unwritten, {:?} blocked\n\
         {violations:#?}",
        contract::IMPLEMENTED_CHECKS,
        contract::ADDITIONAL_CHECKS,
        contract::UNWRITTEN_CHECKS,
        contract::BLOCKED_CHECKS,
    );
}
