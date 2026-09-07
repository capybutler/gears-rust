//! Foundation domain models for the Usage Collector SDK.

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::str::FromStr;

use bigdecimal::BigDecimal;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use toolkit_odata_macros::ODataFilterable;
use uuid::Uuid;

use gts::GtsTypeId;

use crate::error::UsageCollectorError;

// ---------------------------------------------------------------------------
// MetadataKey
// ---------------------------------------------------------------------------

/// Validating newtype over a metadata key string.
///
/// Every site in the SDK that names a declared metadata key — the keys of
/// [`UsageRecord::metadata`], the key of a [`MetadataFilter`], and the
/// payload of [`AggregationDimension::Metadata`] — carries this type rather
/// than a bare `String`, so a malformed key cannot reach any consumer past
/// the SDK boundary.
///
/// Validation rules are intentionally minimal: keys are domain-opaque
/// (operators choose them) so the SDK refuses to encode casing or charset
/// policy.
///
/// Closed-shape membership — every key on a record MUST be in the resolved
/// meter declaration's `metadata_fields` — remains a gateway-time check; it
/// cannot be expressed at the type level without the resolved-declaration
/// context, and the gateway is its single owner.
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

// ---------------------------------------------------------------------------
// Attribution composites
// ---------------------------------------------------------------------------

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
    /// non-empty and contain no NUL bytes.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorError::InvalidArgument`] when `resource_id`
    /// or `resource_type` is empty or contains a NUL byte.
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
    /// also validated to contain no NUL bytes.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorError::InvalidArgument`] when `subject_id`
    /// is empty, `subject_type` is `Some("")`, or either component contains
    /// a NUL byte.
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

// ---------------------------------------------------------------------------
// Idempotency key
// ---------------------------------------------------------------------------

/// Ceiling on the wire length of an idempotency key, from the
/// `IdempotencyKey` schema in `docs/usage-collector-v1.yaml`.
const MAX_IDEMPOTENCY_KEY_LEN: usize = 256;

/// Validating newtype over the caller-supplied idempotency key string.
///
/// Every [`UsageRecord::idempotency_key`] carries this type rather than a
/// bare `String`, and the key is declared mandatory on every record — the
/// newtype enforces that "mandatory" at the type level so an SDK consumer
/// cannot build a record with an empty key. The key is one of the five
/// inputs to the dedup identity, so a malformed one must not reach the
/// derivation at all.
///
/// # Validation
///
/// - Non-empty.
/// - At most 256 bytes (`MAX_IDEMPOTENCY_KEY_LEN`).
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
    /// empty, longer than 256 bytes, or carries an ASCII control character
    /// (including DEL).
    pub fn new(value: impl Into<String>) -> Result<Self, UsageCollectorError> {
        let raw = value.into();
        if raw.is_empty() {
            return Err(UsageCollectorError::invalid_idempotency_key(
                "idempotency_key must not be empty",
            ));
        }
        if raw.len() > MAX_IDEMPOTENCY_KEY_LEN {
            return Err(UsageCollectorError::invalid_idempotency_key(
                "idempotency_key must be at most 256 bytes",
            ));
        }
        // `char::is_ascii_control()` covers U+007F (DEL) alongside
        // U+0000..=U+001F. The exclusion is load-bearing rather than
        // cosmetic: the entry-identity derivation concatenates this value
        // with the other dedup-identity inputs under a `0x1F` separator
        // (`cpt-cf-usage-collector-adr-record-identity-derivation`), and the
        // key is no longer the final field, so a control character inside it
        // would inject a separator into the middle of the pre-image.
        if raw.chars().any(|c| c.is_ascii_control()) {
            return Err(UsageCollectorError::invalid_idempotency_key(
                "idempotency_key must not contain ASCII control characters",
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

// ---------------------------------------------------------------------------
// MeterTypeId
// ---------------------------------------------------------------------------

/// The GTS base type every meter derives from.
///
/// Alias of [`crate::gts::USAGE_RECORD_RESOURCE`] — the same string, not a
/// new identifier. It exists as its own constant so a meter-type call site
/// can name the base in meter-type terms rather than reaching for the
/// resource constant directly.
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
    /// rather than surfacing a generic GTS parse error. The control-character
    /// exclusion is load-bearing, not cosmetic: ADR-0007's entry-identifier
    /// derivation concatenates this value with the other dedup-identity
    /// inputs under a `0x1F` separator, and a control character inside it
    /// would let two distinct dedup identities collapse to the same
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

// ---------------------------------------------------------------------------
// Usage-record exchange types
// ---------------------------------------------------------------------------

/// Wire name of the covered period's inclusive start bound.
///
/// The two bound names are wire vocabulary rather than Rust field names:
/// they appear in a validation error's `field` (so a caller can map the
/// violation back to what they sent), and the read surface reserves and
/// orders on them. Naming them once keeps a typo at one of those sites from
/// silently splitting the vocabulary in two — the same reason the host
/// crate's query module keeps its own predicate field name in a constant.
pub const WINDOW_START_FIELD: &str = "window_start";

/// Wire name of the covered period's exclusive end bound. See
/// [`WINDOW_START_FIELD`] for why both are constants.
pub const WINDOW_END_FIELD: &str = "window_end";

/// Wire name of the record identifier. A constant for the same reason as
/// the two bound names: the gateway's canonical keyset is spelled from
/// these three, and a keyset half-spelled from constants and half from
/// string literals is the shape where one half gets repointed and the
/// other does not.
pub const RECORD_ID_FIELD: &str = "id";

/// Lifecycle status of a stored [`UsageRecord`]. Defaults to `Active`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum UsageRecordStatus {
    /// Live, counts toward aggregates, may be referenced by a compensation.
    #[default]
    Active,
    /// Removed from aggregates by an atomic depth-1 cascade; compensations
    /// referencing this row are rejected per the L1 `corrects_id` rule.
    Inactive,
}

/// Single usage record. The persisted shape is the canonical return value
/// of every create surface (new insert or silent idempotency replay).
// @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-entity-usage-record:p1
// @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-entity-idempotency-key:p1
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageRecord {
    /// Deterministic gateway-derived entry identity: `UUIDv5` of the 5-tuple
    /// dedup identity
    /// `(tenant_id, gts_type_id, idempotency_key, window_start, window_end)`
    /// (see [`crate::derive_usage_record_id`];
    /// `cpt-cf-usage-collector-adr-record-identity-derivation`). Stamped by
    /// [`CreateUsageRecord::try_into_usage_record`] on create and
    /// authoritative on read / return. The identity cannot be
    /// caller-supplied: the create surface takes the identity-free
    /// [`CreateUsageRecord`], not this type.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_ref: Option<SubjectRef>,
    /// Caller-supplied metadata. Keys are validated [`MetadataKey`]s and
    /// values are typed as `String` end-to-end; closed-shape membership
    /// against the usage type's `metadata_fields` and the operator-configured
    /// size cap are enforced at the gateway before plugin dispatch. Omitted
    /// from the wire when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<MetadataKey, String>,
    /// Signed numeric measurement value, carried as a fixed-precision
    /// [`rust_decimal::Decimal`] on every surface (SDK, REST, plugin SPI)
    /// and persisted as Postgres `NUMERIC`. The wire encoding is a JSON
    /// string (`"42.5"`) — never a JSON number — so client/server number
    /// representations cannot round-trip through float and silently lose
    /// precision. The permitted sign is jointly governed by the meter's
    /// counter/gauge semantics (resolved via `types-registry`, not carried by
    /// this SDK) and the presence of `corrects_id` per the four-cell value
    /// matrix.
    #[serde(with = "rust_decimal::serde::str")]
    pub value: Decimal,
    /// Mandatory caller-supplied key for at-least-once-with-dedup
    /// semantics. One of the five inputs to the dedup identity, so a single
    /// stable per-meter key covers many periods without collapsing them
    /// onto one entry.
    pub idempotency_key: IdempotencyKey,
    /// When set, marks this row as a counter compensation referencing a
    /// previously emitted ordinary usage row. The four-cell value matrix
    /// and the L1 referential rule are enforced at the gateway before
    /// plugin dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corrects_id: Option<Uuid>,
    /// Record lifecycle status.
    #[serde(default)]
    pub status: UsageRecordStatus,
    /// Inclusive start of the emitter-supplied covered period (RFC 3339
    /// on the wire). The covered period is the only emitter-supplied time
    /// attribution an entry carries. Persisted at microsecond precision,
    /// UTC-normalized by
    /// [`CreateUsageRecord::try_into_usage_record`].
    #[serde(with = "time::serde::rfc3339")]
    pub window_start: time::OffsetDateTime,
    /// Exclusive end of the covered period. At or after
    /// [`Self::window_start`]; equal bounds mark a point event, not an
    /// error.
    ///
    /// This is the bound the read contract selects on —
    /// `from <= window_end < to`, whatever the length of the period
    /// (`cpt-cf-usage-collector-adr-window-end-selection`, the reference
    /// spelling being [`crate::TimeRange::contains_window_end`]). Reading
    /// the end alone is what makes adjacent ranges sum without double
    /// counting. Both read paths take that range as a typed parameter and
    /// select on this bound, and every raw-path page order names it — so
    /// the column the range selects on is also a sort column, and with no
    /// caller `$orderby` it is the leading one, letting a single index
    /// serve both.
    #[serde(with = "time::serde::rfc3339")]
    pub window_end: time::OffsetDateTime,
}

/// Identity-free create submission — the input to every create surface
/// ([`crate::UsageCollectorClientV1::create_usage_record`] /
/// [`crate::UsageCollectorClientV1::create_usage_records`]).
///
/// This mirrors [`UsageRecord`] minus the two fields a caller cannot own on
/// create: `id` (a deterministic projection of the 5-tuple dedup identity —
/// see [`Self::try_into_usage_record`]) and `status` (always
/// [`UsageRecordStatus::Active`]
/// on a fresh insert). Encoding "id is derived, not supplied" in the type —
/// rather than a doc-comment on a full [`UsageRecord`] — is what keeps a
/// caller from constructing a meaningless identity the gateway would only
/// discard. The wire REST surface encodes the same shape as
/// `CreateUsageRecordRequest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateUsageRecord {
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_ref: Option<SubjectRef>,
    /// Caller-supplied metadata. Same validation and closed-shape rules as
    /// [`UsageRecord::metadata`]. Omitted from the wire when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<MetadataKey, String>,
    /// Signed numeric measurement value. Same encoding and sign governance as
    /// [`UsageRecord::value`].
    #[serde(with = "rust_decimal::serde::str")]
    pub value: Decimal,
    /// Mandatory caller-supplied key for at-least-once-with-dedup semantics.
    pub idempotency_key: IdempotencyKey,
    /// When set, marks this submission as a counter compensation referencing a
    /// previously emitted ordinary usage row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corrects_id: Option<Uuid>,
    /// Inclusive start of the covered period this submission measures (RFC
    /// 3339 on the wire; the codec requires an offset, so an
    /// offset-less timestamp never reaches the projection). Part of the
    /// dedup identity, so it feeds the derived `id`.
    #[serde(with = "time::serde::rfc3339")]
    pub window_start: time::OffsetDateTime,
    /// Exclusive end of the covered period. Must be at or after
    /// [`Self::window_start`]; equal bounds submit a point event. Part of
    /// the dedup identity, so it feeds the derived `id` — which is why an
    /// emitter that recomputes its bounds on retry derives a different
    /// identifier, and why deterministic bounds are an emitter obligation.
    #[serde(with = "time::serde::rfc3339")]
    pub window_end: time::OffsetDateTime,
}

impl CreateUsageRecord {
    /// Projects this submission into the persisted [`UsageRecord`] shape,
    /// validating the covered period first.
    ///
    /// This is the single point at which a submission acquires its identity,
    /// and the validation is inseparable from it:
    /// `cpt-cf-usage-collector-adr-record-identity-derivation` requires both
    /// period preconditions to be rejected **before** the derivation runs,
    /// so the projection is fallible rather than the caller's obligation.
    ///
    /// In order: both bounds are normalized to UTC, each must then carry at
    /// most microsecond precision, the period must be ordered
    /// (`window_start <= window_end`, equal bounds being a point event), and
    /// only then is `id` derived over
    /// `(tenant_id, gts_type_id, idempotency_key, window_start, window_end)`.
    /// `status` is initialized to [`UsageRecordStatus::Active`] and every
    /// other field is forwarded verbatim. Because the identity is a pure
    /// projection of caller-supplied fields it cannot be supplied
    /// independently — which is exactly why the create surface takes this
    /// identity-free type rather than a full [`UsageRecord`].
    ///
    /// Neither precondition truncates. A truncated bound would be persisted
    /// under an `id` derived from the truncated value while the emitter
    /// reproduces the id it submitted, so the two would disagree about the
    /// same entry.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorError::InvalidArgument`] when a bound is
    /// finer than microsecond precision, or when the period is inverted.
    pub fn try_into_usage_record(self) -> Result<UsageRecord, UsageCollectorError> {
        // Normalization runs before the preconditions, not after, and that
        // ordering is safe rather than merely convenient: `UtcOffset` holds
        // whole seconds, so the nanosecond component is invariant under the
        // conversion and moving it earlier cannot change what the precision
        // check accepts. What it does change is the diagnostics — both
        // rejections below now echo the bound in one rendering instead of
        // one echoing the caller's offset and the other UTC — and it keeps
        // an offset carrying non-zero seconds out of the RFC 3339 formatter
        // in `crate::error`, which cannot render one.
        let window_start = self.window_start.to_offset(time::UtcOffset::UTC);
        let window_end = self.window_end.to_offset(time::UtcOffset::UTC);

        require_microsecond_precision(WINDOW_START_FIELD, window_start)?;
        require_microsecond_precision(WINDOW_END_FIELD, window_end)?;

        if window_end < window_start {
            return Err(UsageCollectorError::inverted_covered_period(
                window_start,
                window_end,
            ));
        }

        let id = crate::id::derive_usage_record_id(
            self.tenant_id,
            &self.gts_type_id,
            &self.idempotency_key,
            window_start,
            window_end,
        );

        Ok(UsageRecord {
            id,
            gts_type_id: self.gts_type_id,
            tenant_id: self.tenant_id,
            resource_ref: self.resource_ref,
            subject_ref: self.subject_ref,
            metadata: self.metadata,
            value: self.value,
            idempotency_key: self.idempotency_key,
            corrects_id: self.corrects_id,
            status: UsageRecordStatus::Active,
            window_start,
            window_end,
        })
    }
}

/// Rejects a covered-period bound carrying finer than microsecond
/// precision.
///
/// The microsecond is the precision ceiling of the identity derivation
/// (`cpt-cf-usage-collector-adr-record-identity-derivation` fixes the
/// canonical bound form as a six-digit fraction), so a finer value has no
/// representation there. [`crate::canonical_period_bound`] is the other
/// half of that ceiling — it renders the six digits, and it truncates
/// rather than rejects — so the two must agree on where the ceiling is;
/// there is no shared constant to hold them together, only this pair of
/// references. A leap second arrives here as the same failure:
/// `time`'s RFC 3339 parser renders a `:60` second at a valid stand-in
/// position as `59.999999999`, and [`time::Time`] cannot represent second
/// 60 at all, so this one check is where the ADR's leap-second rejection
/// lands. There is no separate branch for it.
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

// ---------------------------------------------------------------------------
// Declared aggregation fold
// ---------------------------------------------------------------------------

/// The single aggregation a meter declares, read from its GTS type
/// declaration's `x-gts-traits.aggregation_fold`.
///
/// This is never a request parameter. The aggregate path serves the declared
/// fold and no other, so no class of request is well-formed and semantically
/// wrong. The set is closed: adding a fold is an additive change, removing one
/// is breaking.
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
    /// by the greatest `acceptance_sequence`.
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
/// stream. `Metadata(String)` carries a single declared metadata key
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
    /// [`bigdecimal::BigDecimal`] so `SUM`, `MIN`, `MAX`, and `COUNT` are exact
    /// at any magnitude and compensation rows net to zero. Postgres `NUMERIC`
    /// is unbounded, and a wide `SUM` (or large-magnitude `AVG`) can exceed
    /// [`rust_decimal::Decimal`]'s ~7.9×10²⁸ ceiling — which previously
    /// surfaced as an `Internal` (HTTP 500) on decode. `AVG` is now exact in
    /// magnitude but may still carry a backend/plugin-chosen rounding scale on
    /// non-terminating quotients (arbitrary precision is still finite). `None`
    /// when no rows matched the bucket (e.g. `MIN` over an empty set).
    /// Wire-encoded as a JSON string (never a float) for the same round-trip
    /// reason as [`UsageRecord::value`], via
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
/// *scans*; this caps the distinct *groups* it produces. A high-cardinality
/// [`AggregationDimension::Metadata`] key (e.g. a per-record id) could otherwise
/// materialize an unbounded bucket set into memory, unlike the page-size-clamped
/// list path. The storage plugin MUST bound its own result (e.g. append
/// `LIMIT MAX_AGGREGATION_BUCKETS + 1`) so an over-cap query cannot blow up
/// plugin memory; the gateway rejects a result exceeding this cap with a `400`
/// ([`crate::reason::AGGREGATION_RESULT_TOO_LARGE`]). Declared on the wire as the
/// `AggregatedQueryResult.buckets` `maxItems` in `usage-collector-v1.yaml`.
pub const MAX_AGGREGATION_BUCKETS: usize = 100_000;

// ---------------------------------------------------------------------------
// Filter surface for `list_usage_records`
// ---------------------------------------------------------------------------
//
// `UsageRecordQuery` declares the filterable-field schema for the OData
// surface of `list_usage_records`. The struct is never constructed at
// runtime; it exists solely to feed `#[derive(ODataFilterable)]`, which
// generates [`UsageRecordQueryFilterField`] and its
// [`toolkit_odata::filter::FilterField`] impl. Plugin implementations supply
// a `FieldToColumn<UsageRecordFilterField>` mapper next to their entity
// definition; the SDK does not encode storage-layer column mapping.
//
// `gts_type_id` is intentionally absent from this schema. It is carried as a
// typed parameter on `list_usage_records` /
// `query_aggregated_usage_records`; omitting it here means
// `parse_odata_filter::<UsageRecordFilterField>` rejects any
// `gts_type_id`-touching predicate at parse time as
// `FilterError::UnknownField`, so neither plugins nor the gateway need a
// runtime reject path.
//
// The covered-period bounds and `id` ARE on the schema, and none of the
// three is reachable from a `$filter`. That is not a contradiction: this
// schema is two vocabularies at once — the plugin's field-to-column
// mapping and the `$orderby` surface. `window_end` has to be nameable for
// the canonical `(window_end, id)` keyset, and for a cursor's signed
// tokens, to resolve to a column at all; `id` is that keyset's final
// tiebreaker. What keeps a predicate off the bounds is the host crate's
// `reject_reserved_filter_fields` guard, not their absence from here: the
// read range travels as a typed `TimeRange` parameter, so a `$filter`
// conjunct naming a bound would be a second, possibly contradictory,
// constraint on something the range already fixes.
//
// Nested attribution composites (`resource_ref`, `subject_ref`) are
// flattened to their leaf identifiers (`resource_id`, `resource_type`,
// `subject_id`, `subject_type`) so filtering goes through the macro-derived
// path rather than a hand-rolled slash-path `FilterField` impl.
//
// `status` is declared `String` on the filter wire (`"active"` /
// `"inactive"`); plugins translate to their storage representation via
// `FieldToColumn::map_value`.
//
// `metadata` filtering does not flow through OData — see
// [`MetadataFilter`] below, supplied as a separate parameter on
// `list_usage_records`. Postgres has no general `serde_json::Value` filter
// surface in `toolkit-odata`, and there is no precedent for one in the
// workspace.

/// Filterable-field schema for `list_usage_records`'s `ODataQuery` argument.
///
/// Never constructed at runtime. The `dead_code` allow is intentional —
/// the struct is a derive-only artifact (see file-level comment above for
/// rationale).
#[derive(ODataFilterable)]
#[allow(dead_code)]
pub struct UsageRecordQuery {
    /// `usage_records.id` (record primary key). Carried on the filter
    /// surface so the gateway can use it as the canonical cursor
    /// tiebreaker (`(window_end, id)`) and so callers can pin a
    /// specific record via `$filter`.
    #[odata(filter(kind = "Uuid"))]
    pub id: Uuid,
    /// `usage_records.window_start` — the inclusive start of the covered
    /// period. Here so a plugin has a column mapping for it and a caller
    /// can order by it; **not** filterable, because the read range travels
    /// as a typed parameter and `reject_reserved_filter_fields` rejects any
    /// predicate naming it.
    #[odata(filter(kind = "DateTimeUtc"))]
    pub window_start: time::OffsetDateTime,
    /// `usage_records.window_end` — the exclusive end of the covered
    /// period, and the column every read path selects on
    /// (`from <= window_end < to`, per
    /// `cpt-cf-usage-collector-adr-window-end-selection`). Every raw-path
    /// page order names it — see [`is_keyset_safe_record_field`] — so
    /// selection and sorting share a column, and with no caller `$orderby`
    /// it leads the order and a single index serves both. Reserved on the
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
    /// `usage_records.corrects_id` (compensation target). Supports `eq`
    /// and `in`.
    #[odata(filter(kind = "Uuid"))]
    pub corrects_id: Uuid,
    /// `usage_records.status` lifecycle (`"active"` / `"inactive"`). Plugins
    /// translate to the storage representation via
    /// `FieldToColumn::map_value`.
    #[odata(filter(kind = "String"))]
    pub status: String,
}

pub use UsageRecordQueryFilterField as UsageRecordFilterField;

/// The record attributes that are never absent, and therefore sound as
/// keyset-pagination ordering keys — the closed set
/// [`is_keyset_safe_record_field`] tests against.
///
/// Exported because it is the admissible `$orderby` vocabulary, so a `400`
/// refusing a caller's order can name the whole set rather than leave them
/// to guess: seven names is short enough to be actionable, and the set is
/// closed. Matching is exact, like the `$orderby` grammar itself.
pub const KEYSET_SAFE_RECORD_FIELDS: &[&str] = &[
    RECORD_ID_FIELD,
    WINDOW_START_FIELD,
    WINDOW_END_FIELD,
    "tenant_id",
    "resource_id",
    "resource_type",
    "status",
];

/// Record filter fields backed by a **mandatory (never-null)** attribute, and
/// therefore sound to use as a keyset-pagination ordering key.
///
/// The storage plugin's keyset continuation is a row-value tuple comparison
/// (`(c1, c2, …) > ($…)`). In SQL three-valued logic a tuple whose leading
/// column is NULL compares as NULL, so every NULL-keyed row is silently
/// dropped from the paged result — and a page ending on such a row cannot
/// encode a `next_cursor` at all (a 500). A field is keyset-safe **iff** its
/// backing [`UsageRecord`] attribute is never absent:
///
/// - `subject_id` / `subject_type` come from `subject_ref: Option<SubjectRef>`
///   and `corrects_id` is `Option<Uuid>` — all three are domain-optional, so
///   they are **not** keyset-safe.
/// - every entry of [`KEYSET_SAFE_RECORD_FIELDS`] is mandatory on every
///   record, so all of them are keyset-safe.
///
/// This is a domain-optionality fact (an SDK concern), not a storage-column
/// fact — the gateway rejects a caller `$orderby` on a non-keyset-safe field
/// with `400`, and the plugin enforces the same invariant fail-closed. The
/// allowlist is deliberately fail-closed: an unknown or newly added field is
/// unsafe until it is classified there.
#[must_use]
pub fn is_keyset_safe_record_field(name: &str) -> bool {
    KEYSET_SAFE_RECORD_FIELDS.contains(&name)
}

/// Equality-set filter applied to a single [`UsageRecord::metadata`] key.
///
/// `metadata` is a `BTreeMap<MetadataKey, String>` whose keys are not part
/// of any static schema; the `OData` filter surface in `toolkit-odata`
/// cannot express filtering on dynamic map keys. `MetadataFilter` is the
/// typed side-channel used by
/// [`crate::UsageCollectorClientV1::list_usage_records`] and the plugin
/// SPI to filter on those keys.
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
