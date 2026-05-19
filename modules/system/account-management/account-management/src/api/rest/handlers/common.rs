//! Cross-handler helpers shared by the AM REST handler families.

use modkit_odata::ODataQuery;

/// Clamp the `OData` `$top` against the per-endpoint deployment cap.
/// Repos already enforce an absolute ceiling (200), but a deployment
/// that has dropped `listing.max_top` below it would otherwise be
/// bypassed — clamp here so the service signature stays a thin
/// `(scope, target, &ODataQuery)` forward.
pub(super) fn clamp_listing_top(mut query: ODataQuery, max_top: u32) -> ODataQuery {
    let cap = u64::from(max_top);
    query.limit = Some(query.limit.map_or(cap, |requested| requested.min(cap)));
    query
}

#[cfg(test)]
#[path = "common_tests.rs"]
mod tests;
