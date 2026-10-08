//! The `TimescaleDB` storage backend: the ledger, its indexes, the two keyed
//! tables beside it, and the query builders that read them.
//!
//! Every structure here keys on the meter's `types-registry` Registry
//! Reference. The GTS identifier is diagnostic on
//! [`MeterRef`](usage_collector_sdk::MeterRef) and is not persisted, which is
//! what the Plugin SPI requires of a storage backend.

pub mod entity;
pub mod error;
pub mod feed_horizon;
pub mod mapper;
// Test-only support: the shared parse of `migrations/0001_init.sql` that every
// column-sequence constant in this crate is checked against. Gated on the
// `postgres` feature too — itself test-only — so the `tests/*.rs` integration
// crates can reach the same parse instead of writing a second one.
#[cfg(any(test, feature = "postgres"))]
#[cfg_attr(coverage_nightly, coverage(off))]
pub mod migration_probe;
pub mod pool;
pub mod query;
pub mod record_store;
pub mod retention_sweep;
pub mod rollup_maintenance;
pub mod type_key;
