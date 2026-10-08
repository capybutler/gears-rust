//! Infrastructure adapters for the usage-collector module.
//!
//! Houses the host-only REST error-envelope lift. The SDK crate stays
//! `toolkit-canonical-errors`-free; the RFC-9457 `Problem` envelope is
//! produced exclusively in this host crate.

pub mod declaration_mirror;
pub mod metrics;
// The §3.11.5 conformance pin's transcribed data and its own tests — nothing
// outside this crate's unit-test build consumes it (unlike the plugin's
// directly analogous `declared_instrument_names`, which stays available
// under `feature = "postgres"` because external `tests/*.rs` integration
// binaries call it; this gear has no such crate to serve), so there is no
// reason to ship it.
#[cfg(test)]
pub mod metrics_inventory;
pub mod sdk_error_mapping;
// The subtree traceability oracle and its three excuse-list constants —
// nothing outside this crate's unit-test build consumes it, the same rule
// `metrics_inventory` above states for itself, and it is a heavier reason
// to apply here: this walks the filesystem from a build-machine-absolute
// `env!("CARGO_MANIFEST_DIR")` path and carries ~40 KB of excuse-list data,
// neither of which belongs in the shipped library.
#[cfg(test)]
pub mod traceability;
pub mod types_registry_source;
