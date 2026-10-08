#![cfg(feature = "postgres")]

//! The DESIGN section 3.3 plugin contract suite, run against a live
//! `TimescaleDB` backend. This is the acceptance criterion for the port.
//!
//! **A green run here is not a conformance certificate**:
//!
//! * The suite's checks are named by [`contract::IMPLEMENTED_CHECKS`] plus
//!   everything in [`contract::ADDITIONAL_CHECKS`], each a check DESIGN
//!   obliges without tabulating — the set **either** entry point,
//!   [`contract::run_all`] or [`contract::run_all_with_retention`], dispatches;
//!   the two differ only in whether the driven quarter to three-quarters of
//!   [`contract::RETENTION_DRIVEN_CHECKS`] runs (bullet below). What **neither**
//!   runs is in [`contract::UNWRITTEN_CHECKS`] and [`contract::BLOCKED_CHECKS`],
//!   the latter with what unblocks each. Deliberately no counts here: a check
//!   moving from blocked to implemented would leave the assertion correct and
//!   a tallied sentence stale.
//! * The suite now exercises the SPI's keyset obligations directly:
//!   `contract::RAW_PAGE_KEYSET_WALK` and `contract::RAW_PAGE_CALLER_ORDER`
//!   dispatch `list_usage_records` with a real continuation, including a
//!   caller-supplied order (ascending and descending alike) distinct from
//!   the canonical `(window_end, id)` pair, and inspect the returned
//!   [`usage_collector_sdk::keyset::Keyset`] itself rather than a wire
//!   token. Both run against this backend on every dispatch of this suite,
//!   driven or not. This closed a real gap: before them, the reference
//!   backend they were validated against served only the canonical order
//!   and answered `next: None` on every call, so no check could exercise
//!   either obligation on any backend — the reference now honours
//!   `query.order` and returns a real `Keyset` too. `record_store_tests`'
//!   keyset pair over `build_list_page`
//!   (`a_look_ahead_page_returns_the_last_rows_keyset_for_the_gateway_to_mint_from`,
//!   `a_keyset_that_will_not_fit_the_wire_cursor_is_refused_rather_than_built`,
//!   `a_mixed_direction_order_refuses_the_keyset_rather_than_pick_the_leading_key`)
//!   and `records_query_integration_pg` still cover what these two do not —
//!   both drive `PgRecordStore::list` directly, **below** the SPI trait
//!   this driven run dispatches through, so they exercise the store's own
//!   API surface rather than the `StorageAdapter` wrapper. They are also
//!   wider on specifics no fixture here constructs:
//!   `records_query_integration_pg`'s `every_keyset_safe_field_is_an_admissible_order`
//!   walks every one of `KEYSET_SAFE_RECORD_FIELDS`, not the two orders
//!   `RAW_PAGE_KEYSET_WALK` and `RAW_PAGE_CALLER_ORDER` happen to dispatch,
//!   and its own doc names a page boundary falling between an invalidation
//!   and its target, a case this driven run's fixtures do not build. (Since
//!   slice 7 task 2, the gateway mints and decodes the wire cursor; this
//!   plugin's own obligation is the typed `Keyset` `build_list_page`
//!   returns, and `query.cursor` is no longer a parameter it ever reads.)
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
//!
//! See `contract.rs`'s module header.
//!
//! The plugin declares `linearizable` in its README, so the suite runs at that level.

use usage_collector_sdk::contract;

mod common;

/// Run every implemented check against the `TimescaleDB` backend, driven, and
/// require the driven contract suite to report no violations.
#[tokio::test]
async fn the_timescale_backend_passes_the_contract_suite() {
    let (_harness, backend, drive) = common::start_backend_with_retention_drive().await;

    let violations =
        contract::run_all_with_retention(&backend, contract::DedupLevel::Linearizable, &drive)
            .await;

    assert!(
        violations.is_empty(),
        "TimescaleDB backend failed the driven contract suite:\n{violations:#?}"
    );
}
