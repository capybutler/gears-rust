//! Foundation domain models for the Usage Collector SDK.

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::str::FromStr;

use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};
use toolkit_odata_macros::ODataFilterable;
use uuid::Uuid;

use gts::GtsTypeId;

use crate::error::UsageCollectorError;
use crate::quantity::UsageQuantity;

// MetadataKey

/// Validating newtype over a metadata key string.
///
/// Every SDK site naming a declared metadata key carries this type rather
/// than a bare `String` — the keys of [`UsageRecord::metadata`], a
/// [`MetadataFilter`]'s key, [`AggregationDimension::Metadata`]'s payload —
/// so a malformed key cannot reach a consumer past the SDK boundary.
///
/// Validation is deliberately minimal: keys are domain-opaque (operators
/// choose them), so the SDK encodes no casing or charset policy.
/// Closed-shape membership against the resolved meter declaration's
/// `metadata_fields` needs that declaration, so it stays a gateway-time
/// check the gateway alone owns.
///
/// # Validation
///
/// - Non-empty.
/// - No NUL bytes (Postgres `jsonb` key requirement).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct MetadataKey(String);

impl MetadataKey {
    /// Creates a [`MetadataKey`] after validating the value is non-empty and
    /// contains no NUL bytes.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorError::InvalidArgument`] when the input is
    /// empty or contains a NUL byte.
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    pub fn new(value: impl Into<String>) -> Result<Self, UsageCollectorError> {
        let raw = value.into();
        if raw.is_empty() {
            return Err(UsageCollectorError::invalid_metadata_key(
                "metadata key must not be empty",
            ));
        }
        if raw.contains('\0') {
            return Err(UsageCollectorError::invalid_metadata_key(
                "metadata key must not contain NUL bytes",
            ));
        }
        Ok(Self(raw))
    }

    /// Borrows the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the newtype and returns the owned string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl AsRef<str> for MetadataKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for MetadataKey {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MetadataKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for MetadataKey {
    type Err = UsageCollectorError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl<'de> Deserialize<'de> for MetadataKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        MetadataKey::new(raw).map_err(serde::de::Error::custom)
    }
}

// Attribution composites

/// Character cap on each attribution component, from the `ResourceRef` and
/// `SubjectRef` schemas (`maxLength: 256`) in `docs/usage-collector-v1.yaml`.
const MAX_ATTRIBUTION_LEN: usize = 256;

/// Refuses an attribution component longer than [`MAX_ATTRIBUTION_LEN`]
/// characters, naming its dotted path.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
fn cap_attribution(field: &str, value: &str) -> Result<(), UsageCollectorError> {
    if value.chars().count() > MAX_ATTRIBUTION_LEN {
        return Err(UsageCollectorError::attribution_too_long(
            field,
            MAX_ATTRIBUTION_LEN,
        ));
    }
    Ok(())
}

/// Reference to the resource instance to which usage is attributed.
/// Mandatory on every usage record.
///
/// Both `resource_id` and `resource_type` are validated non-empty and
/// NUL-byte-free at construction (Postgres `text` column requirement);
/// `Deserialize` routes through [`Self::new`] so wire payloads cannot
/// bypass the invariant.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ResourceRef {
    /// Resource instance identifier inside the attributed tenant scope.
    resource_id: String,
    /// Type discriminator such as `compute.vm`.
    resource_type: String,
}

impl ResourceRef {
    /// Creates a [`ResourceRef`] after validating both components are
    /// non-empty, contain no NUL bytes, and are not longer than 256 characters.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorError::InvalidArgument`] when `resource_id`
    /// or `resource_type` is empty, contains a NUL byte, or longer than 256 characters.
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    pub fn new(
        resource_id: impl Into<String>,
        resource_type: impl Into<String>,
    ) -> Result<Self, UsageCollectorError> {
        let resource_id = resource_id.into();
        if resource_id.is_empty() {
            return Err(UsageCollectorError::invalid_resource_ref(
                "resource_id must not be empty",
            ));
        }
        if resource_id.contains('\0') {
            return Err(UsageCollectorError::invalid_resource_ref(
                "resource_id must not contain NUL bytes",
            ));
        }
        cap_attribution("resource_ref.resource_id", &resource_id)?;
        let resource_type = resource_type.into();
        if resource_type.is_empty() {
            return Err(UsageCollectorError::invalid_resource_ref(
                "resource_type must not be empty",
            ));
        }
        if resource_type.contains('\0') {
            return Err(UsageCollectorError::invalid_resource_ref(
                "resource_type must not contain NUL bytes",
            ));
        }
        cap_attribution("resource_ref.resource_type", &resource_type)?;
        Ok(Self {
            resource_id,
            resource_type,
        })
    }

    /// Borrows the resource instance identifier.
    #[must_use]
    pub fn resource_id(&self) -> &str {
        &self.resource_id
    }

    /// Borrows the resource-type discriminator.
    #[must_use]
    pub fn resource_type(&self) -> &str {
        &self.resource_type
    }
}

impl<'de> Deserialize<'de> for ResourceRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            resource_id: String,
            resource_type: String,
        }
        let Raw {
            resource_id,
            resource_type,
        } = Raw::deserialize(deserializer)?;
        ResourceRef::new(resource_id, resource_type).map_err(serde::de::Error::custom)
    }
}

/// Optional reference to the principal to which usage is attributed.
/// Caller-supplied and never derived from the caller `SecurityContext`.
///
/// `subject_id` is validated non-empty and NUL-byte-free; `subject_type`,
/// when supplied, is validated non-empty and NUL-byte-free (an explicit
/// `Some("")` is rejected). The NUL restriction matches the Postgres
/// `text` column requirement. `Deserialize` routes through [`Self::new`]
/// so wire payloads cannot bypass either invariant.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct SubjectRef {
    /// Internal platform identifier issued by the identity layer.
    subject_id: String,
    /// Optional type discriminator for systems with subject-type taxonomies.
    #[serde(skip_serializing_if = "Option::is_none")]
    subject_type: Option<String>,
}

impl SubjectRef {
    /// Creates a [`SubjectRef`] after validating `subject_id` is non-empty
    /// and `subject_type`, when supplied, is non-empty; both components are
    /// also validated to contain no NUL bytes and are not longer than 256 characters.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorError::InvalidArgument`] when `subject_id`
    /// is empty, `subject_type` is `Some("")`, either component contains
    /// a NUL byte, or longer than 256 characters.
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    pub fn new(
        subject_id: impl Into<String>,
        subject_type: Option<impl Into<String>>,
    ) -> Result<Self, UsageCollectorError> {
        let subject_id = subject_id.into();
        if subject_id.is_empty() {
            return Err(UsageCollectorError::invalid_subject_ref(
                "subject_id must not be empty",
            ));
        }
        if subject_id.contains('\0') {
            return Err(UsageCollectorError::invalid_subject_ref(
                "subject_id must not contain NUL bytes",
            ));
        }
        cap_attribution("subject_ref.subject_id", &subject_id)?;
        let subject_type = match subject_type {
            None => None,
            Some(s) => {
                let s = s.into();
                if s.is_empty() {
                    return Err(UsageCollectorError::invalid_subject_ref(
                        "subject_type must not be empty when supplied",
                    ));
                }
                if s.contains('\0') {
                    return Err(UsageCollectorError::invalid_subject_ref(
                        "subject_type must not contain NUL bytes",
                    ));
                }
                cap_attribution("subject_ref.subject_type", &s)?;
                Some(s)
            }
        };
        Ok(Self {
            subject_id,
            subject_type,
        })
    }

    /// Borrows the subject identifier.
    #[must_use]
    pub fn subject_id(&self) -> &str {
        &self.subject_id
    }

    /// Borrows the optional subject-type discriminator.
    #[must_use]
    pub fn subject_type(&self) -> Option<&str> {
        self.subject_type.as_deref()
    }
}

impl<'de> Deserialize<'de> for SubjectRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            subject_id: String,
            #[serde(default)]
            subject_type: Option<String>,
        }
        let Raw {
            subject_id,
            subject_type,
        } = Raw::deserialize(deserializer)?;
        SubjectRef::new(subject_id, subject_type).map_err(serde::de::Error::custom)
    }
}

// Idempotency key

/// Ceiling on the wire length of an idempotency key, from the
/// `IdempotencyKey` schema in `docs/usage-collector-v1.yaml`.
const MAX_IDEMPOTENCY_KEY_LEN: usize = 256;

/// Validating newtype over the caller-supplied idempotency key string.
///
/// Every [`UsageRecord::idempotency_key`] carries this type rather than a
/// bare `String`: the key is mandatory on every entry, and the newtype
/// enforces that at the type level. It is a dedup-identity input
/// ([`crate::id::derive_usage_record_id`]), so a malformed one must not reach
/// the derivation at all.
///
/// **No prefix is reserved** (DESIGN §3.1, `IdempotencyKey`). Both entry
/// kinds carry a caller-supplied key and an invalidation repeats its
/// target's, so the whole key space stays the caller's and `entry_type` —
/// not the key — is what tells the two apart.
///
/// # Validation
///
/// - Non-empty.
/// - At most 256 characters (`MAX_IDEMPOTENCY_KEY_LEN`).
/// - No ASCII control characters, DEL included — the wire contract's
///   `^[^\x00-\x1F\x7F]+$`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// Creates an [`IdempotencyKey`] after validating it against the wire
    /// contract's `IdempotencyKey` schema (`minLength: 1`,
    /// `maxLength: 256`, `pattern: ^[^\x00-\x1F\x7F]+$`).
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorError::InvalidArgument`] when the input is
    /// empty, longer than 256 characters, or carries an ASCII control
    /// character (including DEL).
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    pub fn new(value: impl Into<String>) -> Result<Self, UsageCollectorError> {
        let raw = value.into();
        validate_idempotency_key(&raw)?;
        Ok(Self(raw))
    }

    /// Rebuilds a key read back from storage.
    ///
    /// Delegates to [`Self::new`], so the two cannot drift. What the separate
    /// spelling carries is provenance: it names a rehydration of a persisted
    /// entry rather than the admission of a submission.
    ///
    /// # Errors
    ///
    /// [`UsageCollectorError::InvalidArgument`] when the stored value is
    /// empty, longer than 256 characters, or carries an ASCII control
    /// character.
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    pub fn from_stored(value: impl Into<String>) -> Result<Self, UsageCollectorError> {
        Self::new(value)
    }

    /// Borrows the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the newtype and returns the owned string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

/// Every check an [`IdempotencyKey`] meets, whichever constructor built it:
/// non-empty, at most [`MAX_IDEMPOTENCY_KEY_LEN`] characters, and no ASCII
/// control character (DEL included).
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
fn validate_idempotency_key(raw: &str) -> Result<(), UsageCollectorError> {
    if raw.is_empty() {
        return Err(UsageCollectorError::invalid_idempotency_key(
            "idempotency_key must not be empty",
        ));
    }
    if raw.chars().count() > MAX_IDEMPOTENCY_KEY_LEN {
        return Err(UsageCollectorError::invalid_idempotency_key(
            "idempotency_key must be at most 256 characters",
        ));
    }
    // `char::is_ascii_control()` covers U+007F (DEL) alongside U+0000..=U+001F.
    // Load-bearing, not cosmetic: [`crate::id::derive_usage_record_id`] joins
    // this value with the other identity inputs under a `0x1F` separator, and
    // the key is not the final field, so a control character inside it would
    // inject a separator into the middle of the pre-image.
    if raw.chars().any(|c| c.is_ascii_control()) {
        return Err(UsageCollectorError::invalid_idempotency_key(
            "idempotency_key must not contain ASCII control characters",
        ));
    }
    Ok(())
}

impl AsRef<str> for IdempotencyKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for IdempotencyKey {
    type Err = UsageCollectorError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl<'de> Deserialize<'de> for IdempotencyKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        IdempotencyKey::new(raw).map_err(serde::de::Error::custom)
    }
}

// ReasonCode

/// Character cap on an invalidation reason code, from the wire contract's
/// `ReasonCode` schema (`maxLength: 128`).
const MAX_REASON_CODE_LEN: usize = 128;

/// Validating newtype over the caller-supplied invalidation reason code.
///
/// [`Invalidation::reason`] carries this rather than a bare `String` for
/// the reason [`IdempotencyKey`] does: the wire contract constrains the
/// value and an unvalidated one would reach a storage plugin.
///
/// The vocabulary itself is deliberately open — the gear records the
/// emitter's stated intent and infers nothing from it, so no closed enum
/// is declared here or on the wire.
///
/// # Validation
///
/// - Non-empty.
/// - At most 128 characters (`MAX_REASON_CODE_LEN`).
/// - No ASCII control characters, DEL included. Unlike [`IdempotencyKey`]'s,
///   this is wire hygiene rather than pre-image safety: the code is not an
///   identity input. The wire contract states no pattern for the code, so
///   this is the stricter of the two and fails closed.
///
/// # Known gap — a whitespace-only code is admitted
///
/// The wire contract and DESIGN §3.1 ask only that the code be present, and
/// [`Self::new`] enforces exactly that, so `" "` is accepted.
/// `cpt-cf-usage-collector-dod-mandatory-reason-code` and
/// `cpt-cf-usage-collector-algo-entry-type-discrimination` ask for more — an
/// absent, empty **or whitespace-only** code must be rejected — and no `trim`
/// runs here or on either submission path. Both identifiers stay unticked for
/// that reason; the fix is one `raw.trim().is_empty()` guard beside the
/// emptiness check, and it changes wire behaviour.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ReasonCode(String);

impl ReasonCode {
    /// Creates a [`ReasonCode`] after validating it against the wire
    /// contract's `ReasonCode` schema (`minLength: 1`, `maxLength: 128`).
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorError::InvalidArgument`] when the input is
    /// empty, longer than 128 characters, or carries an ASCII control character
    /// (including DEL).
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    pub fn new(value: impl Into<String>) -> Result<Self, UsageCollectorError> {
        let raw = value.into();
        if raw.is_empty() {
            return Err(UsageCollectorError::invalid_reason_code(
                "reason_code must not be empty",
            ));
        }
        if raw.chars().count() > MAX_REASON_CODE_LEN {
            return Err(UsageCollectorError::invalid_reason_code(
                "reason_code must be at most 128 characters",
            ));
        }
        // `char::is_ascii_control()` covers U+007F (DEL) alongside
        // U+0000..=U+001F.
        if raw.chars().any(|c| c.is_ascii_control()) {
            return Err(UsageCollectorError::invalid_reason_code(
                "reason_code must not contain ASCII control characters",
            ));
        }
        Ok(Self(raw))
    }

    /// Borrows the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the newtype and returns the owned string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl AsRef<str> for ReasonCode {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ReasonCode {
    type Err = UsageCollectorError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl<'de> Deserialize<'de> for ReasonCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        ReasonCode::new(raw).map_err(serde::de::Error::custom)
    }
}

// MeterTypeId

/// The GTS base type every meter derives from.
///
/// Alias of [`crate::gts::USAGE_RECORD_RESOURCE`] — the same string, not a
/// new identifier — so a meter-type call site can name the base in
/// meter-type terms.
pub const USAGE_RECORD_BASE_TYPE: &str = crate::gts::USAGE_RECORD_RESOURCE;

/// Ceiling on the wire length of a meter type identifier, from
/// `docs/schemas/usage_record.v1.schema.json`'s `gts_type_id` property.
const MAX_METER_TYPE_ID_LEN: usize = 512;

/// Reference to the GTS type declaration a ledger entry is metered against.
///
/// A meter is a derived **type** of [`USAGE_RECORD_BASE_TYPE`] with exactly
/// one further segment — not an instance — so this wraps a [`GtsTypeId`].
/// The declaration it names is owned by `types-registry`; this gear resolves
/// it and mints none.
///
/// The gear infers no metering meaning from the shape of the identifier.
/// Fold, canonical unit, and metadata surface come from the resolved
/// declaration alone.
///
/// Deliberately implements neither `Ord` nor `PartialOrd`: the wrapped
/// [`GtsTypeId`] implements neither. A caller that needs this as a
/// `BTreeMap`/`BTreeSet` key must add manual impls delegating to the string
/// form.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct MeterTypeId(GtsTypeId);

impl MeterTypeId {
    /// Creates a [`MeterTypeId`] after validating it against the base type's
    /// published pattern
    /// (`^gts\.cf\.core\.uc\.usage_record\.v1~[^\x00-\x1F\x7F~]+~$`,
    /// `maxLength: 512`).
    ///
    /// Length and control characters are checked before the value is handed
    /// to [`GtsTypeId::try_new`], so the diagnostic names the real problem
    /// rather than a generic GTS parse error. The control-character exclusion
    /// is load-bearing: [`crate::id::derive_usage_record_id`] joins this value
    /// with the other identity inputs under a `0x1F` separator, so a control
    /// character inside it would let two distinct identities collapse to one
    /// pre-image.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorError::InvalidArgument`] when the value is
    /// longer than 512 bytes, carries an ASCII control character (including
    /// DEL), does not derive from [`USAGE_RECORD_BASE_TYPE`], is not
    /// `~`-terminated, does not add exactly one further derivation segment
    /// (empty, or containing an interior `~`) to the base, or — having
    /// passed all of the above — fails the per-segment GTS grammar enforced
    /// by the delegated [`GtsTypeId::try_new`] call.
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    pub fn new(value: impl Into<String>) -> Result<Self, UsageCollectorError> {
        let raw = value.into();

        if raw.len() > MAX_METER_TYPE_ID_LEN {
            return Err(UsageCollectorError::invalid_meter_type_id(
                &raw,
                "must be at most 512 bytes",
            ));
        }

        // `char::is_ascii_control()` already covers U+007F (DEL) alongside
        // U+0000..=U+001F, so no separate DEL check is needed.
        if raw.chars().any(|c| c.is_ascii_control()) {
            return Err(UsageCollectorError::invalid_meter_type_id(
                &raw,
                "must not contain ASCII control characters",
            ));
        }

        let Some(suffix) = raw.strip_prefix(USAGE_RECORD_BASE_TYPE) else {
            return Err(UsageCollectorError::invalid_meter_type_id(
                &raw,
                "must derive from `gts.cf.core.uc.usage_record.v1~`",
            ));
        };

        // Exactly one further segment: non-empty, `~`-terminated, and with
        // no interior `~` that would make it two.
        if suffix.is_empty() {
            return Err(UsageCollectorError::invalid_meter_type_id(
                &raw,
                "must add exactly one derivation segment to the base type",
            ));
        }
        let Some(segment) = suffix.strip_suffix('~') else {
            return Err(UsageCollectorError::invalid_meter_type_id(
                &raw,
                "must end with `~`",
            ));
        };
        if segment.is_empty() || segment.contains('~') {
            return Err(UsageCollectorError::invalid_meter_type_id(
                &raw,
                "must add exactly one derivation segment to the base type",
            ));
        }

        let parsed = GtsTypeId::try_new(&raw)
            .map_err(|e| UsageCollectorError::invalid_meter_type_id(&raw, &e.to_string()))?;

        Ok(Self(parsed))
    }

    /// Borrows the wire string, terminator included.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }

    /// Borrows the underlying GTS type identifier.
    #[must_use]
    pub fn as_gts(&self) -> &GtsTypeId {
        &self.0
    }
}

impl AsRef<str> for MeterTypeId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for MeterTypeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MeterTypeId {
    type Err = UsageCollectorError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl<'de> Deserialize<'de> for MeterTypeId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        MeterTypeId::new(raw).map_err(serde::de::Error::custom)
    }
}

// Usage-record exchange types

/// Wire name of the covered period's inclusive start bound.
///
/// The two bound names are wire vocabulary rather than Rust field names:
/// they appear in a validation error's `field`, and the read surface
/// reserves and orders on them. Naming them once keeps a typo at one site
/// from silently splitting the vocabulary in two.
pub const WINDOW_START_FIELD: &str = "window_start";

/// Wire name of the covered period's exclusive end bound. See
/// [`WINDOW_START_FIELD`] for why both are constants.
pub const WINDOW_END_FIELD: &str = "window_end";

/// Wire name of the record identifier. A constant for the same reason as the
/// two bound names: the gateway's canonical keyset is `(window_end, id)`,
/// spelled from [`WINDOW_END_FIELD`] and this, and a keyset half-spelled from
/// constants and half from literals is where one half gets repointed and the
/// other does not.
pub const RECORD_ID_FIELD: &str = "id";

/// The REST path of the dedicated backfill route, named by the live path's
/// past-tolerance rejection.
///
/// A constant because the string appears in that rejection message and in the
/// route registration, and must stay in step with `usage-collector-v1.yaml`,
/// which carries the path independently.
pub const BACKFILL_ROUTE_PATH: &str = "/usage-collector/v1/records/backfill";

/// The closed discriminator between a measurement and a withdrawal.
///
/// **Caller-supplied on the ingestion shape, required and with no default**
/// (DESIGN §3.1, `EntryType`). A stored field of [`CreateUsageRecord`], never
/// inferred: an invalidation repeats its target's idempotency key, so a
/// withdrawal stripped of its discriminator is an exact copy of the entry it
/// means to withdraw and would be absorbed as a retry. This field and
/// `reason_code` must agree, and both projections refuse a submission where
/// they do not.
///
/// **Derived on the persisted shape.** [`UsageRecord::entry_type`] reads it
/// off the [`Invalidation`] the entry carries, so an accepted entry has one
/// place its kind can be read and no marker that can disagree with the
/// payload it marks (`cpt-cf-usage-collector-adr-append-only-invalidation`).
///
/// The kind is never read off the quantity: a zero or negative quantity is an
/// ordinary measurement, and an invalidation echoes the quantity it withdraws
/// rather than negating it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryType {
    /// An ordinary measurement: the entry carries no `invalidates`.
    Record,
    /// A withdrawal: the entry names the record it invalidates.
    Invalidation,
}

impl EntryType {
    /// Every variant, so a guard over the closed label set cannot fall
    /// behind the enum. A third entry type would otherwise be admissible
    /// here and silently absent from the `$filter` literal check that
    /// consumes [`Self::wire_labels`].
    pub const ALL: &'static [Self] = &[Self::Record, Self::Invalidation];

    /// The wire spelling, shared by the REST projection and the `$filter`
    /// surface.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Record => "record",
            Self::Invalidation => "invalidation",
        }
    }

    /// Every admissible wire label, derived from [`Self::ALL`] via
    /// [`Self::as_str`] rather than hand-listed a second time. A third
    /// variant appears here the moment it appears in `ALL`.
    #[must_use]
    pub fn wire_labels() -> &'static [&'static str] {
        static LABELS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
        LABELS
            .get_or_init(|| Self::ALL.iter().map(|v| v.as_str()).collect())
            .as_slice()
    }
}

/// Which ingestion path admitted an entry.
///
/// **Server-assigned, never caller-supplied.** The Ingestion Gateway stamps
/// it from the route the entry arrived on
/// (`cpt-cf-usage-collector-adr-backfill-isolation`), which is why it is
/// absent from [`CreateUsageRecord`] and refused by that type's
/// `deny_unknown_fields` wire shadow.
///
/// It applies to invalidations exactly as to measurements: the covered-period
/// bounds belong to the path rather than the entry kind, so a withdrawal of a
/// period older than the live past tolerance travels the backfill route and
/// reads `backfill`. The value is what lets a consumer rating the feed treat
/// imported history as batch catch-up rather than current consumption.
///
/// Closed, and deliberately so: it is also a bounded metric label on
/// `uc_ingestion_records_total` and `uc_ingestion_duration_seconds`
/// (DESIGN §3.11.5), so an open vocabulary here would be unbounded
/// cardinality there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordOrigin {
    /// Admitted by the live ingestion path — `POST /records` or
    /// `create_usage_records`.
    Live,
    /// Admitted by the dedicated bulk-import route — `POST /records/backfill`
    /// or `backfill_usage_records`.
    Backfill,
}

impl RecordOrigin {
    /// Every variant, so a guard over the closed label set cannot fall
    /// behind the enum. See [`EntryType::ALL`].
    pub const ALL: &'static [Self] = &[Self::Live, Self::Backfill];

    /// The wire spelling, shared by the REST projection, the `$filter`
    /// surface and the metric label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Backfill => "backfill",
        }
    }

    /// Every admissible wire label, derived from [`Self::ALL`]. See
    /// [`EntryType::wire_labels`].
    #[must_use]
    pub fn wire_labels() -> &'static [&'static str] {
        static LABELS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
        LABELS
            .get_or_init(|| Self::ALL.iter().map(|v| v.as_str()).collect())
            .as_slice()
    }
}

/// The withdrawal an invalidation entry carries: the entry it retracts and
/// why.
///
/// Grouping the two makes the half-shape unrepresentable. They are
/// both-or-neither
/// (`cpt-cf-usage-collector-adr-append-only-invalidation`), so no rule,
/// check or error for that case exists anywhere.
///
/// **The wire shape is two flat sibling properties, not a nested object.**
/// `usage-collector-v1.yaml` declares `invalidates` and `reason_code` side by
/// side on `UsageRecord` and `CreateUsageRecordRequest`; the serde shadows in
/// the wire-codec section split and rejoin the pair so the bytes are
/// unchanged. A plugin author therefore implements against one grouped field
/// while reading an OAS that shows two properties.
///
/// Fields are public rather than accessor-guarded: unlike [`ResourceRef`] and
/// [`SubjectRef`] this type has no cross-field invariant to protect — both
/// components are mandatory by construction and [`ReasonCode`] validates
/// itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Invalidation {
    /// The accepted entry this one withdraws. Not an input to the derived
    /// identity; see [`UsageRecord::invalidation`].
    pub target: Uuid,
    /// Why the withdrawal was issued. Forbidden on an ordinary record —
    /// including one whose quantity is negative, which records real
    /// consumption rather than a correction.
    pub reason: ReasonCode,
}

/// Single usage record. The persisted shape is the canonical return value
/// of every create surface (new insert or silent idempotency replay).
///
/// A withdrawal's two halves are one field ([`Invalidation`]), so the pairing
/// is a property of this type rather than of a validation step. The only
/// place the pair can arrive apart is a wire body, and the shadow struct that
/// deserializes one refuses the half-shape there. The wire encoding is
/// unchanged by the grouping: two flat sibling properties, both omitted on an
/// ordinary measurement.
// @cpt-dod:cpt-cf-usage-collector-entity-model:p1
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(try_from = "UsageRecordWire")]
pub struct UsageRecord {
    /// Deterministic gateway-derived entry identity — see
    /// [`crate::derive_usage_record_id`], which is normative for its inputs.
    /// Stamped by [`CreateUsageRecord::try_into_usage_record`] or
    /// [`CreateUsageRecord::try_into_invalidation_record`] on create and
    /// authoritative on read. It cannot be caller-supplied: the create
    /// surface takes the identity-free [`CreateUsageRecord`], not this type.
    pub id: Uuid,
    /// Meter this record attaches to — the derived GTS type declaration
    /// (`gts.cf.core.uc.usage_record.v1~<segment>~`) resolved through
    /// `types-registry`, not a plugin-owned catalog row.
    pub gts_type_id: MeterTypeId,
    /// Owning tenant for this record. Caller-supplied; PDP uses it as the
    /// `OWNER_TENANT_ID` attribute.
    pub tenant_id: Uuid,
    /// Resource attribution composite (mandatory).
    pub resource_ref: ResourceRef,
    /// Optional subject attribution composite.
    pub subject_ref: Option<SubjectRef>,
    /// Caller-supplied metadata. Keys are validated [`MetadataKey`]s and
    /// values are typed as `String` end-to-end; closed-shape membership
    /// against the usage type's `metadata_fields` and the operator-configured
    /// size cap are enforced at the gateway before plugin dispatch. Omitted
    /// from the wire when empty.
    pub metadata: BTreeMap<MetadataKey, String>,
    /// The measured quantity, in the canonical unit of the entry's GTS type.
    /// Validated against the published range at construction and carried on
    /// the wire as a JSON string (see [`crate::UsageQuantity`]). The sign is
    /// never constrained and carries no structural meaning: a negative
    /// quantity records real consumption, and an invalidation echoes the
    /// quantity it withdraws rather than negating it
    /// (`cpt-cf-usage-collector-adr-append-only-invalidation`).
    ///
    /// The wire encoding of this type is declared on its two shadow structs
    /// in the wire-codec section, because it serializes through them.
    pub quantity: UsageQuantity,
    /// Mandatory caller-supplied key for at-least-once-with-dedup semantics,
    /// on both entry kinds: an invalidation repeats its target's (DESIGN
    /// §3.1, `IdempotencyKey`), which is how the gateway finds the entry
    /// being withdrawn. An identity input, so a single stable per-meter key
    /// covers many periods without collapsing them onto one entry.
    pub idempotency_key: IdempotencyKey,
    /// Gear-assigned instant of acceptance, stamped once per request by the
    /// Ingestion Gateway at microsecond precision. Never caller-supplied:
    /// [`CreateUsageRecord`] has no such field and its wire shadow refuses
    /// one. An absorbed retry reports the stored entry's instant, which is
    /// how a caller tells a replay from a first write. No identity input, and
    /// not compared on a dedup collision.
    pub accepted_at: time::OffsetDateTime,
    /// Which path admitted this entry — server-assigned by the Ingestion
    /// Gateway from the route it arrived on, never caller-supplied. See
    /// [`RecordOrigin`]. Not an input to [`Self::id`]'s derivation.
    pub origin: RecordOrigin,
    /// The withdrawal this entry carries, absent on an ordinary measurement.
    /// Its presence is what makes this entry an invalidation — hence
    /// [`Self::entry_type`], which reads it.
    ///
    /// [`Invalidation::target`] is no identity input, and server-assigned
    /// rather than caller-supplied (DESIGN §3.1, Field-ownership): the
    /// gateway derives it from this entry's own identity inputs with
    /// `entry_type = record` and stamps it here. Every withdrawal of one
    /// entry therefore reaches one identity, so a second collides instead of
    /// producing a second withdrawal.
    pub invalidation: Option<Invalidation>,
    /// Inclusive start of the emitter-supplied covered period (RFC 3339 on
    /// the wire) — the only emitter-supplied time attribution an entry
    /// carries. Persisted at microsecond precision, UTC-normalized by
    /// [`CreateUsageRecord::try_into_usage_record`].
    pub window_start: time::OffsetDateTime,
    /// Exclusive end of the covered period. At or after
    /// [`Self::window_start`]; equal bounds mark a point event, not an error.
    ///
    /// This is the bound every read path selects on; see
    /// [`crate::TimeRange::contains_window_end`], which is normative for the
    /// rule. Every raw-path page order also names it, so the column the range
    /// selects on is a sort column too — with no caller `$orderby` the
    /// leading one, letting a single index serve both.
    pub window_end: time::OffsetDateTime,
}

impl UsageRecord {
    /// This entry's kind, derived from the reference it carries.
    ///
    /// Not a field: a stored discriminator is a second place the kind can be
    /// read and the two can disagree
    /// (`cpt-cf-usage-collector-adr-append-only-invalidation`). Every
    /// surface that needs the value computes it here.
    #[must_use]
    pub const fn entry_type(&self) -> EntryType {
        if self.invalidation.is_some() {
            EntryType::Invalidation
        } else {
            EntryType::Record
        }
    }

    /// Whether `other` carries the same caller-supplied fields as `self`.
    ///
    /// This is the dedup comparison (DESIGN §3.1 "Collision resolution"): two
    /// entries on one dedup identity are one entry when every field the caller
    /// supplied is equal, and a conflict otherwise. Most server-assigned
    /// fields are not compared: `id` (derived from the identity
    /// both sides already share), `accepted_at` (stamped afresh per request)
    /// and `origin` (the route, which a retry may change).
    ///
    /// The exception, an invalidation's [`Invalidation::target`], **is**
    /// compared, because it travels grouped with the caller's `reason_code`
    /// in one field. DESIGN says `invalidates` "is derived from the identity
    /// and is not compared", and that is not a departure from it: two entries
    /// reaching this comparison share the identity the target is derived
    /// from, so including it can change no outcome. What the grouped field
    /// really decides here is `reason_code`.
    ///
    /// The one case where it does decide something needs a non-conformant
    /// plugin to reach: on the batch path the right-hand side can be an
    /// `existing` row the *plugin* supplied with an `IdempotencyConflict`
    /// (`domain::service`'s `settle_dispatched` → `resolve_follower`). A row
    /// equal in every caller-supplied field but carrying a different
    /// `invalidates` contradicts its own dedup identity, and is reported as a
    /// conflict rather than absorbed.
    ///
    /// Both sides are destructured, so a field added to [`UsageRecord`] fails
    /// to compile here until it is classified.
    #[must_use]
    pub fn caller_supplied_eq(&self, other: &Self) -> bool {
        let Self {
            id: _,
            gts_type_id,
            tenant_id,
            resource_ref,
            subject_ref,
            metadata,
            quantity,
            idempotency_key,
            accepted_at: _,
            origin: _,
            invalidation,
            window_start,
            window_end,
        } = self;
        let Self {
            id: _,
            gts_type_id: other_gts_type_id,
            tenant_id: other_tenant_id,
            resource_ref: other_resource_ref,
            subject_ref: other_subject_ref,
            metadata: other_metadata,
            quantity: other_quantity,
            idempotency_key: other_idempotency_key,
            accepted_at: _,
            origin: _,
            invalidation: other_invalidation,
            window_start: other_window_start,
            window_end: other_window_end,
        } = other;
        gts_type_id == other_gts_type_id
            && tenant_id == other_tenant_id
            && resource_ref == other_resource_ref
            && subject_ref == other_subject_ref
            && metadata == other_metadata
            && quantity == other_quantity
            && idempotency_key == other_idempotency_key
            && invalidation == other_invalidation
            && window_start == other_window_start
            && window_end == other_window_end
    }
}

/// Identity-free create submission — the input to every create surface
/// ([`crate::UsageCollectorClientV1::create_usage_records`]).
///
/// This mirrors [`UsageRecord`] with [`Self::entry_type`] added — the
/// persisted shape derives its kind, this one is told it — and minus the
/// fields the server assigns: `id`, `accepted_at`, `origin`, and, on an
/// invalidation, the target the gateway resolves. Encoding "id is derived,
/// not supplied" in the type keeps a caller from constructing a meaningless
/// identity the gateway would only discard. The wire REST surface encodes the
/// same shape as `CreateUsageRecordRequest`.
///
/// One shape carries both entry kinds and [`Self::entry_type`] alone decides
/// which (DESIGN §3.1, `CreateUsageRecord`). `reason_code` is the caller's
/// second statement about that same kind rather than the kind itself:
/// required on an `invalidation`, forbidden on a `record`, and both
/// projections refuse a submission whose two statements disagree.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(try_from = "CreateUsageRecordWire")]
pub struct CreateUsageRecord {
    /// The kind this submission declares — caller-supplied, required, and
    /// the sixth input to the derived identity.
    ///
    /// **Never inferred** (DESIGN §3.1, `EntryType`). It is the one field
    /// that keeps a withdrawal's identity apart from its target's: the
    /// faithful copy leaves every other identity input equal to the
    /// target's, so a submission that omitted this would derive the
    /// target's own `id` and be absorbed as a retry of it.
    ///
    /// It must agree with [`Self::invalidation`], and the projections enforce
    /// that rather than the type.
    pub entry_type: EntryType,
    /// Meter this record attaches to — the derived GTS type declaration
    /// (`gts.cf.core.uc.usage_record.v1~<segment>~`) resolved through
    /// `types-registry`, not a plugin-owned catalog row.
    pub gts_type_id: MeterTypeId,
    /// Owning tenant for this record. Caller-supplied; PDP uses it as the
    /// `OWNER_TENANT_ID` attribute.
    pub tenant_id: Uuid,
    /// Resource attribution composite (mandatory).
    pub resource_ref: ResourceRef,
    /// Optional subject attribution composite.
    pub subject_ref: Option<SubjectRef>,
    /// Caller-supplied metadata. Same validation and closed-shape rules as
    /// [`UsageRecord::metadata`]. Omitted from the wire when empty.
    pub metadata: BTreeMap<MetadataKey, String>,
    /// The measured quantity. Same encoding and sign rules as [`UsageRecord::quantity`].
    pub quantity: UsageQuantity,
    /// The caller's key, required on **both** entry kinds: a withdrawal
    /// repeats its target's, which is what lets the gateway find the target
    /// from the submission alone (DESIGN §3.1, Target resolution).
    ///
    /// `Option` rather than a bare key because the absence is a caller
    /// mistake a projection has to report: both projections refuse a
    /// submission missing one.
    pub idempotency_key: Option<IdempotencyKey>,
    /// The withdrawal's stated reason, present exactly when this submission
    /// is an invalidation.
    ///
    /// **No target.** DESIGN §3.1's Field-ownership table makes
    /// `invalidates` server-assigned: the gateway derives the target's
    /// identifier from this submission's own identity inputs with
    /// `entry_type = record`, looks it up converged-only under the PDP scope,
    /// and stamps the result. A caller-supplied target would let an emitter
    /// name a record it never measured.
    pub invalidation: Option<ReasonCode>,
    /// Inclusive start of the covered period this submission measures (RFC
    /// 3339 on the wire; the codec requires an offset, so an offset-less
    /// timestamp never reaches the projection). An identity input, so it
    /// feeds the derived `id`.
    pub window_start: time::OffsetDateTime,
    /// Exclusive end of the covered period. Must be at or after
    /// [`Self::window_start`]; equal bounds submit a point event. An identity
    /// input, which is why an emitter that recomputes its bounds on retry
    /// derives a different identifier, and why deterministic bounds are an
    /// emitter obligation.
    pub window_end: time::OffsetDateTime,
}

impl CreateUsageRecord {
    /// The kind this submission declares — [`Self::entry_type`], read back.
    ///
    /// Unlike [`UsageRecord::entry_type`] this reads a stored field rather
    /// than projecting one: DESIGN §3.1 requires the discriminator on every
    /// submission and forbids inferring it, so there is nothing here to
    /// project it from. An accessor all the same, so asking a submission its
    /// kind reads the same way as asking an accepted entry.
    #[must_use]
    pub const fn entry_type(&self) -> EntryType {
        self.entry_type
    }

    /// Refuses a submission whose two statements about its own kind
    /// disagree.
    ///
    /// DESIGN §3.1, "Entry type and reason code": `reason_code` is required
    /// when `entry_type` is `invalidation` and MUST NOT appear on a `record`.
    /// Both halves are caller-supplied, so a disagreement is an emitter error
    /// — [`UsageCollectorError::InvalidArgument`] naming `reason_code`, not
    /// the [`UsageCollectorError::Internal`] the projections raise when the
    /// submission is consistent and the *projection* is the wrong one for it.
    ///
    /// Both projections run this **before** their own guard, and that
    /// ordering decides the classification of one disagreement: a submission
    /// declaring `entry_type: invalidation` with no reason code satisfies
    /// either projection's guard, so run second it would be reported as a
    /// host-contract breach against a gateway that chose correctly. The
    /// mirror case — a `record` stating a reason code — satisfies **neither**
    /// guard and so reaches this check in either order, but only because
    /// `try_into_usage_record`'s guard keys on the declared `entry_type`
    /// rather than on `invalidation.is_some()`; see the comment there.
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload"
    )]
    fn require_agreeing_kind(&self) -> Result<(), UsageCollectorError> {
        match (self.entry_type, self.invalidation.is_some()) {
            (EntryType::Invalidation, false) => {
                Err(UsageCollectorError::reason_code_required_on_invalidation())
            }
            (EntryType::Record, true) => {
                Err(UsageCollectorError::reason_code_forbidden_on_record())
            }
            (EntryType::Record, false) | (EntryType::Invalidation, true) => Ok(()),
        }
    }

    /// Projects an ordinary measurement into the persisted [`UsageRecord`]
    /// shape, validating the submission's own shape first.
    ///
    /// This is the single point at which a measurement acquires its identity,
    /// and the period validation is inseparable from it: both period
    /// preconditions must be rejected **before** the derivation runs, so the
    /// projection is fallible rather than the caller's obligation.
    ///
    /// In order: `entry_type` and `reason_code` must agree and the declared
    /// kind must be `record`; both bounds are normalized to UTC; each must
    /// then carry at most microsecond precision; the period must be ordered
    /// (`window_start <= window_end`, equal bounds being a point event); a key
    /// must be present; and only then is `id` derived
    /// ([`crate::id::derive_usage_record_id`], with `entry_type = record`).
    /// Every other field is forwarded verbatim; `origin` and `accepted_at`
    /// are server-assigned and stamped from the arguments.
    ///
    /// **A submission is refused rather than reconciled.** Dropping a half
    /// would turn a withdrawal into an ordinary measurement, colliding with
    /// the very entry it meant to withdraw.
    ///
    /// `origin` arrives as an argument rather than being stamped onto an
    /// already-constructed record, so one site decides an entry's origin and
    /// no modified copy sits on the ingestion path. A convention rather than
    /// a guarantee: [`UsageRecord`]'s fields are public.
    ///
    /// Neither period precondition truncates. A truncated bound would be
    /// persisted under an `id` derived from the truncated value while the
    /// emitter reproduces the id it submitted.
    ///
    /// The rules an invalidation must satisfy against its *target* — that it
    /// resolves, is itself a record, and is copied faithfully — need a lookup
    /// and belong to the ingestion gateway. At most one invalidation per
    /// record needs no rule at all: every withdrawal of one record reaches
    /// the same identity, so ordinary dedup settles it.
    ///
    /// # Errors
    ///
    /// [`UsageCollectorError::InvalidArgument`] when the submission's
    /// `entry_type` and `reason_code` disagree, when a bound is finer than
    /// microsecond precision, when the period is inverted, or when no
    /// idempotency key is present — all emitter errors.
    ///
    /// [`UsageCollectorError::Internal`] when a self-consistent submission
    /// declares `entry_type: invalidation`: it is well-formed and simply not
    /// this projection's, needing a target only the gateway can resolve, so
    /// arriving here means this crate's caller chose wrongly. See
    /// [`UsageCollectorError::withdrawal_needs_its_target`].
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    pub fn try_into_usage_record(
        self,
        origin: RecordOrigin,
        accepted_at: time::OffsetDateTime,
    ) -> Result<UsageRecord, UsageCollectorError> {
        self.require_agreeing_kind()?;
        // Keyed on the declared `entry_type`, not on `invalidation.is_some()`:
        // keyed on the reason code, a `record` that states one would satisfy
        // this guard, and whether the emitter got a 400 or the gateway got an
        // `Internal` would depend on `require_agreeing_kind` running first.
        if matches!(self.entry_type, EntryType::Invalidation) {
            return Err(UsageCollectorError::withdrawal_needs_its_target());
        }
        self.project(origin, accepted_at, None)
    }

    /// Projects a withdrawal, stamping the target the gateway resolved.
    ///
    /// Separate from [`Self::try_into_usage_record`] because the target is
    /// server-assigned and only the gateway knows it: a single projection
    /// would either take a caller-supplied target, which DESIGN forbids, or
    /// leave the field unset on an entry whose whole purpose is to name it.
    ///
    /// `target` is the identifier the gateway resolved for the entry being
    /// withdrawn — [`crate::derive_usage_record_id`] over this submission's
    /// own identity inputs with [`EntryType::Record`], looked up
    /// converged-only under the PDP scope (DESIGN §3.1, Target resolution).
    /// It is not re-derived here, so an unresolvable target has already been
    /// refused by the time this is called.
    ///
    /// Validation and derivation are otherwise [`Self::try_into_usage_record`]'s,
    /// except that `id` is derived with `entry_type = invalidation` — the one
    /// input keeping this entry's identity apart from its target's.
    ///
    /// # Errors
    ///
    /// [`UsageCollectorError::InvalidArgument`] on the same grounds as
    /// [`Self::try_into_usage_record`] — including a submission that
    /// declares `entry_type: invalidation` and states no reason code.
    ///
    /// [`UsageCollectorError::Internal`] when a self-consistent submission
    /// declares `entry_type: record`, which is this projection's mirror of
    /// the misuse above. See [`UsageCollectorError::missing_reason_code`].
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    pub fn try_into_invalidation_record(
        mut self,
        origin: RecordOrigin,
        accepted_at: time::OffsetDateTime,
        target: Uuid,
    ) -> Result<UsageRecord, UsageCollectorError> {
        self.require_agreeing_kind()?;
        let Some(reason) = self.invalidation.take() else {
            return Err(UsageCollectorError::missing_reason_code());
        };
        self.project(origin, accepted_at, Some(Invalidation { target, reason }))
    }

    /// The half both projections share: UTC normalization, the two period
    /// preconditions, the key rule, and the derivation over the identity
    /// inputs.
    ///
    /// The digest's kind input is the caller's own [`Self::entry_type`],
    /// because DESIGN §3.1 forbids inferring the kind from anything else.
    /// `invalidation` is the resolved withdrawal the projected entry carries.
    /// The two cannot disagree: each caller above runs `require_agreeing_kind`
    /// and then refuses the kind that is not its own, so `invalidation` is
    /// `Some` exactly when the declared type is [`EntryType::Invalidation`].
    /// A third projection owes the same two checks before calling this.
    ///
    /// `self` is destructured rather than read field by field, so a field
    /// added to [`CreateUsageRecord`] fails to compile here until it is
    /// carried onto the projected entry instead of being silently dropped.
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    fn project(
        self,
        origin: RecordOrigin,
        accepted_at: time::OffsetDateTime,
        invalidation: Option<Invalidation>,
    ) -> Result<UsageRecord, UsageCollectorError> {
        // The pairing both callers are responsible for, made checkable rather
        // than only stated: a third projection that skipped either check trips
        // this in debug and test builds instead of minting an identifier under
        // the wrong type.
        debug_assert_eq!(
            matches!(self.entry_type, EntryType::Invalidation),
            invalidation.is_some(),
            "project() requires the declared entry_type and the resolved withdrawal to agree; \
             its caller owes require_agreeing_kind and its own kind guard",
        );

        let Self {
            // Already checked by the caller against the reason code, and
            // re-encoded in the `invalidation` argument — which is why the
            // kind input below reads this value while the projected entry
            // carries that one.
            entry_type,
            gts_type_id,
            tenant_id,
            resource_ref,
            subject_ref,
            metadata,
            quantity,
            idempotency_key,
            // Always `None` by the time this runs: one caller refuses a
            // submission declaring `entry_type: invalidation`, the other takes
            // the reason out and hands it back as the `invalidation` argument,
            // already paired with its resolved target.
            invalidation: _,
            window_start: submitted_start,
            window_end: submitted_end,
        } = self;

        // Normalization runs before the preconditions. That is safe —
        // `UtcOffset` holds whole seconds, so the nanosecond component is
        // invariant under the conversion and the precision check accepts the
        // same values either way — and it makes both rejections below echo the
        // bound in one rendering, keeping an offset with non-zero seconds out
        // of `crate::error`'s RFC 3339 formatter, which cannot render one.
        let window_start = submitted_start.to_offset(time::UtcOffset::UTC);
        let window_end = submitted_end.to_offset(time::UtcOffset::UTC);

        require_microsecond_precision(WINDOW_START_FIELD, window_start)?;
        require_microsecond_precision(WINDOW_END_FIELD, window_end)?;

        if window_end < window_start {
            return Err(UsageCollectorError::inverted_covered_period(
                window_start,
                window_end,
            ));
        }

        let Some(idempotency_key) = idempotency_key else {
            return Err(UsageCollectorError::missing_idempotency_key());
        };

        let id = crate::id::derive_usage_record_id(
            tenant_id,
            &gts_type_id,
            &idempotency_key,
            window_start,
            window_end,
            entry_type,
        );

        Ok(UsageRecord {
            id,
            gts_type_id,
            tenant_id,
            resource_ref,
            subject_ref,
            metadata,
            quantity,
            idempotency_key,
            // Declaration order, which
            // `clippy::inconsistent_struct_constructor` requires.
            accepted_at,
            origin,
            invalidation,
            window_start,
            window_end,
        })
    }
}

/// Rejects a covered-period bound carrying finer than microsecond
/// precision.
///
/// The microsecond is the precision ceiling of the identity derivation, whose
/// canonical bound form is a six-digit fraction, so a finer value has no
/// representation there. [`crate::canonical_period_bound`] is the other half
/// of that ceiling — it renders the six digits and truncates rather than
/// rejects — and no shared constant holds the two together, only this pair of
/// references. A leap second lands here too, with no branch of its own:
/// `time`'s RFC 3339 parser renders a `:60` second as `59.999999999`, and
/// [`time::Time`] cannot represent second 60 at all.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
fn require_microsecond_precision(
    field: &'static str,
    bound: time::OffsetDateTime,
) -> Result<(), UsageCollectorError> {
    if bound.nanosecond().is_multiple_of(1_000) {
        Ok(())
    } else {
        Err(UsageCollectorError::sub_microsecond_period_bound(
            field, bound,
        ))
    }
}

// Wire codecs
//
// Every entry shape's serde lives here rather than beside the type, so the two
// domain shapes stay adjacent instead of being separated by their plumbing.
// Nothing below is public API: the shadows are private and the two
// `Serialize` impls are the visible behaviour of the domain types themselves.

/// `skip_serializing_if` predicate for a borrowed metadata map. The attribute
/// hands the field by reference, so a borrowed field arrives as `&&BTreeMap`
/// and `BTreeMap::is_empty` does not typecheck against it. The double
/// reference is the attribute's calling convention — hence the allow.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn metadata_is_empty(metadata: &&BTreeMap<MetadataKey, String>) -> bool {
    metadata.is_empty()
}

/// Splits an [`Invalidation`] into the two flat wire properties, or refuses
/// a half-shape.
///
/// The persisted shape's shadow alone. The ingestion shape carries the reason
/// code without a target — `invalidates` is server-assigned, so its shadow
/// refuses a submitted one as an unknown field — leaving no half-shape there.
///
/// The refusal is a plain message rather than a typed
/// [`UsageCollectorError`]: it fires inside a `Deserialize`, which erases
/// everything but the string.
fn invalidation_from_wire(
    invalidates: Option<Uuid>,
    reason_code: Option<ReasonCode>,
) -> Result<Option<Invalidation>, String> {
    match (invalidates, reason_code) {
        (None, None) => Ok(None),
        (Some(target), Some(reason)) => Ok(Some(Invalidation { target, reason })),
        (Some(_), None) => Err(
            "`invalidates` and `reason_code` are both-or-neither on a usage \
             record; `reason_code` is missing"
                .to_owned(),
        ),
        (None, Some(_)) => Err(
            "`invalidates` and `reason_code` are both-or-neither on a usage \
             record; `invalidates` is missing"
                .to_owned(),
        ),
    }
}

/// Owned deserialization shadow for [`UsageRecord`].
///
/// Exists because the pair is flat on the wire and grouped in the type, and
/// `#[serde(flatten)]` — the obvious way to bridge that — cannot be used
/// under `#[serde(deny_unknown_fields)]`. The `deny_unknown_fields` lives
/// here, so a submitted `entry_type` (or any other stray key) is still
/// refused.
///
/// # The four-shadow shape, and the two-shadow alternative
///
/// Each entry type has an owned shadow for reading and a borrowing one for
/// writing — four in all. One owned shadow per type would serve both
/// directions (`try_from` and `into` coexist), at the cost of cloning the
/// whole record on every serialization. That saving is unproven and probably
/// small: neither `Serialize` impl is on an HTTP path this gear serves. The
/// counterweight is that splitting the directions splits each field's serde
/// attributes between them, with nothing but `models_tests`' round-trip making
/// the halves agree. **If a third hand-written codec appears here, revisit
/// this trade rather than copying it a third time.**
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageRecordWire {
    id: Uuid,
    gts_type_id: MeterTypeId,
    tenant_id: Uuid,
    resource_ref: ResourceRef,
    #[serde(default)]
    subject_ref: Option<SubjectRef>,
    #[serde(default)]
    metadata: BTreeMap<MetadataKey, String>,
    quantity: UsageQuantity,
    idempotency_key: String,
    #[serde(with = "time::serde::rfc3339")]
    accepted_at: time::OffsetDateTime,
    origin: RecordOrigin,
    #[serde(default)]
    invalidates: Option<Uuid>,
    #[serde(default)]
    reason_code: Option<ReasonCode>,
    #[serde(with = "time::serde::rfc3339")]
    window_start: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    window_end: time::OffsetDateTime,
}

impl TryFrom<UsageRecordWire> for UsageRecord {
    type Error = String;

    fn try_from(wire: UsageRecordWire) -> Result<Self, Self::Error> {
        let UsageRecordWire {
            id,
            gts_type_id,
            tenant_id,
            resource_ref,
            subject_ref,
            metadata,
            quantity,
            idempotency_key,
            accepted_at,
            origin,
            invalidates,
            reason_code,
            window_start,
            window_end,
        } = wire;
        Ok(Self {
            id,
            gts_type_id,
            tenant_id,
            resource_ref,
            subject_ref,
            metadata,
            quantity,
            idempotency_key: IdempotencyKey::from_stored(idempotency_key)
                .map_err(|e| e.to_string())?,
            accepted_at,
            origin,
            invalidation: invalidation_from_wire(invalidates, reason_code)?,
            window_start,
            window_end,
        })
    }
}

/// Borrowing serialization shadow for [`UsageRecord`].
///
/// Borrowed rather than owned so serializing a record costs no clone of its
/// metadata map and strings — which `#[serde(into = "…")]` would charge on
/// every call. The [`Serialize`] impl below destructures the record
/// exhaustively, so a field added to [`UsageRecord`] and not to this shadow
/// is a compile error rather than a key that silently stops being emitted.
#[derive(Serialize)]
struct UsageRecordWireRef<'a> {
    id: Uuid,
    gts_type_id: &'a MeterTypeId,
    tenant_id: Uuid,
    resource_ref: &'a ResourceRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject_ref: Option<&'a SubjectRef>,
    #[serde(skip_serializing_if = "metadata_is_empty")]
    metadata: &'a BTreeMap<MetadataKey, String>,
    quantity: UsageQuantity,
    idempotency_key: &'a IdempotencyKey,
    #[serde(with = "time::serde::rfc3339")]
    accepted_at: time::OffsetDateTime,
    origin: RecordOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    invalidates: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason_code: Option<&'a ReasonCode>,
    #[serde(with = "time::serde::rfc3339")]
    window_start: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    window_end: time::OffsetDateTime,
}

impl Serialize for UsageRecord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let Self {
            id,
            gts_type_id,
            tenant_id,
            resource_ref,
            subject_ref,
            metadata,
            quantity,
            idempotency_key,
            accepted_at,
            origin,
            invalidation,
            window_start,
            window_end,
        } = self;
        UsageRecordWireRef {
            id: *id,
            gts_type_id,
            tenant_id: *tenant_id,
            resource_ref,
            subject_ref: subject_ref.as_ref(),
            metadata,
            quantity: *quantity,
            idempotency_key,
            accepted_at: *accepted_at,
            origin: *origin,
            invalidates: invalidation.as_ref().map(|i| i.target),
            reason_code: invalidation.as_ref().map(|i| &i.reason),
            window_start: *window_start,
            window_end: *window_end,
        }
        .serialize(serializer)
    }
}

/// Owned deserialization shadow for [`CreateUsageRecord`]. See
/// [`UsageRecordWire`] for why the shadow exists; this is the ingestion
/// half.
///
/// It is **not** on the REST path: a request body deserializes into the host
/// crate's own `CreateUsageRecordRequest` DTO. This shadow is reached when the
/// SDK type itself is deserialized. Both paths must agree about the ingestion
/// key set, and neither can check the other.
///
/// It declares no `invalidates` — that field is server-assigned, and
/// `deny_unknown_fields` above is what rejects a submitted one.
///
/// It **does** declare `entry_type`, with no `serde(default)`: the published
/// schema lists it among the required properties and discriminates its two
/// `oneOf` branches on it, so a body omitting it is refused as a missing
/// field rather than read as a `record`. Its agreement with `reason_code` is
/// checked by the projections, which can raise a typed field violation where
/// a codec could only raise a plain serde string.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateUsageRecordWire {
    entry_type: EntryType,
    gts_type_id: MeterTypeId,
    tenant_id: Uuid,
    resource_ref: ResourceRef,
    #[serde(default)]
    subject_ref: Option<SubjectRef>,
    #[serde(default)]
    metadata: BTreeMap<MetadataKey, String>,
    quantity: UsageQuantity,
    #[serde(default, deserialize_with = "present_idempotency_key")]
    idempotency_key: Option<IdempotencyKey>,
    #[serde(default)]
    reason_code: Option<ReasonCode>,
    #[serde(with = "time::serde::rfc3339")]
    window_start: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    window_end: time::OffsetDateTime,
}

/// Decodes a **present** `idempotency_key`. Absence is `serde(default)`'s
/// `None`; an explicit `null` is refused, because the published schema types
/// the property `string` and requires it on both `oneOf` branches.
fn present_idempotency_key<'de, D>(deserializer: D) -> Result<Option<IdempotencyKey>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    IdempotencyKey::deserialize(deserializer).map(Some)
}

impl TryFrom<CreateUsageRecordWire> for CreateUsageRecord {
    type Error = String;

    fn try_from(wire: CreateUsageRecordWire) -> Result<Self, Self::Error> {
        let CreateUsageRecordWire {
            entry_type,
            gts_type_id,
            tenant_id,
            resource_ref,
            subject_ref,
            metadata,
            quantity,
            idempotency_key,
            reason_code,
            window_start,
            window_end,
        } = wire;
        Ok(Self {
            entry_type,
            gts_type_id,
            tenant_id,
            resource_ref,
            subject_ref,
            metadata,
            quantity,
            idempotency_key,
            invalidation: reason_code,
            window_start,
            window_end,
        })
    }
}

/// Borrowing serialization shadow for [`CreateUsageRecord`]. See
/// [`UsageRecordWireRef`] for why it borrows.
#[derive(Serialize)]
struct CreateUsageRecordWireRef<'a> {
    entry_type: EntryType,
    gts_type_id: &'a MeterTypeId,
    tenant_id: Uuid,
    resource_ref: &'a ResourceRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject_ref: Option<&'a SubjectRef>,
    #[serde(skip_serializing_if = "metadata_is_empty")]
    metadata: &'a BTreeMap<MetadataKey, String>,
    quantity: UsageQuantity,
    #[serde(skip_serializing_if = "Option::is_none")]
    idempotency_key: Option<&'a IdempotencyKey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason_code: Option<&'a ReasonCode>,
    #[serde(with = "time::serde::rfc3339")]
    window_start: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    window_end: time::OffsetDateTime,
}

impl Serialize for CreateUsageRecord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let Self {
            entry_type,
            gts_type_id,
            tenant_id,
            resource_ref,
            subject_ref,
            metadata,
            quantity,
            idempotency_key,
            invalidation,
            window_start,
            window_end,
        } = self;
        CreateUsageRecordWireRef {
            entry_type: *entry_type,
            gts_type_id,
            tenant_id: *tenant_id,
            resource_ref,
            subject_ref: subject_ref.as_ref(),
            metadata,
            quantity: *quantity,
            idempotency_key: idempotency_key.as_ref(),
            reason_code: invalidation.as_ref(),
            window_start: *window_start,
            window_end: *window_end,
        }
        .serialize(serializer)
    }
}

// Declared aggregation fold

/// The single aggregation a meter declares, read from its GTS type
/// declaration's `x-gts-traits.aggregation_fold`.
///
/// Never a request parameter: the aggregate path serves the declared fold and
/// no other, so no class of request is well-formed and semantically wrong. The
/// set is closed — adding a fold is additive, removing one is breaking.
///
/// `SUM` is the only fold yielding a chargeable period quantity. A meter whose
/// consumption is naturally a level is pre-integrated at the emitter into an
/// accrued quantity and declared `SUM`; this gear integrates on no path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum AggregationFold {
    /// Total of the selected quantities.
    Sum,
    /// Count of selected entries. Under this fold a quantity means nothing:
    /// one record is one event.
    Count,
    /// Greatest selected quantity.
    Max,
    /// Least selected quantity.
    Min,
    /// The quantity of the entry with the greatest `window_end`, ties broken
    /// by the greatest `accepted_at`, then by the greatest `id` in byte
    /// order (DESIGN §3.1, which is normative for this and is where to
    /// verify it). `id` is unique, so the order is total, and every one of
    /// those keys compares across tenants and types, so it holds for a group
    /// spanning them. A plugin owes this exact order; a backend-local sequence
    /// or insertion order is not it.
    Latest,
}

impl AggregationFold {
    /// The wire spelling, identical to the trait-schema enum member.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sum => "SUM",
            Self::Count => "COUNT",
            Self::Max => "MAX",
            Self::Min => "MIN",
            Self::Latest => "LATEST",
        }
    }
}

impl std::fmt::Display for AggregationFold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AggregationFold {
    type Err = UsageCollectorError;

    /// Mirrors the serde wire shape without paying a `serde_json::Value`
    /// allocation per call. Case-sensitive: the trait schema's enum is upper
    /// case, so accepting another casing would admit a declaration
    /// `types-registry` rejects.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "SUM" => Ok(Self::Sum),
            "COUNT" => Ok(Self::Count),
            "MAX" => Ok(Self::Max),
            "MIN" => Ok(Self::Min),
            "LATEST" => Ok(Self::Latest),
            other => Err(UsageCollectorError::invalid_aggregation_fold(other)),
        }
    }
}

/// Dimension to group an aggregation by.
///
/// Each variant is a column or JSON-key facet of the underlying record
/// stream. `Metadata(MetadataKey)` carries a single declared metadata key
/// (validated against the queried usage type's `metadata_fields`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregationDimension {
    /// Group by owning tenant.
    TenantId,
    /// Group by `resource_ref.resource_id`.
    ResourceId,
    /// Group by `resource_ref.resource_type`.
    ResourceType,
    /// Group by `subject_ref.subject_id` (rows without a subject are
    /// excluded from the grouping).
    SubjectId,
    /// Group by `subject_ref.subject_type` (rows without a `subject_type`
    /// are excluded from the grouping).
    SubjectType,
    /// Group by the value of a single declared metadata key.
    Metadata(MetadataKey),
}

/// Single aggregated bucket. One entry per element in the query's
/// `group_by` dimensions, in the same order; empty when `group_by` was
/// empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AggregationBucket {
    /// Dimension key values in `group_by` order. Each entry is the string
    /// form of the corresponding [`AggregationDimension`]:
    ///
    /// - [`AggregationDimension::TenantId`] — `Uuid::to_string()`, the
    ///   canonical lowercase hyphenated form
    ///   (`01234567-89ab-cdef-0123-456789abcdef`).
    /// - [`AggregationDimension::ResourceId`],
    ///   [`AggregationDimension::ResourceType`],
    ///   [`AggregationDimension::SubjectId`],
    ///   [`AggregationDimension::SubjectType`] — the record's corresponding
    ///   identifier or type string verbatim.
    /// - [`AggregationDimension::Metadata`] — the metadata value at the
    ///   declared key, which is already a `String` (or string-coercible)
    ///   per the resolved meter declaration's closed-shape metadata rule.
    ///
    /// Plugins own this string-form contract at bucket-construction time;
    /// the SDK does not transform values at the boundary. Empty when
    /// `group_by` was empty (the no-grouping case yields a single bucket).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key: Vec<String>,
    /// Aggregation result for the bucket, carried as an arbitrary-precision
    /// [`bigdecimal::BigDecimal`] so every fold is exact at any magnitude:
    /// Postgres `NUMERIC` is unbounded, and a wide `SUM` can exceed
    /// [`rust_decimal::Decimal`]'s ~7.9×10²⁸ ceiling.
    ///
    /// `None` when no rows matched the bucket, except under `SUM` and
    /// `COUNT`, which answer `Some(0)`. DESIGN §3.3's plugin obligations are
    /// normative: *"`SUM` and `COUNT` are defined over an empty selection and
    /// report `0`; `MAX`, `MIN` and `LATEST` are not and report absent"*.
    ///
    /// Wire-encoded as a JSON string (never a float) for the same round-trip
    /// reason as [`UsageRecord::quantity`], via
    /// [`crate::serde_helpers::bigdecimal_str_option`].
    #[serde(default, with = "crate::serde_helpers::bigdecimal_str_option")]
    pub value: Option<BigDecimal>,
}

/// Aggregated-query result.
///
/// A single bucket with an empty `key` represents the no-grouping case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AggregationResult {
    /// Result buckets in plugin-emitted order.
    pub buckets: Vec<AggregationBucket>,
}

/// Maximum number of buckets a single [`AggregationResult`] may carry.
///
/// The mandatory read-path [`crate::TimeRange`] caps the rows an aggregate
/// *scans*; this caps the distinct *groups* it produces, which a
/// high-cardinality [`AggregationDimension::Metadata`] key could otherwise
/// leave unbounded. The storage plugin MUST bound its own result (e.g. append
/// `LIMIT MAX_AGGREGATION_BUCKETS + 1`); the gateway rejects a result
/// exceeding this cap with a `400`
/// ([`crate::reason::AGGREGATION_RESULT_TOO_LARGE`]). Declared on the wire as
/// `AggregatedQueryResult.buckets`' `maxItems`.
pub const MAX_AGGREGATION_BUCKETS: usize = 100_000;

// Filter surface for `list_usage_records`
//
// `UsageRecordQuery` declares the filterable-field schema for the OData
// surface of `list_usage_records`. The struct is never constructed at
// runtime; it exists solely to feed `#[derive(ODataFilterable)]`, which
// generates `UsageRecordQueryFilterField` and its
// `toolkit_odata::filter::FilterField` impl. Plugin implementations supply
// a `FieldToColumn<UsageRecordFilterField>` mapper next to their entity
// definition; the SDK does not encode storage-layer column mapping.
//
// `gts_type_id` is intentionally absent from this schema. It travels as a
// typed parameter on both read surfaces, and omitting it here makes
// `parse_odata_filter::<UsageRecordFilterField>` reject any
// `gts_type_id`-touching predicate as `FilterError::UnknownField`. That
// covers the wire path only: an in-process caller builds an `ast::Expr` by
// hand, so the host crate's runtime `reject_unpublished_filter_fields` guard
// reserves `gts_type_id` too, and needs to.
//
// The covered-period bounds and `id` ARE on the schema, and none of them is
// `$filter`-reachable. This schema is two vocabularies at once — the
// plugin's field-to-column mapping and the `$orderby` surface — and is a
// superset of `$filter`'s admissible surface rather than a definition of it;
// `PUBLISHED_FILTER_FIELDS` is what defines that, and
// `reject_unpublished_filter_fields` is what enforces it.
//
// Nested attribution composites (`resource_ref`, `subject_ref`) are
// flattened to their leaf identifiers (`resource_id`, `resource_type`,
// `subject_id`, `subject_type`) so filtering goes through the macro-derived
// path rather than a hand-rolled slash-path `FilterField` impl.
//
// `entry_type` is declared `String` on the filter wire (`"record"` /
// `"invalidation"`). The SDK stores no such attribute, so a plugin wanting the
// field filterable holds a column of its own — written or derived, its choice
// — and returns it from `FieldToColumn::map_field`. `map_value` cannot carry
// it: that hook rewrites a value and can change neither column nor operator,
// and `toolkit-db`'s `sea_orm_filter` converts the mapped value before its
// `IS NULL` / `IS NOT NULL` branch, so an `ODataValue::Null` from the hook is
// refused rather than lowered, for `in` as well as `eq`.
//
// `metadata` filtering does not flow through OData — see `MetadataFilter`
// below, supplied as a separate parameter on `list_usage_records`.
// `toolkit-odata` has no general `serde_json::Value` filter surface and
// there is no precedent for one in the workspace.

/// Filterable-field schema for `list_usage_records`'s `ODataQuery` argument.
///
/// Never constructed at runtime. The `dead_code` allow is intentional —
/// the struct is a derive-only artifact (see file-level comment above for
/// rationale).
#[derive(ODataFilterable)]
#[allow(dead_code)]
pub struct UsageRecordQuery {
    /// `usage_records.id` (record primary key). On the filter surface so the
    /// gateway can use it as the canonical cursor tiebreaker
    /// (`(window_end, id)`) and a plugin has a column mapping for it;
    /// **not** filterable — it is off [`PUBLISHED_FILTER_FIELDS`], so
    /// `reject_unpublished_filter_fields` rejects any predicate naming it.
    #[odata(filter(kind = "Uuid"))]
    pub id: Uuid,
    /// `usage_records.window_start` — the inclusive start of the covered
    /// period. Here so a plugin has a column mapping for it and a caller can
    /// order by it; **not** filterable, because the read range travels as a
    /// typed parameter.
    #[odata(filter(kind = "DateTimeUtc"))]
    pub window_start: time::OffsetDateTime,
    /// `usage_records.window_end` — the exclusive end of the covered period,
    /// and the column every read path selects on (see
    /// [`crate::TimeRange::contains_window_end`]). Every raw-path page order
    /// names it, so selection and sorting share a column. Reserved on the
    /// `$filter` surface for the same reason as [`Self::window_start`].
    #[odata(filter(kind = "DateTimeUtc"))]
    pub window_end: time::OffsetDateTime,
    /// `usage_records.tenant_id` (owning tenant). Supports `eq` and `in`.
    #[odata(filter(kind = "Uuid"))]
    pub tenant_id: Uuid,
    /// `usage_records.resource_ref.resource_id`, flattened for the filter
    /// surface.
    #[odata(filter(kind = "String"))]
    pub resource_id: String,
    /// `usage_records.resource_ref.resource_type`, flattened for the filter
    /// surface.
    #[odata(filter(kind = "String"))]
    pub resource_type: String,
    /// `usage_records.subject_ref.subject_id`, flattened for the filter
    /// surface.
    #[odata(filter(kind = "String"))]
    pub subject_id: String,
    /// `usage_records.subject_ref.subject_type`, flattened for the filter
    /// surface.
    #[odata(filter(kind = "String"))]
    pub subject_type: String,
    /// The entry an invalidation withdraws, or absent on an ordinary
    /// record. Filterable (`eq` / `in`) so a consumer folding entries
    /// itself can find a withdrawn pair; **not** an order key, because it
    /// is domain-optional — see [`is_keyset_safe_record_field`].
    #[odata(filter(kind = "Uuid"))]
    pub invalidates: Uuid,
    /// The derived `record` / `invalidation` discriminator. On the filter
    /// surface because the wire contract lists it there; a plugin backs it
    /// with a column of its own and returns that from
    /// `FieldToColumn::map_field` (see the file-level comment above).
    /// **Not** an order key — see [`is_keyset_safe_record_field`].
    #[odata(filter(kind = "String"))]
    pub entry_type: String,
    /// `usage_records.origin` — which ingestion path admitted the entry
    /// (`live` / `backfill`). On the filter surface because DESIGN §3.1 lists
    /// it among the fixed `$filter` and `group_by` fields: a consumer that has
    /// already raised a charge for a period needs to separate imported history
    /// from current consumption.
    ///
    /// Declared `String` on the filter wire like [`Self::entry_type`], but
    /// unlike it a stored attribute rather than a function of an optional one,
    /// so it **is** a sound order key. See [`is_keyset_safe_record_field`].
    #[odata(filter(kind = "String"))]
    pub origin: String,
}

pub use UsageRecordQueryFilterField as UsageRecordFilterField;

/// The `$filter` field set the public contract publishes, and the whole of
/// it — the closed set the gear's `reject_unpublished_filter_fields` guard
/// admits.
///
/// `DESIGN.md`'s `UsageRecordFilterField` row and `usage-collector-v1.yaml`'s
/// `$filter` parameter each enumerate exactly this set, and DESIGN adds
/// "Fixed, not resolved per request". A strict subset of
/// [`UsageRecordFilterField`]'s variants, which are wider: membership
/// in that schema says nothing about `$filter` admissibility (see the
/// file-level comment above `UsageRecordQuery`), and this constant is what
/// says it.
///
/// Exported so
/// [`unpublished_filter_field`](crate::UsageCollectorError::unpublished_filter_field)
/// can render the whole set into its `detail` *from here*, leaving no refused
/// caller to guess and no prose to drift from the guard. Matching against it
/// is case-insensitive, not exact: see the gear's `is_published_filter_field`,
/// which has to agree with `toolkit_odata::FilterField::from_name`, the
/// resolver `$filter` identifiers actually go through.
pub const PUBLISHED_FILTER_FIELDS: &[&str] = &[
    "tenant_id",
    "resource_id",
    "resource_type",
    "subject_id",
    "subject_type",
    "entry_type",
    "origin",
    "invalidates",
];

/// The record attributes every entry carries in its own right, and
/// therefore sound as keyset-pagination ordering keys — the closed set
/// [`is_keyset_safe_record_field`] tests against.
///
/// Exported because it is the admissible `$orderby` vocabulary, so a `400`
/// refusing a caller's order can name the whole closed set.
///
/// Matching is **exact**, unlike [`PUBLISHED_FILTER_FIELDS`] just above, under
/// the same rule: each side matches the way its own downstream resolver
/// matches. An `$orderby` key never passes through
/// `toolkit_odata::filter::FilterField::from_name` — the plugin hands the
/// caller's string straight to its field-to-column map, an exact `match` — so
/// folding case here would admit a key the storage layer then fails to
/// resolve.
pub const KEYSET_SAFE_RECORD_FIELDS: &[&str] = &[
    RECORD_ID_FIELD,
    WINDOW_START_FIELD,
    WINDOW_END_FIELD,
    "tenant_id",
    "resource_id",
    "resource_type",
    "origin",
    "accepted_at",
];

/// Record filter fields sound to use as a keyset-pagination ordering key.
///
/// The storage plugin's keyset continuation is a row-value tuple comparison
/// (`(c1, c2, …) > ($…)`). In SQL three-valued logic a tuple whose leading
/// column is NULL compares as NULL, so every NULL-keyed row is silently
/// dropped from the paged result — and a page ending on such a row cannot
/// encode a `next_cursor` at all (a 500). A keyset key therefore has to be an
/// attribute this SDK guarantees present on every entry. Two kinds of field
/// are not, and both are on the filterable schema:
///
/// - **Domain-optional attributes.** `subject_id` / `subject_type` come from
///   `subject_ref: Option<SubjectRef>`, and the `invalidates` filter field
///   reads the target inside [`UsageRecord::invalidation`], absent on every
///   ordinary measurement.
/// - **Attributes derived from an optional one.** `entry_type` has a value on
///   every entry and is still not keyset-safe: it is
///   [`UsageRecord::entry_type`], a function of `invalidates` that partitions
///   entries on that field's *absence*. The SDK carries no such attribute of
///   its own and obliges no plugin to materialize one, so it will not promise
///   a keyset key over it — whatever column a plugin chooses to build.
///
/// Derivation, not presence, is what separates the second bullet from the
/// safe fields: [`UsageRecord::origin`] and [`UsageRecord::accepted_at`] also
/// have a value on every entry, but each is a *field* of the record, so the
/// SDK's shape obliges every plugin persisting a [`UsageRecord`] to store it
/// non-null. This is a claim about the shape this SDK guarantees, not about
/// any storage schema.
///
/// Enforcement is the **gateway's alone**: it refuses a caller `$orderby` on a
/// non-keyset-safe field with a `400` and guarantees the order slot the Plugin
/// SPI documents on every surface, so a plugin needs no fallback keyset and an
/// unusable order is a gateway breach rather than a case to paper over — see
/// [`crate::UsageCollectorPluginV1::list_usage_records`], normative for the
/// plugin side. The allowlist is fail-closed: a newly added field is unsafe
/// until it is classified there.
///
/// Being on this list is necessary but not sufficient — an order key also has
/// to resolve to a column. For every key but `accepted_at` that vocabulary is
/// [`UsageRecordFilterField`]'s, and `models_tests` checks the two against
/// each other in both directions. `accepted_at` has no
/// `UsageRecordFilterField` variant, so its column resolution is a
/// plugin-level fact; the `TimescaleDB` plugin's `record_column` /
/// `record_row_key` and this crate's reference backend resolve it, each with
/// its own test iterating this constant.
#[must_use]
pub fn is_keyset_safe_record_field(name: &str) -> bool {
    KEYSET_SAFE_RECORD_FIELDS.contains(&name)
}

/// Equality-set filter applied to a single [`UsageRecord::metadata`] key.
///
/// `metadata` keys are not part of any static schema, and `toolkit-odata`'s
/// filter surface cannot express filtering on dynamic map keys. This is the
/// typed side-channel [`crate::UsageCollectorClientV1::list_usage_records`]
/// and the plugin SPI use instead.
///
/// Semantics across a `&[MetadataFilter]`:
///
/// - AND across **every** filter in the slice, including two that name the
///   same key: a storage plugin emits one AND-ed clause per entry, so
///   `[k in {a}, k in {b}]` selects rows whose `k` is both — generally
///   nothing — and is **not** equivalent to the single filter
///   `k in {a, b}`. A consumer that merged same-key entries would widen
///   the result set.
/// - OR within a single filter's `values()`.
/// - An empty slice imposes no metadata filter.
///
/// Constructor [`Self::new`] enforces a validated [`MetadataKey`] (non-empty,
/// no NUL bytes) and a non-empty value set; `Deserialize` routes through
/// `new` so wire payloads cannot bypass validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MetadataFilter {
    key: MetadataKey,
    values: Vec<String>,
}

impl MetadataFilter {
    /// Creates a [`MetadataFilter`] after validating `key` (via
    /// [`MetadataKey::new`]) and that `values` carries at least one entry.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorError::InvalidArgument`] when the key
    /// is empty or contains a NUL byte, or when `values` is empty. A
    /// failing `MetadataKey::new` is rewrapped as
    /// `InvalidMetadataFilter` so callers see one variant for the whole
    /// `MetadataFilter::new` boundary.
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    pub fn new(
        key: impl Into<String>,
        values: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, UsageCollectorError> {
        let key = MetadataKey::new(key).map_err(|err| match err {
            UsageCollectorError::InvalidArgument { detail, .. } => {
                UsageCollectorError::invalid_metadata_filter(detail)
            }
            other => other,
        })?;
        let values: Vec<String> = values.into_iter().map(Into::into).collect();
        if values.is_empty() {
            return Err(UsageCollectorError::invalid_metadata_filter(format!(
                "metadata filter for key `{key}` must carry at least one value"
            )));
        }
        Ok(Self { key, values })
    }

    /// Borrows the metadata key.
    #[must_use]
    pub fn key(&self) -> &MetadataKey {
        &self.key
    }

    /// Borrows the candidate value set.
    #[must_use]
    pub fn values(&self) -> &[String] {
        &self.values
    }
}

impl<'de> Deserialize<'de> for MetadataFilter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            key: String,
            values: Vec<String>,
        }
        let Raw { key, values } = Raw::deserialize(deserializer)?;
        MetadataFilter::new(key, values).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "models_tests.rs"]
mod models_tests;
