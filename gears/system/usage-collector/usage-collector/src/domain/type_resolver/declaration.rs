//! The declaration attributes the write and read paths read off a meter.

use std::sync::Arc;

use types_registry_sdk::GtsTypeSchema;
use usage_collector_sdk::{AggregationFold, MeterTypeId};

use crate::domain::error::DomainError;
use crate::domain::type_resolver::metadata::CompiledMetadataSchema;

/// A meter's declaration, resolved from `types-registry` and cached.
///
/// Fold, canonical unit and metadata surface are immutable for the life of a
/// GTS type, which is what makes resolving them at read time safe: the gear
/// never pins them onto an accepted entry.
///
/// `retention` is deliberately absent. The storage plugin reads it from
/// `types-registry` itself, because the plugin is what applies it
/// (DESIGN §3.2 "Type Resolver" responsibility scope; DESIGN §3.3, plugin
/// obligations).
#[derive(Debug, Clone)]
pub struct ResolvedDeclaration {
    /// The meter this declaration describes.
    pub gts_type_id: MeterTypeId,
    /// The single fold the aggregate path serves for this meter.
    pub aggregation_fold: AggregationFold,
    /// The unit quantities travel and persist in. No path converts or scales.
    pub canonical_unit: String,
    /// The closed metadata surface, compiled once per declaration.
    pub metadata_schema: Arc<CompiledMetadataSchema>,
    /// ISO 8601 duration, informational only (DESIGN §3.1). The gear
    /// exposes it to consumers so they can detect gaps or choose an
    /// integration step, but never acts on it itself.
    pub nominal_sampling_interval: Option<String>,
}

/// Names the JSON type of a value for a diagnostic. Never the value's own
/// content: it may be arbitrarily large, and the point is only to tell an
/// operator what shape they wrote instead of what was expected.
fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// Reads a required string-valued trait, distinguishing "absent" from
/// "present but not a string" so the diagnostic names the actual defect.
///
/// `traits.get(key)` collapsing both cases into one "declares no `{key}`"
/// message would send an operator who wrote e.g. `"aggregation_fold": 5`
/// hunting for a missing key that is not the problem — fail-closed only
/// earns its keep when the failure is actionable.
fn require_trait_str<'a>(
    traits: &'a serde_json::Value,
    key: &str,
    gts_type_id: &MeterTypeId,
) -> Result<&'a str, DomainError> {
    match traits.get(key) {
        None => Err(DomainError::declaration_incomplete(
            gts_type_id,
            &format!("declares no `{key}`"),
        )),
        Some(serde_json::Value::String(s)) => Ok(s.as_str()),
        Some(other) => Err(DomainError::declaration_incomplete(
            gts_type_id,
            &format!(
                "declares `{key}` as {}, not a string",
                json_type_name(other)
            ),
        )),
    }
}

impl ResolvedDeclaration {
    /// Parses a declaration out of a registered type schema.
    ///
    /// Reads `x-gts-traits` merged across the inheritance chain, and the
    /// `metadata` property likewise merged, so a trait or a metadata
    /// constraint declared on an ancestor is honoured.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] when a mandatory trait is missing, is present
    /// but not a string, or names a fold this major version does not serve.
    /// Every case fails closed: the gear never substitutes a default for a
    /// declared attribute. Also returns whatever
    /// [`CompiledMetadataSchema::compile`] returns, which fails when the
    /// declared `metadata` subschema is not a compilable JSON Schema.
    pub fn from_schema(
        gts_type_id: MeterTypeId,
        schema: &GtsTypeSchema,
    ) -> Result<Self, DomainError> {
        let traits = schema.effective_traits();

        let fold_raw = require_trait_str(&traits, "aggregation_fold", &gts_type_id)?;
        let aggregation_fold: AggregationFold = fold_raw.parse().map_err(|_| {
            DomainError::declaration_incomplete(
                &gts_type_id,
                &format!(
                    "declares `aggregation_fold: {fold_raw}`, which this \
                     major version does not serve"
                ),
            )
        })?;

        let canonical_unit = require_trait_str(&traits, "canonical_unit", &gts_type_id)?.to_owned();

        let nominal_sampling_interval = traits
            .get("nominal_sampling_interval")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);

        let metadata_schema = Arc::new(CompiledMetadataSchema::compile(&gts_type_id, schema)?);

        Ok(Self {
            gts_type_id,
            aggregation_fold,
            canonical_unit,
            metadata_schema,
            nominal_sampling_interval,
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "declaration_tests.rs"]
mod declaration_tests;
