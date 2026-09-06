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

impl ResolvedDeclaration {
    /// Parses a declaration out of a registered type schema.
    ///
    /// Reads `x-gts-traits` merged across the inheritance chain, and the
    /// `metadata` property likewise merged, so a trait or a metadata
    /// constraint declared on an ancestor is honoured.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] when a mandatory trait is missing, when the
    /// fold is outside the set this major version serves, or when the
    /// metadata subschema does not compile. Every case fails closed: the gear
    /// never substitutes a default for a declared attribute.
    pub fn from_schema(
        gts_type_id: MeterTypeId,
        schema: &GtsTypeSchema,
    ) -> Result<Self, DomainError> {
        let traits = schema.effective_traits();

        let fold_raw = traits
            .get("aggregation_fold")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                DomainError::declaration_incomplete(&gts_type_id, "declares no `aggregation_fold`")
            })?;
        let aggregation_fold: AggregationFold = fold_raw.parse().map_err(|_| {
            DomainError::declaration_incomplete(
                &gts_type_id,
                &format!(
                    "declares `aggregation_fold: {fold_raw}`, which this \
                     major version does not serve"
                ),
            )
        })?;

        let canonical_unit = traits
            .get("canonical_unit")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                DomainError::declaration_incomplete(&gts_type_id, "declares no `canonical_unit`")
            })?
            .to_owned();

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
