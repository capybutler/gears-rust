//! Infrastructure adapters for the usage-collector module.
//!
//! Houses the host-only REST error-envelope lift. The SDK crate stays
//! `toolkit-canonical-errors`-free; the RFC-9457 `Problem` envelope is
//! produced exclusively in this host crate.

pub mod metrics;
pub mod storage;
// The §3.11.5 conformance pin's transcribed data and its own tests — nothing
// outside this crate's unit-test build consumes it (unlike the plugin's
// directly analogous `declared_instrument_names`, which stays available
// under `feature = "postgres"` because external `tests/*.rs` integration
// binaries call it; this gear has no such crate to serve), so there is no
// reason to ship it.
#[cfg(test)]
pub mod metrics_inventory;
pub mod sdk_error_mapping;
pub mod types_registry_source;
