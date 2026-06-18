//! Injection-safe `OData` → SQL translation foundation.
//!
//! Pure (no DB) logic that turns a validated `toolkit_odata` filter AST into a
//! parameterized `PostgreSQL` `WHERE` fragment plus an ordered list of binds.
//! Every SQL identifier is drawn from a closed allowlist
//! ([`translate::record_column`] / [`translate::usage_type_column`]); every
//! value is bound (`$N`), never interpolated.

pub mod aggregate;
pub mod bind;
pub mod keyset;
pub mod translate;
