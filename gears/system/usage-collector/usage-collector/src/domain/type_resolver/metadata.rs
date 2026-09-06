//! The closed metadata surface a meter declares.
//!
//! The base type admits any key with a string value. A derived meter closes
//! the set by declaring its properties with `additionalProperties: false`, and
//! the two constraints intersect to declared-keys-only. Compiling the
//! subschema once per declaration keeps the per-entry cost to a validation
//! pass rather than a parse plus a compile.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use types_registry_sdk::GtsTypeSchema;
use usage_collector_sdk::MeterTypeId;

use crate::domain::error::DomainError;

/// A meter's `metadata` subschema, compiled for repeated validation.
///
/// Declared property names are the closed metadata surface for the meter:
/// an entry carrying any other key is rejected ([`Self::validate`]), and
/// every declared key is groupable and equality-filterable on both read
/// paths (DESIGN §3.1).
pub struct CompiledMetadataSchema {
    validator: jsonschema::Validator,
    declared_keys: BTreeSet<String>,
}

impl std::fmt::Debug for CompiledMetadataSchema {
    /// `jsonschema::Validator` is not `Debug`, and the compiled program is not
    /// useful in a log line anyway. The declared surface is.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledMetadataSchema")
            .field("declared_keys", &self.declared_keys)
            .finish_non_exhaustive()
    }
}

impl CompiledMetadataSchema {
    /// Compiles the `metadata` property merged across the schema chain.
    ///
    /// A meter that declares no `metadata` property gets an empty closed
    /// surface rather than an open one. Defaulting to open would reopen the
    /// shape the base type only half-closes, and an undeclared key would then
    /// reach storage.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] when the subschema is not a valid JSON Schema.
    pub fn compile(id: &MeterTypeId, schema: &GtsTypeSchema) -> Result<Self, DomainError> {
        let merged = schema.effective_properties();

        let subschema: Value = match merged.get("metadata") {
            Some(v) => v.clone(),
            None => serde_json::json!({
                "type": "object",
                "additionalProperties": false
            }),
        };

        let declared_keys = subschema
            .get("properties")
            .and_then(Value::as_object)
            .map(|props| props.keys().cloned().collect())
            .unwrap_or_default();

        let validator = jsonschema::validator_for(&subschema).map_err(|e| {
            DomainError::declaration_incomplete(
                id,
                &format!("declares a metadata schema that does not compile: {e}"),
            )
        })?;

        Ok(Self {
            validator,
            declared_keys,
        })
    }

    /// The declared property names.
    ///
    /// Declared equals queryable: every one of these is groupable and
    /// equality-filterable on both read paths, recomputed per request.
    #[must_use]
    pub fn declared_keys(&self) -> &BTreeSet<String> {
        &self.declared_keys
    }

    /// Validates an entry's metadata against the declared surface.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] naming every violation, so a caller correcting
    /// a payload sees all of them at once rather than one per round-trip.
    pub fn validate(&self, metadata: &BTreeMap<String, String>) -> Result<(), DomainError> {
        let instance = serde_json::to_value(metadata)
            .map_err(|e| DomainError::Internal(format!("metadata is not serializable: {e}")))?;

        let violations: Vec<String> = self
            .validator
            .iter_errors(&instance)
            .map(|e| e.to_string())
            .collect();

        if violations.is_empty() {
            return Ok(());
        }

        Err(DomainError::invalid_metadata(violations.join("; ")))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "metadata_tests.rs"]
mod metadata_tests;
