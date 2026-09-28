//! One module per check.
//!
//! Each check owns its window offsets, its page limit and its fixtures, and
//! shares [`super::fixtures`] with the others. The checks that page the feed
//! share [`super::feed_walk`] as well: a walk re-implemented per check is
//! one chance per check for two of them to disagree about what reaching the
//! head means. The entry point of each is re-exported below under the
//! check's own name, so [`super::run_all`] reads as the list of checks it
//! runs.
//!
//! All but one are named by DESIGN §3.3;
//! [`scope_is_a_filter_on_every_read_path`](scope_is_a_filter_on_every_read_path())
//! is not, and
//! [`super::SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH`] says why.

mod at_most_one_invalidation;
mod converged_target_lookup;
mod dedup_concurrent;
mod dedup_floor;
mod dedup_identity_over_window;
mod feed_bootstrap_position;
mod feed_completeness;
mod feed_retention_refusal;
mod feed_snapshot_and_replay;
mod invalidation_excluded_from_fold;
mod latest_tie_break;
mod quantity_round_trip;
mod record_and_invalidation_distinct_identity;
mod scope_is_a_filter_on_every_read_path;
mod server_field_round_trip;
mod window_end_selection;

pub use at_most_one_invalidation::at_most_one_invalidation;
pub use converged_target_lookup::converged_target_lookup;
pub use dedup_concurrent::dedup_concurrent;
pub use dedup_floor::dedup_floor;
pub use dedup_identity_over_window::dedup_identity_over_window;
pub use feed_bootstrap_position::feed_bootstrap_position;
pub use feed_completeness::feed_completeness;
pub use feed_retention_refusal::feed_retention_refusal;
pub use feed_snapshot_and_replay::feed_snapshot_and_replay;
pub use invalidation_excluded_from_fold::invalidation_excluded_from_fold;
pub use latest_tie_break::latest_tie_break;
pub use quantity_round_trip::quantity_round_trip;
pub use record_and_invalidation_distinct_identity::record_and_invalidation_distinct_identity;
pub use scope_is_a_filter_on_every_read_path::scope_is_a_filter_on_every_read_path;
pub use server_field_round_trip::server_field_round_trip;
pub use window_end_selection::window_end_selection;
