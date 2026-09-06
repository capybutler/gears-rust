//! The closed metadata surface a meter declares.
//!
//! Task 5 adds schema compilation and per-entry validation here. This
//! version holds only the key-extraction half: which property names a
//! meter's `metadata` object declares, merged across its inheritance chain.
//!
//! No sibling `metadata_tests.rs` exists yet: `compile`/`declared_keys` are
//! exercised only transitively today, through `declaration_tests.rs`'s
//! `ResolvedDeclaration::from_schema` tests. That is adequate for this thin,
//! infallible extraction, but Task 5's validation logic (and its new
//! failure mode) is substantial enough to need its own dedicated test file —
//! add one then, following the crate's `#[path = "..."]` sibling-file
//! convention.

use std::collections::BTreeSet;

use serde_json::Value;
use types_registry_sdk::GtsTypeSchema;
use usage_collector_sdk::MeterTypeId;

use crate::domain::error::DomainError;

/// A meter's `metadata` subschema.
///
/// Declared property names are the closed metadata surface for the meter:
/// an entry carrying any other key is rejected (Task 5), and every declared
/// key is groupable and equality-filterable on both read paths (DESIGN
/// §3.1).
#[derive(Debug)]
pub struct CompiledMetadataSchema {
    declared_keys: BTreeSet<String>,
}

impl CompiledMetadataSchema {
    /// Reads the declared property names off the `metadata` property merged
    /// across the schema chain.
    ///
    /// # Errors
    ///
    /// Infallible today. Task 5 makes it fail on a subschema that does not
    /// compile as a JSON Schema validator; the signature carries the
    /// `Result` from the start so that change is not a breaking one for
    /// callers.
    #[allow(clippy::unnecessary_wraps)]
    pub fn compile(_id: &MeterTypeId, schema: &GtsTypeSchema) -> Result<Self, DomainError> {
        let merged = schema.effective_properties();
        let declared_keys = merged
            .get("metadata")
            .and_then(|m| m.get("properties"))
            .and_then(Value::as_object)
            .map(|props| props.keys().cloned().collect())
            .unwrap_or_default();
        Ok(Self { declared_keys })
    }

    /// The declared property names. Declared equals groupable and
    /// equality-filterable on both read paths.
    #[must_use]
    pub fn declared_keys(&self) -> &BTreeSet<String> {
        &self.declared_keys
    }
}
