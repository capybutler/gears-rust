//! Domain layer for the usage-collector module.

pub(crate) mod aggregate_query;
pub mod authz;
pub mod covered_period;
pub mod error;
pub mod feed;
pub(crate) mod fingerprint;
pub(crate) mod ingestion;
pub mod invalidation;
pub(crate) mod list_query;
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

pub use error::DomainError;
pub use local_client::UsageCollectorLocalClient;
pub use service::{Service, ServiceConfig};
