//! [`RetentionSource`] over `types-registry`.
//!
//! The plugin reads a type's declared `retention` from the registry itself; no
//! SPI method carries it (the gear's ADR-0015 statement 6). It is read on every
//! sweep, never cached, because retention is not on the declaration's
//! immutable list (ADR-0008 statement 3).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use toolkit::client_hub::ClientHub;
use toolkit_utils::iso8601_duration::Iso8601Duration;
use types_registry_sdk::{TypesRegistryClient, TypesRegistryError};

use crate::domain::ports::{RetentionError, RetentionSource};

/// Resolves retention through the `TypesRegistryClient` in `ClientHub`.
pub struct TypesRegistryRetentionSource {
    hub: Arc<ClientHub>,
}

impl TypesRegistryRetentionSource {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>) -> Self {
        Self { hub }
    }
}

#[async_trait]
impl RetentionSource for TypesRegistryRetentionSource {
    async fn retention(&self, gts_type_id: &str) -> Result<Duration, RetentionError> {
        // Resolved per call rather than captured at construction: the registry
        // client may register after this gear's `init`.
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| RetentionError::Unavailable(e.to_string()))?;
        let schema = registry
            .get_type_schema(gts_type_id)
            .await
            .map_err(|e| classify(TypesRegistryError::from(e)))?;
        retention_from_traits(&schema.effective_traits())
    }
}

/// A definite not-found keeps its meaning; every other failure is an
/// availability problem, which the sweep treats the same way — it keeps data.
fn classify(err: TypesRegistryError) -> RetentionError {
    match err {
        TypesRegistryError::NotFound { .. } => RetentionError::NotFound,
        other => RetentionError::Unavailable(other.to_string()),
    }
}

/// The declared `retention` out of a type's merged traits.
///
/// # Errors
///
/// [`RetentionError::MissingTrait`] when the trait is absent, and
/// [`RetentionError::InvalidTrait`] when it is not a string, not a
/// fixed-length ISO 8601 duration (years and months are rejected by
/// [`Iso8601Duration`]), or zero.
pub fn retention_from_traits(traits: &Value) -> Result<Duration, RetentionError> {
    let raw = traits
        .get("retention")
        .ok_or(RetentionError::MissingTrait)?;
    let text = raw
        .as_str()
        .ok_or_else(|| RetentionError::InvalidTrait(format!("retention is not a string: {raw}")))?;
    let parsed: Iso8601Duration = text
        .parse()
        .map_err(|e| RetentionError::InvalidTrait(format!("retention `{text}`: {e}")))?;
    let duration = parsed.as_duration();
    if duration.is_zero() {
        return Err(RetentionError::InvalidTrait(format!(
            "retention `{text}` is zero"
        )));
    }
    Ok(duration)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "registry_retention_tests.rs"]
mod registry_retention_tests;
