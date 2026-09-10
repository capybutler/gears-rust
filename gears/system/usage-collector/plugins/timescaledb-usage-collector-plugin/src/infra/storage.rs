pub mod entity;
pub mod error;
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
