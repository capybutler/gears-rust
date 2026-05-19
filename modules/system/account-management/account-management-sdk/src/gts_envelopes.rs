//! Link-time inventory submit for AM envelope schemas (base envelopes
//! used by derived tenant/metadata schemas).
//!
//! Sync-test in this crate diffs the inline literals against
//! `docs/schemas/*.schema.json` so the wire shape and the doc never drift.
//!
//! TODO(cyberfabric/cyberware-rust#1928): migrate to
//! `#[gts_type_schema(dir_path = ...)]` once the macro supports
//! `x-gts-traits-schema` / `x-gts-traits`. Current `inventory::submit!`
//! workaround re-implements what the macro will do at expansion time;
//! cutover removes the inline literals + this drift-guard test.
//! See <https://github.com/cyberfabric/cyberware-rust/issues/1928>.

/// Lazy accessor for the `tenant_type.v1~` envelope JSON Schema.
///
/// Returned as a `String` per [`modkit_gts::InventorySchema::schema_fn`]'s
/// contract. Mirrors
/// `modules/system/account-management/docs/schemas/tenant_type.v1.schema.json`
/// with the addition of `x-gts-traits` defaults on the envelope itself
/// (`allowed_parent_types: []`, `idp_provisioning: false`) — required
/// for `gts::store::validate_entity_traits` (OP#13) to admit the base
/// schema as a deployable entity, because the validator rejects any
/// chain that declares `x-gts-traits-schema` without supplying matching
/// `x-gts-traits` values somewhere in the chain.
fn tenant_type_envelope_schema() -> String {
    r#"{
  "$id": "gts://gts.cf.core.am.tenant_type.v1~",
  "$schema": "http://json-schema.org/draft-07/schema#",
  "description": "Base tenant type schema for Account Management. Derived tenant type schemas resolve behavioral traits via x-gts-traits. Traits configure system behavior for processing tenants of each type - they are not part of the tenant instance data model.",
  "type": "object",
  "x-gts-traits-schema": {
    "type": "object",
    "additionalProperties": false,
    "properties": {
      "allowed_parent_types": {
        "description": "GTS instance identifiers of tenant types allowed as parent. Empty array means the type is root-only or leaf-only. The root tenant type has allowed_parent_types: [] by convention.",
        "type": "array",
        "items": { "type": "string" },
        "default": []
      },
      "idp_provisioning": {
        "description": "Whether tenants of this type typically require dedicated IdP-side resources. Account Management still calls IdpPluginClient::provision_tenant and deprovision_tenant; provider implementations may use this trait to decide whether to create resources or complete as a no-op.",
        "type": "boolean",
        "default": false
      }
    }
  },
  "x-gts-traits": {
    "allowed_parent_types": [],
    "idp_provisioning": false
  }
}"#
    .to_owned()
}

/// Lazy accessor for the `tenant_metadata.v1~` envelope JSON Schema.
///
/// Mirrors
/// `modules/system/account-management/docs/schemas/tenant_metadata.v1.schema.json`
/// with the addition of `x-gts-traits` defaults (`inheritance_policy:
/// "override_only"`) for the same OP#13 reason as
/// [`tenant_type_envelope_schema`].
fn tenant_metadata_envelope_schema() -> String {
    r#"{
  "$id": "gts://gts.cf.core.am.tenant_metadata.v1~",
  "$schema": "http://json-schema.org/draft-07/schema#",
  "description": "Base tenant metadata schema for Account Management. Derived metadata schemas resolve behavioral traits via x-gts-traits. Traits configure how MetadataService treats entries of each schema - they are not part of the metadata payload.",
  "type": "object",
  "x-gts-traits-schema": {
    "type": "object",
    "additionalProperties": false,
    "properties": {
      "inheritance_policy": {
        "description": "How MetadataService resolves values across the tenant hierarchy for this schema. 'override_only' returns the tenant's own entry or empty; 'inherit' walks ancestors via parent_id, stopping at self-managed barriers.",
        "type": "string",
        "enum": ["override_only", "inherit"],
        "default": "override_only"
      }
    }
  },
  "x-gts-traits": {
    "inheritance_policy": "override_only"
  }
}"#
    .to_owned()
}

modkit_gts::inventory::submit! {
    modkit_gts::InventorySchema {
        schema_id: "gts.cf.core.am.tenant_type.v1~",
        schema_fn: tenant_type_envelope_schema,
    }
}

modkit_gts::inventory::submit! {
    modkit_gts::InventorySchema {
        schema_id: "gts.cf.core.am.tenant_metadata.v1~",
        schema_fn: tenant_metadata_envelope_schema,
    }
}

#[cfg(test)]
mod sync_tests {
    //! Drift guard: each `*_envelope_schema()` literal MUST agree with
    //! its `docs/schemas/<file>.json` counterpart, except for the
    //! inline-only `x-gts-traits` defaults block on the envelope itself
    //! (required for `gts::store::validate_entity_traits` / OP#13 to
    //! admit the base schema as a deployable entity). Once
    //! `#[modkit_gts::gts_type_schema(dir_path = ...)]` adoption lands,
    //! this drift guard is replaced by the macro reading the on-disk
    //! file at expansion time.

    use std::fs;
    use std::path::PathBuf;

    use serde_json::Value;

    fn read_disk_schema(file_name: &str) -> Value {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("docs")
            .join("schemas")
            .join(file_name);
        let body = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        serde_json::from_str(&body)
            .unwrap_or_else(|err| panic!("parse {} as JSON: {err}", path.display()))
    }

    fn parse_inline(body: &str, schema_id: &str) -> Value {
        serde_json::from_str(body)
            .unwrap_or_else(|err| panic!("parse inline body for {schema_id} as JSON: {err}"))
    }

    /// Remove the inline-only `x-gts-traits` defaults block so the
    /// remaining shape can be diffed against the docs/schemas/ source
    /// of truth (which omits the block — the validator-level
    /// requirement is satisfied at the inline `inventory::submit!`
    /// site only).
    fn strip_inline_only_keys(value: &mut Value) {
        if let Value::Object(map) = value {
            map.remove("x-gts-traits");
        }
    }

    #[test]
    fn tenant_type_envelope_matches_on_disk_schema() {
        let disk = read_disk_schema("tenant_type.v1.schema.json");
        let inline = super::tenant_type_envelope_schema();
        let mut inventory = parse_inline(&inline, "gts.cf.core.am.tenant_type.v1~");
        strip_inline_only_keys(&mut inventory);
        assert_eq!(
            inventory, disk,
            "inline tenant_type envelope drifted from docs/schemas/tenant_type.v1.schema.json -- \
             re-sync `tenant_type_envelope_schema` with the on-disk file. The inline body MAY \
             additionally carry an `x-gts-traits` defaults block on the envelope itself; any \
             other key MUST agree byte-for-byte after JSON normalisation.",
        );
    }

    #[test]
    fn tenant_metadata_envelope_matches_on_disk_schema() {
        let disk = read_disk_schema("tenant_metadata.v1.schema.json");
        let inline = super::tenant_metadata_envelope_schema();
        let mut inventory = parse_inline(&inline, "gts.cf.core.am.tenant_metadata.v1~");
        strip_inline_only_keys(&mut inventory);
        assert_eq!(
            inventory, disk,
            "inline tenant_metadata envelope drifted from docs/schemas/tenant_metadata.v1.schema.json -- \
             re-sync `tenant_metadata_envelope_schema` with the on-disk file. The inline body MAY \
             additionally carry an `x-gts-traits` defaults block on the envelope itself; any \
             other key MUST agree byte-for-byte after JSON normalisation.",
        );
    }
}
