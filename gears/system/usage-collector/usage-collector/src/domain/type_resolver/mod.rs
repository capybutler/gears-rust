//! Type Resolver — resolves a meter's declaration from `types-registry`.
//!
//! Resolution sits on the ingestion hot path, and `types-registry` publishes
//! no latency obligation of its own, so a per-entry registry call would make
//! this gear's ingestion NFRs contingent on a second gear. A local cache of
//! resolved declarations (Task 6) keeps those obligations self-contained.
//!
//! This module currently holds the parsing and validation halves —
//! [`ResolvedDeclaration`] and the [`CompiledMetadataSchema`] key extraction
//! and per-entry validation it depends on. The cache and the
//! `DeclarationSource`-backed resolver service arrive in later tasks.

mod declaration;
mod metadata;

pub use declaration::ResolvedDeclaration;
pub use metadata::CompiledMetadataSchema;
