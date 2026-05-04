//! Domain layer for the Account Management storage floor.
//!
//! Houses the error taxonomy, metric catalog, `IdP` provisioner contract,
//! tenant domain model + repository trait, and the internal audit-event
//! shapes emitted by future AM features. Domain-service logic, bootstrap
//! saga, audit emission, and tenant-type checks arrive in later PRs.

pub mod audit;
pub mod error;
pub mod idp;
pub mod metrics;
pub mod tenant;
