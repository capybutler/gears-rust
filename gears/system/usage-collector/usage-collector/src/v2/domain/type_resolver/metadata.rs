//! The closed metadata surface a meter declares.
//!
//! The base type admits any key with a string value. A derived meter closes
//! the set by declaring its properties with `additionalProperties: false`, and
//! the two constraints intersect to declared-keys-only. Compiling the
//! subschema once per declaration keeps the per-entry cost to a validation
//! pass rather than a parse plus a compile.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use toolkit_macros::domain_model;
use types_registry_sdk::GtsTypeSchema;
use usage_collector_sdk::v2::MeterTypeId;

use crate::v2::domain::error::DomainError;

/// One way a submitted `metadata` map departs from the declared surface.
///
/// Kept as a typed value rather than a rendered string so the caller can
/// classify the set — [`crate::v2::domain::validation`] holds the
/// `ResolvedDeclaration`, and therefore the meter id, and picks the wire
/// reason code from the *kinds* present. `CompiledMetadataSchema` itself has
/// no usage-type identity in scope and deliberately does not choose.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataViolation {
    /// `key` is not a member of the declared closed surface.
    UndeclaredKey {
        /// The offending key, by name.
        key: String,
    },
    /// A *declared* key's value failed a per-value constraint the subschema
    /// names (`minLength`, `pattern`, …). `message` is `jsonschema`'s own
    /// diagnostic; it does not echo the instance path, so it may not name
    /// the key.
    ValueConstraint {
        /// The validator's rendered diagnostic.
        message: String,
    },
}

impl std::fmt::Display for MetadataViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UndeclaredKey { key } => write!(
                f,
                "additional property '{key}' is not allowed: not a declared metadata key"
            ),
            Self::ValueConstraint { message } => f.write_str(message),
        }
    }
}

/// A meter's `metadata` subschema, compiled for repeated validation.
///
/// Declared property names are the closed metadata surface for the meter:
/// an entry carrying any other key is rejected ([`Self::validate`]), and
/// every declared key is groupable and equality-filterable on both read
/// paths (DESIGN §3.1).
#[domain_model]
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
    /// **Closure is enforced in code, not delegated to the subschema.**
    /// `GtsTypeSchema::effective_properties` resolves a key by *override*,
    /// not by intersection, and the base type always declares an open
    /// `metadata` (`additionalProperties: {"type": "string"}`), so a meter
    /// supplying no closing override inherits that open definition verbatim
    /// and the key is never absent.
    ///
    /// So [`Self::validate`] checks `metadata.keys() ⊆ declared_keys()`
    /// itself, and the invariant holds whether or not a schema author
    /// remembered `additionalProperties: false`. The compiled validator still
    /// enforces per-value constraints such as `minLength`.
    ///
    /// A meter declaring no `metadata` properties therefore admits no keys at
    /// all — fail-closed and actionable, since the author sees a rejection
    /// naming the key rather than an open extension surface.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] when the subschema is not a valid JSON Schema.
    pub fn compile(id: &MeterTypeId, schema: &GtsTypeSchema) -> Result<Self, DomainError> {
        let merged = schema.effective_properties();

        // A defensive fallback, not the closure mechanism: every real
        // chain resolves `metadata` to the base's open definition, so this
        // branch does not fire against a real declaration, and `validate`
        // enforces closure either way.
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
    /// Closure is checked here directly (`metadata.keys() ⊆
    /// declared_keys()`) rather than trusted to the compiled schema's own
    /// `additionalProperties` keyword — see [`Self::compile`] for why. Only
    /// the declared keys' values go through the compiled validator, for
    /// per-value constraints; an undeclared key is reported by name here and
    /// excluded from that pass,
    /// so it cannot also be masked or double-reported by whatever the
    /// subschema's own `additionalProperties` keyword happens to say.
    ///
    /// Returns EVERY violation — every undeclared key and every
    /// declared-constraint violation — so a caller correcting a payload sees
    /// all of them at once rather than one per round-trip. An empty vector
    /// means the metadata is valid.
    ///
    /// Choosing a wire reason code from the result is the caller's job, not
    /// this type's: see [`MetadataViolation`].
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Internal`] only if the declared subset of the
    /// map fails to serialize, which the `BTreeMap<String, String>` shape
    /// makes unreachable in practice.
    pub fn validate(
        &self,
        metadata: &BTreeMap<String, String>,
    ) -> Result<Vec<MetadataViolation>, DomainError> {
        let mut violations: Vec<MetadataViolation> = metadata
            .keys()
            .filter(|key| !self.declared_keys.contains(key.as_str()))
            .map(|key| MetadataViolation::UndeclaredKey { key: key.clone() })
            .collect();

        let declared: BTreeMap<&String, &String> = metadata
            .iter()
            .filter(|(key, _)| self.declared_keys.contains(key.as_str()))
            .collect();
        let instance = serde_json::to_value(&declared)
            .map_err(|e| DomainError::Internal(format!("metadata is not serializable: {e}")))?;

        violations.extend(self.validator.iter_errors(&instance).map(|e| {
            MetadataViolation::ValueConstraint {
                message: e.to_string(),
            }
        }));

        Ok(violations)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "metadata_tests.rs"]
mod metadata_tests;
