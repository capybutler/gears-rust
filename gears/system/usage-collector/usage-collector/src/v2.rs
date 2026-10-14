//! Generation 2 of the usage-collector gateway — the reworked metering
//! surface, served by the same gear as generation 1.
//!
//! Configuration is **not** here: both generations share one flat
//! [`crate::config::UsageCollectorConfig`], because generation 2's
//! configuration is a strict superset of generation 1's.
//!
//! `gts` and `module` are gated behind the crate's `v2` feature — the former
//! because it submits link-time GTS content a generation-1 deployment should
//! not publish, the latter because it is what joins this surface to the gear's
//! bootstrap. Everything else compiles unconditionally so a default-feature
//! `cargo nextest run` covers it.

pub mod api;
pub mod domain;
pub mod infra;

#[cfg(feature = "v2")]
mod gts;

#[cfg(feature = "v2")]
pub mod module;
