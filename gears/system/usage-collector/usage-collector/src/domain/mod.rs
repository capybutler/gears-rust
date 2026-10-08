//! Domain layer for the usage-collector module.

pub mod authz;
pub mod covered_period;
pub mod error;
pub mod feed;
pub(crate) mod fingerprint;
pub mod invalidation;
pub mod local_client;
pub mod meter_reverse;
pub mod observability;
pub mod ports;
pub mod query;
pub mod quota;
pub mod reconciliation;
pub mod service;
#[cfg(test)]
pub mod test_support;
pub mod type_resolver;
pub mod validation;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "data_classification_tests.rs"]
mod data_classification_tests;

pub use error::DomainError;
pub use local_client::UsageCollectorLocalClient;
pub use service::Service;
