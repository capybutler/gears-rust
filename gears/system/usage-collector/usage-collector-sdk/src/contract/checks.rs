//! One module per check.
//!
//! Each check owns its window offsets, its page limit, its fixtures and its
//! readers, and shares only [`super::fixtures`] with the others. The entry
//! point of each is re-exported below under the check's own name, so
//! [`super::run_all`] reads as the list of checks it runs.
//!
//! All but one are named by DESIGN §3.3;
//! [`scope_is_a_filter_on_every_read_path`](scope_is_a_filter_on_every_read_path())
//! is not, and
//! [`super::SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH`] says why.

mod at_most_one_invalidation;
mod dedup_identity_over_window;
mod invalidation_excluded_from_fold;
mod quantity_round_trip;
mod scope_is_a_filter_on_every_read_path;
mod window_end_selection;

pub use at_most_one_invalidation::at_most_one_invalidation;
pub use dedup_identity_over_window::dedup_identity_over_window;
pub use invalidation_excluded_from_fold::invalidation_excluded_from_fold;
pub use quantity_round_trip::quantity_round_trip;
pub use scope_is_a_filter_on_every_read_path::scope_is_a_filter_on_every_read_path;
pub use window_end_selection::window_end_selection;
