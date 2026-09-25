//! Domain layer for the usage-collector module.

pub mod authz;
pub mod covered_period;
pub mod error;
pub mod invalidation;
pub mod local_client;
pub mod ports;
pub mod query;
pub mod service;
#[cfg(test)]
pub mod test_support;
pub mod type_resolver;
pub mod validation;

pub use error::DomainError;
pub use local_client::UsageCollectorLocalClient;
pub use service::Service;
