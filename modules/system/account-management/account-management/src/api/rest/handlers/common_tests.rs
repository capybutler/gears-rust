//! Unit tests for [`super::clamp_listing_top`]. Live alongside the
//! helper so a future change to the clamp policy cannot drift
//! silently — every AM listing handler clamps `$top` through the
//! same seam.

use super::clamp_listing_top;
use modkit_odata::ODataQuery;

#[test]
fn clamp_listing_top_defaults_unset_limit_to_operator_cap() {
    // A caller that omits `$top` should inherit the operator-tuned
    // cap rather than the repo-level absolute ceiling. Without this,
    // a deployment with `listing.max_top = 25` would still issue an
    // unbounded query to the repo and rely on the repo-level
    // `*_LISTING_LIMIT_CFG.max = 200` -- bypassing the per-deployment
    // policy.
    let query = ODataQuery::new();
    let clamped = clamp_listing_top(query, 25);
    assert_eq!(clamped.limit, Some(25));
}

#[test]
fn clamp_listing_top_caps_oversized_caller_limit_to_operator_cap() {
    let query = ODataQuery::new().with_limit(500);
    let clamped = clamp_listing_top(query, 25);
    assert_eq!(clamped.limit, Some(25));
}

#[test]
fn clamp_listing_top_preserves_smaller_caller_limit() {
    // A caller-supplied `$top` BELOW the cap is preserved verbatim --
    // the clamp is an upper bound, not a forced default.
    let query = ODataQuery::new().with_limit(10);
    let clamped = clamp_listing_top(query, 25);
    assert_eq!(clamped.limit, Some(10));
}

#[test]
fn clamp_listing_top_with_max_cap_allows_repo_absolute_ceiling() {
    // When the operator cap matches the repo's absolute ceiling
    // (`*_LISTING_LIMIT_CFG.max = 200`) the clamp degenerates into a
    // no-op for in-range caller values -- preserve the documented
    // default behaviour.
    let query = ODataQuery::new().with_limit(50);
    let clamped = clamp_listing_top(query, 200);
    assert_eq!(clamped.limit, Some(50));
}
