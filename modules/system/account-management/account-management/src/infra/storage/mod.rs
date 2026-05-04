//! Storage layer — `SeaORM` entities and repository implementations for
//! Account Management tables.
//!
//! Exposes `entity` (column-for-column entities), `migrations` (only
//! `m0001_initial_schema` is wired today; later migrations land in
//! follow-up PRs), and `repo_impl` (the SeaORM-backed `TenantRepo`
//! implementation). The audit classifier set (`audit/`) arrives in a
//! later PR.

pub mod entity;
pub mod migrations;
pub mod repo_impl;
