//! Tests for the contract suite itself.
//!
//! Two different things are asserted here, and neither is a plugin's
//! conformance. The first is that the suite runs and passes against a
//! backend built to conform, which is what makes a violation reported
//! against a real plugin worth reading. The second is that
//! [`BLOCKED_CHECKS`] still describes the gap it claims to.
//!
//! What this file cannot establish is that the checks *discriminate*: the
//! reference backend and the assertions were written alongside each other,
//! so a check too weak to catch a non-conforming backend passes here
//! exactly as a good one does. Closing that seam takes a deliberately
//! non-conforming subject, and it is a separate piece of work.

use std::collections::BTreeSet;

use super::{BLOCKED_CHECKS, QUANTITY_ROUND_TRIP, reference::InMemoryReferencePlugin, run_all};

#[tokio::test]
async fn the_reference_backend_conforms() {
    let plugin = InMemoryReferencePlugin::new();

    let violations = run_all(&plugin).await;

    assert!(
        violations.is_empty(),
        "the reference backend is the suite's own subject and must pass every implemented \
         check; it reported: {violations:#?}"
    );
}

/// The blocked list names checks DESIGN §3.3 actually declares, and does not
/// name one this suite implements.
///
/// Without this, `BLOCKED_CHECKS` is prose: a typo, or a row left behind
/// after a check became writable, reads exactly like an honest gap.
#[test]
fn the_blocked_checks_are_the_ones_not_implemented() {
    let blocked: BTreeSet<&str> = BLOCKED_CHECKS.iter().map(|(check, _)| *check).collect();

    assert_eq!(
        blocked,
        BTreeSet::from(["feed-snapshot-and-replay", "latest-tie-break"]),
        "the blocked set must be exactly the two DESIGN section 3.3 checks the current SPI cannot \
         express; a name that is not in DESIGN's table, or one whose check has since become \
         writable, reads as an honest gap and is not one"
    );
    assert!(
        !blocked.contains(QUANTITY_ROUND_TRIP),
        "`{QUANTITY_ROUND_TRIP}` is implemented and run by `run_all`, so listing it as blocked \
         would under-report the suite's coverage"
    );
    for (check, reason) in BLOCKED_CHECKS {
        assert!(
            !reason.trim().is_empty(),
            "`{check}` is listed as blocked with no reason: the entry exists to say what \
             unblocks it, and an empty one only hides the check"
        );
    }
}
