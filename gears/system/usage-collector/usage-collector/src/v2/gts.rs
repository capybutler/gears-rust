//! Generation 2's link-time GTS content.
//!
//! Everything declared here reaches `types-registry` automatically through the
//! process-wide `toolkit-gts` inventory — no registration code in the gear's
//! `init` path is needed. One file per content kind keeps this directory
//! navigable (permissions and the reserved base type today).
//!
//! That inventory is process-wide and generation 1's `crate::gts` submits
//! into the same one, which is why this whole subtree sits behind the crate's
//! `v2` feature: a generation-1 deployment must publish nothing it does not
//! serve. Every identifier below carries a `.v2` tail for the same reason —
//! admission would fail on a collision with generation 1's `.v1` catalog.

mod permissions;
mod usage_record;
