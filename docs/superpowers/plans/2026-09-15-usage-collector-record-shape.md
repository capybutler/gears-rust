# Usage Collector — Record and Ingestion Wire Shape (Slice A) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring the usage-collector's record and ingestion wire shape into
line with the reworked spec:
- `quantity` as a validated `UsageQuantity`;
- a server-stamped `accepted_at`;
- the `inv:` idempotency-key rules;
- attribution string caps counted in characters;
- a configurable batch cap;
- REST batches decoded one entry at a time.

**Architecture:** The rules live in the SDK (`usage-collector-sdk`), so the
REST surface and in-process callers share one implementation:
- the newtypes validate at construction;
- `CreateUsageRecord::try_into_usage_record` enforces key/invalidation
  exclusivity and stamps `accepted_at`.

The gateway stamps one microsecond-precision instant per request, reads its
batch cap from config, and decodes REST entries as raw JSON after checking the
cap. The TimescaleDB plugin renames and adds columns by editing `0001` in
place. The contract harness follows the model.

**Tech Stack:** Rust 2024, `cargo nextest`, `clippy::pedantic` (deny),
`rust_decimal` 1.41, `time`, `serde`/`serde_json`, `sqlx` + TimescaleDB
(Docker for pg tests), `utoipa` via `toolkit_macros::api_dto`, pytest e2e.

**Spec:** `docs/superpowers/specs/2026-09-15-usage-collector-record-shape-design.md`.
Background: `SPEC-DIFF.md` items 1.1–1.4, 1.7, 1.8 and 4.1.

## Global Constraints

- **Sources of truth:**
  - `gears/system/usage-collector/docs/{DESIGN.md,PRD.md,usage-collector-v1.yaml,ADR/*,schemas/*}`;
  - the `SPEC-DIFF.md` **Decisions** table, which wins where it disagrees
    with the docs.

  Do NOT edit those spec docs. `DECOMPOSITION.md` and `docs/features/*` are
  stale; do not use them.
- **Out of scope, do not touch:**
  - pipeline order, PDP before validation (slice C);
  - removing `origin` from dedup comparisons (slice B);
  - `AlreadyInvalidated` or plugin error changes (slice B);
  - metric label changes (slice G);
  - adding or removing contract checks (slice E);
  - aggregate request body and page size (slice D).
- **Quantity rules:**
  - Accepted text matches `^-?(?:0|[1-9][0-9]{0,27})(?:\.[0-9]{1,28})?$` and
    has at most **28 significant digits**. Leading zeros are excluded from the
    count, across the decimal point; trailing zeros count.
  - **Negative zero** (`-0`, `-0.00`) is rejected.
  - A violation gives reason `QUANTITY_OUT_OF_RANGE` on field `quantity`.
  - `PartialEq` stays numeric (derived over `Decimal`).
- **Key rules:**
  - A record requires a caller key; it is rejected if missing, with
    `VALIDATION` on `idempotency_key`.
  - A caller key beginning `inv:` is rejected with `RESERVED_KEY_PREFIX`.
  - An invalidation carrying a key is rejected with `KEY_ON_INVALIDATION`.
  - An invalidation's stored key is `inv:` followed by the target id,
    lowercase and hyphenated.
- **Length caps, counted in characters:**
  - `resource_id`, `resource_type`, `subject_id`, `subject_type`: at most 256,
    with the violation on field `resource_ref.resource_id` etc.;
  - `IdempotencyKey`: at most 256;
  - `ReasonCode`: at most 128.
- **`accepted_at`:** one instant per request, truncated to microseconds, and
  the same instant used for covered-period bounds.
- **Batch cap:** config key `max_batch_records`, default 100, must be
  greater than 0.
- **Commits:**
  - every commit compiles, passes clippy, and passes the unit tests of the
    four crates;
  - signed with `git commit -s`;
  - a wire-breaking commit uses `!` plus a `BREAKING CHANGE:` trailer.
- **Package names:** `cf-gears-usage-collector-sdk`, `cf-gears-usage-collector`,
  `cf-gears-noop-usage-collector-plugin`, `cf-gears-timescaledb-usage-collector-plugin`.
- **Unit test command** (used as `$UNIT` below):
  `cargo nextest run -p cf-gears-usage-collector-sdk --features contract -p cf-gears-usage-collector -p cf-gears-noop-usage-collector-plugin -p cf-gears-timescaledb-usage-collector-plugin`.
  If `--features contract` is rejected in a multi-package invocation, run the
  SDK separately with `cargo nextest run -p cf-gears-usage-collector-sdk --all-features`.
- **Lint command** (`$LINT`):
  `cargo clippy -p cf-gears-usage-collector-sdk -p cf-gears-usage-collector -p cf-gears-noop-usage-collector-plugin -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings`
  plus `cargo fmt --all`.
- **PG test command** (`$PG`): `make test-usage-collector-pg`. Needs Docker.
  If Docker is unavailable, say so explicitly in the task report; never
  claim it passed.

Paths below are relative to `gears/system/usage-collector/` unless they start
with `docs/`, `testing/` or `libs/`.

---

## File Structure

| File | Responsibility | Tasks |
| --- | --- | --- |
| `usage-collector-sdk/src/quantity.rs` (new) + `quantity_tests.rs` (new) | `UsageQuantity` newtype: parse, display, serde | 1 |
| `usage-collector-sdk/src/reason.rs`, `reason_tests.rs` | Three new validation reasons | 1 |
| `usage-collector-sdk/src/error.rs` | New `InvalidArgument` constructors | 1, 2, 5 |
| `usage-collector-sdk/src/lib.rs` | Export `UsageQuantity` | 1 |
| `usage-collector-sdk/src/models.rs`, `models_tests.rs` | Length caps, `quantity`, `accepted_at`, key model, projection | 2–5 |
| `usage-collector-sdk/src/id.rs`, `id_tests.rs` | Derivation doc, `inv:` golden vectors | 5 |
| `usage-collector-sdk/src/contract/{fixtures.rs,reference.rs,checks/*.rs}`, `contract_mutants.rs`, `contract_tests.rs` | Harness follows the model | 3–5 |
| `usage-collector/src/api/rest/dto.rs`, `dto_tests.rs` | Wire DTOs | 3, 4, 5, 6 |
| `usage-collector/src/api/rest/handlers/usage_records.rs`, `usage_records_tests.rs` | Per-entry decode, cap from service, key mapping | 3, 5, 6 |
| `usage-collector/src/domain/service.rs`, `service_tests.rs`, `service_metrics_tests.rs`, `test_support.rs` | Stamp, cap, projection call | 3, 4, 6 |
| `usage-collector/src/domain/invalidation.rs`, `invalidation_tests.rs` | Faithful-copy compares `quantity` | 3, 5 |
| `usage-collector/src/config.rs`, `config_tests.rs`, `module.rs` | `max_batch_records` | 6 |
| `usage-collector/src/infra/metrics.rs`, `metrics_tests.rs` | Batch-size buckets from cap | 6 |
| `plugins/timescaledb-usage-collector-plugin/migrations/{0001_init.sql,0002_usage_rollup.sql}` | Columns `quantity`, `accepted_at` | 3, 4 |
| `plugins/timescaledb-usage-collector-plugin/src/infra/storage/{entity.rs,mapper.rs,record_store.rs,migration_probe.rs,query/*.rs}` and tests | Column mapping | 3, 4, 5 |
| `plugins/noop-usage-collector-plugin/src/plugin_tests.rs` | Renames | 3, 4, 5 |
| `testing/e2e/suites/usage_collector/{conftest.py,test_integration_seams.py}` | Wire payloads | 3, 5 |
| `docs/api/api.json` (repo root) | Regenerated OpenAPI | 7 |

---

### Task 1: `UsageQuantity` and the three new reasons

**Files:**
- Create: `usage-collector-sdk/src/quantity.rs`
- Create: `usage-collector-sdk/src/quantity_tests.rs`
- Modify: `usage-collector-sdk/src/lib.rs`
- Modify: `usage-collector-sdk/src/reason.rs`, `usage-collector-sdk/src/reason_tests.rs`
- Modify: `usage-collector-sdk/src/error.rs` (the `// ── InvalidArgument (400)` section)

**Interfaces:**
- Produces:
  - `pub struct UsageQuantity(Decimal)`, which is `Copy`, `Eq` and `Hash`.
  - `UsageQuantity::parse(&str) -> Result<UsageQuantity, UsageCollectorError>`.
  - `UsageQuantity::as_decimal(&self) -> Decimal`.
  - Trait impls: `FromStr`, `TryFrom<&str>`, `TryFrom<Decimal>`, `Display`, `Serialize`/`Deserialize` (string only).
  - `pub const MAX_QUANTITY_SIGNIFICANT_DIGITS: usize = 28`.
  - `UsageCollectorError::quantity_out_of_range(detail: impl Into<String>) -> Self`.
  - `reason::{QUANTITY_OUT_OF_RANGE, RESERVED_KEY_PREFIX, KEY_ON_INVALIDATION}`.
  - `ValidationReason::{QuantityOutOfRange, ReservedKeyPrefix, KeyOnInvalidation}`.

- [ ] **Step 1: Write the failing tests** in `usage-collector-sdk/src/quantity_tests.rs`

```rust
use std::str::FromStr;

use rust_decimal::Decimal;
use serde_json::json;

use super::UsageQuantity;
use crate::error::UsageCollectorError;
use crate::reason::ValidationReason;

fn assert_out_of_range(text: &str) {
    let err = UsageQuantity::parse(text).expect_err(text);
    assert!(
        matches!(
            &err,
            UsageCollectorError::InvalidArgument { field, reason: ValidationReason::QuantityOutOfRange, .. }
                if field == "quantity"
        ),
        "`{text}` must be QUANTITY_OUT_OF_RANGE on `quantity`, got {err:?}",
    );
}

#[test]
fn accepted_forms_render_back_digit_for_digit() {
    for text in [
        "0",
        "0.000",
        "42",
        "42.500",
        "-42.5",
        "9999999999999999999999999999",
        "-9999999999999999999999999999",
        "0.0000000000000000000000000001",
        "-0.0000000000000000000000000001",
        "1.234567890123456789012345678",
        "12345678901234567890.12345678",
    ] {
        let quantity = UsageQuantity::parse(text).unwrap_or_else(|e| panic!("`{text}`: {e}"));
        assert_eq!(quantity.to_string(), text, "`{text}` must render back unchanged");
    }
}

#[test]
fn forms_outside_the_published_pattern_are_rejected() {
    for text in [
        "", "-", "+1", "1e3", "1E3", "1_000", "01", "-01", "00.5", ".5", "5.", "1.2.3",
        " 1", "1 ", "0x10", "NaN", "inf",
        // 29 integer digits.
        "10000000000000000000000000000",
        // 29 fraction digits.
        "0.00000000000000000000000000001",
    ] {
        assert_out_of_range(text);
    }
}

#[test]
fn more_than_28_significant_digits_is_rejected_even_inside_the_pattern() {
    // 28 integer digits + 1 fraction digit = 29 significant digits.
    assert_out_of_range("9999999999999999999999999999.9");
    // Trailing zeros are significant: 1 followed by 28 zeros after the point.
    assert_out_of_range("1.0000000000000000000000000000");
    // Leading zeros are not: this is one significant digit.
    UsageQuantity::parse("0.0000000000000000000000000009").expect("one significant digit");
    // Exactly 28.
    UsageQuantity::parse("1.234567890123456789012345678").expect("28 significant digits");
}

#[test]
fn negative_zero_is_rejected_because_it_cannot_round_trip() {
    for text in ["-0", "-0.0", "-0.0000000000000000000000000000"] {
        assert_out_of_range(text);
    }
}

#[test]
fn equality_is_numeric_and_as_decimal_exposes_the_value() {
    let a = UsageQuantity::parse("42.5").unwrap();
    let b = UsageQuantity::parse("42.500").unwrap();
    assert_eq!(a, b, "slice A keeps numeric equality; textual equality is slice B");
    assert_eq!(a.as_decimal(), Decimal::from_str("42.5").unwrap());
}

#[test]
fn from_str_and_try_from_route_through_parse() {
    assert_eq!(UsageQuantity::from_str("7").unwrap().to_string(), "7");
    assert_eq!(UsageQuantity::try_from("7.10").unwrap().to_string(), "7.10");
    assert!(UsageQuantity::from_str("-0").is_err());
    assert!(UsageQuantity::try_from("1e3").is_err());
}

#[test]
fn try_from_decimal_preserves_scale_and_validates() {
    let d = Decimal::from_str("42.500").unwrap();
    assert_eq!(UsageQuantity::try_from(d).unwrap().to_string(), "42.500");
    // rust_decimal carries 29 significant digits; the published bound is 28.
    let too_wide = Decimal::from_str("79228162514264337593543950335").unwrap();
    assert!(UsageQuantity::try_from(too_wide).is_err());
}

#[test]
fn serde_is_a_json_string_only() {
    let q = UsageQuantity::parse("42.500").unwrap();
    assert_eq!(serde_json::to_value(q).unwrap(), json!("42.500"));
    assert_eq!(serde_json::from_value::<UsageQuantity>(json!("42.500")).unwrap(), q);
    serde_json::from_value::<UsageQuantity>(json!(42.5)).expect_err("a JSON number is refused");
    serde_json::from_value::<UsageQuantity>(json!("1e3")).expect_err("routes through parse");
}
```

Add to `usage-collector-sdk/src/reason_tests.rs`: put these three tuples inside the array of
`validation_reason_round_trips_each_constant`, after the `PAST_WINDOW` entry:

```rust
        (QUANTITY_OUT_OF_RANGE, ValidationReason::QuantityOutOfRange),
        (RESERVED_KEY_PREFIX, ValidationReason::ReservedKeyPrefix),
        (KEY_ON_INVALIDATION, ValidationReason::KeyOnInvalidation),
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p cf-gears-usage-collector-sdk quantity reason`
Expected: compile FAIL (`quantity` module and `QUANTITY_OUT_OF_RANGE` do not exist).

- [ ] **Step 3: Implement**

`usage-collector-sdk/src/reason.rs`: add after the `PAST_WINDOW` const:

```rust
/// A quantity outside the published `UsageQuantity` range or precision: not
/// matching the wire pattern, more than 28 significant digits, or a negative
/// zero (which no backend can read back digit for digit).
pub const QUANTITY_OUT_OF_RANGE: &str = "QUANTITY_OUT_OF_RANGE";
/// A caller-supplied idempotency key began with `inv:`, the prefix reserved
/// for keys the gateway derives for invalidations.
pub const RESERVED_KEY_PREFIX: &str = "RESERVED_KEY_PREFIX";
/// An invalidation carried an idempotency key. Its key is always derived as
/// `inv:` followed by the target id, so a supplied one is refused.
pub const KEY_ON_INVALIDATION: &str = "KEY_ON_INVALIDATION";
```

Add variants before `Unknown(String)`:

```rust
    /// See [`QUANTITY_OUT_OF_RANGE`].
    QuantityOutOfRange,
    /// See [`RESERVED_KEY_PREFIX`].
    ReservedKeyPrefix,
    /// See [`KEY_ON_INVALIDATION`].
    KeyOnInvalidation,
```

In `from_wire`, before `other =>`:

```rust
            QUANTITY_OUT_OF_RANGE => Self::QuantityOutOfRange,
            RESERVED_KEY_PREFIX => Self::ReservedKeyPrefix,
            KEY_ON_INVALIDATION => Self::KeyOnInvalidation,
```

In `as_wire`, before `Self::Unknown(s)`:

```rust
            Self::QuantityOutOfRange => QUANTITY_OUT_OF_RANGE,
            Self::ReservedKeyPrefix => RESERVED_KEY_PREFIX,
            Self::KeyOnInvalidation => KEY_ON_INVALIDATION,
```

`usage-collector-sdk/src/error.rs`: add directly after `invalid_batch_size`:

```rust
    /// A quantity outside the published `UsageQuantity` range or precision.
    /// `field` is `quantity`.
    #[must_use]
    pub fn quantity_out_of_range(detail: impl Into<String>) -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "quantity".to_owned(),
            reason: ValidationReason::QuantityOutOfRange,
            detail: detail.into(),
        }
    }
```

Create `usage-collector-sdk/src/quantity.rs`:

```rust
//! The quantity an entry carries.
//!
//! [`UsageQuantity`] is the published `UsageQuantity` range made into a type:
//! a finite signed decimal, wire-encoded as a JSON string, with at most 28
//! significant digits and 28 fraction digits
//! (`cpt-cf-usage-collector-adr-quantity-precision`). The carrier,
//! `rust_decimal::Decimal`, reaches 29 significant digits, so the type rather
//! than the carrier holds the bound.
//!
//! Parsing is exact. The scale the caller sent is kept, so `42.500` renders
//! back as `42.500`. Nothing is rounded: a value the range cannot hold is
//! rejected. Negative zero is rejected too. `rust_decimal` renders it as `0`
//! and Postgres `numeric` has no negative zero, so no path could return it
//! digit for digit.
//!
//! Equality is numeric (`42.5 == 42.500`), the carrier's own.

use core::fmt;
use core::str::FromStr;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::UsageCollectorError;

/// The published bound on significant digits, leading zeros excluded.
pub const MAX_QUANTITY_SIGNIFICANT_DIGITS: usize = 28;
/// The pattern's integer-part bound: `0` or `[1-9][0-9]{0,27}`.
const MAX_INTEGER_DIGITS: usize = 28;
/// The pattern's fraction-part bound: `[0-9]{1,28}`.
const MAX_FRACTION_DIGITS: usize = 28;
/// The published wire pattern, quoted in rejections.
const QUANTITY_PATTERN: &str = r"^-?(?:0|[1-9][0-9]{0,27})(?:\.[0-9]{1,28})?$";

/// A validated entry quantity. See the module documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UsageQuantity(Decimal);

impl UsageQuantity {
    /// Parses a quantity from its wire text.
    ///
    /// # Errors
    ///
    /// [`UsageCollectorError::InvalidArgument`] with reason
    /// `QUANTITY_OUT_OF_RANGE` on field `quantity` when the text does not
    /// match the published pattern, carries more than 28 significant digits,
    /// or is a negative zero.
    pub fn parse(text: &str) -> Result<Self, UsageCollectorError> {
        check_text(text).map_err(UsageCollectorError::quantity_out_of_range)?;
        Decimal::from_str_exact(text).map(Self).map_err(|err| {
            UsageCollectorError::quantity_out_of_range(format!(
                "quantity `{text}` is not representable: {err}"
            ))
        })
    }

    /// The numeric value, for storage binds and arithmetic.
    #[must_use]
    pub const fn as_decimal(&self) -> Decimal {
        self.0
    }
}

/// Checks `text` against the pattern, the significant-digit bound and the
/// negative-zero rule, without allocating on success.
fn check_text(text: &str) -> Result<(), String> {
    let pattern_violation = || format!("quantity `{text}` must match {QUANTITY_PATTERN}");
    let (negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (integer, fraction) = match unsigned.split_once('.') {
        Some((integer, fraction)) => (integer, fraction),
        None => (unsigned, ""),
    };
    let all_digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    let integer_ok = !integer.is_empty()
        && integer.len() <= MAX_INTEGER_DIGITS
        && all_digits(integer)
        && (integer == "0" || !integer.starts_with('0'));
    let fraction_ok = !unsigned.contains('.')
        || (!fraction.is_empty() && fraction.len() <= MAX_FRACTION_DIGITS && all_digits(fraction));
    if !integer_ok || !fraction_ok {
        return Err(pattern_violation());
    }
    let significant = integer
        .bytes()
        .chain(fraction.bytes())
        .skip_while(|b| *b == b'0')
        .count();
    if significant > MAX_QUANTITY_SIGNIFICANT_DIGITS {
        return Err(format!(
            "quantity `{text}` has {significant} significant digits; at most \
             {MAX_QUANTITY_SIGNIFICANT_DIGITS} are allowed"
        ));
    }
    if negative && significant == 0 {
        return Err(format!(
            "quantity `{text}` is a negative zero, which cannot be read back digit for \
             digit; send it unsigned"
        ));
    }
    Ok(())
}

impl fmt::Display for UsageQuantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl FromStr for UsageQuantity {
    type Err = UsageCollectorError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<&str> for UsageQuantity {
    type Error = UsageCollectorError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

/// Validates a stored numeric value, for storage plugins decoding a column.
/// Routed through the text form so a stored value is held to the same bound
/// as a submitted one.
impl TryFrom<Decimal> for UsageQuantity {
    type Error = UsageCollectorError;

    fn try_from(value: Decimal) -> Result<Self, Self::Error> {
        Self::parse(&value.to_string())
    }
}

impl Serialize for UsageQuantity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for UsageQuantity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "quantity_tests.rs"]
mod quantity_tests;
```

`usage-collector-sdk/src/lib.rs`: add `pub mod quantity;` after `pub mod plugin_api;`,
and `pub use quantity::{MAX_QUANTITY_SIGNIFICANT_DIGITS, UsageQuantity};` after the
`pub use plugin_api::…` line.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p cf-gears-usage-collector-sdk quantity reason`
Expected: PASS. If `try_from_decimal_preserves_scale_and_validates` fails
because `Decimal::from_str` rejects the 29-digit literal, replace that literal
with `Decimal::MAX`.

- [ ] **Step 5: Lint, run the full unit suite, commit**

Run `$LINT`, then `$UNIT`. Expected: clean, all PASS.

```bash
git add gears/system/usage-collector/usage-collector-sdk/src/{quantity.rs,quantity_tests.rs,lib.rs,reason.rs,reason_tests.rs,error.rs}
git commit -s -m "feat(usage-collector-sdk): add UsageQuantity and the slice-A validation reasons"
```

---

### Task 2: Attribution, key and reason-code length caps in characters

**Files:**
- Modify: `usage-collector-sdk/src/models.rs`: `ResourceRef::new`, `SubjectRef::new`, `IdempotencyKey::new` and its consts/docs, `ReasonCode::new` and its consts/docs
- Modify: `usage-collector-sdk/src/error.rs`
- Test: `usage-collector-sdk/src/models_tests.rs`

**Interfaces:**
- Produces:
  - `UsageCollectorError::attribution_too_long(field: &str, max: usize) -> Self`, which sets `field` to the given dotted path and uses reason `VALIDATION`.
  - `pub const MAX_ATTRIBUTION_LEN: usize = 256` in `models.rs`, private.

- [ ] **Step 1: Write the failing tests.** Append to `models_tests.rs`:

```rust
fn field_of(err: &UsageCollectorError) -> &str {
    match err {
        UsageCollectorError::InvalidArgument { field, .. } => field.as_str(),
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

#[test]
fn attribution_components_are_capped_at_256_characters_not_bytes() {
    // U+00E9 is two bytes: 256 of them are 512 bytes, still 256 characters.
    let at_cap = "\u{e9}".repeat(256);
    let over = "\u{e9}".repeat(257);
    ResourceRef::new(at_cap.clone(), at_cap.clone()).expect("256 characters is the ceiling");
    SubjectRef::new(at_cap.clone(), Some(at_cap.clone())).expect("256 characters is the ceiling");

    let err = ResourceRef::new(over.clone(), "t").expect_err("resource_id over the cap");
    assert_eq!(field_of(&err), "resource_ref.resource_id");
    let err = ResourceRef::new("r", over.clone()).expect_err("resource_type over the cap");
    assert_eq!(field_of(&err), "resource_ref.resource_type");
    let err = SubjectRef::new(over.clone(), None::<String>).expect_err("subject_id over the cap");
    assert_eq!(field_of(&err), "subject_ref.subject_id");
    let err = SubjectRef::new("s", Some(over)).expect_err("subject_type over the cap");
    assert_eq!(field_of(&err), "subject_ref.subject_type");
}

#[test]
fn idempotency_key_and_reason_code_count_characters_not_bytes() {
    IdempotencyKey::new("\u{e9}".repeat(256)).expect("256 characters, 512 bytes");
    IdempotencyKey::new("\u{e9}".repeat(257)).expect_err("257 characters");
    ReasonCode::new("\u{e9}".repeat(128)).expect("128 characters, 256 bytes");
    ReasonCode::new("\u{e9}".repeat(129)).expect_err("129 characters");
}
```

In `reason_code_rejects_the_wire_bounds_and_control_characters`, replace the
`"\u{e9}".repeat(65)` line and the comment block above it with:

```rust
    // The cap counts characters, as the wire schema's `maxLength` does.
    ReasonCode::new("\u{e9}".repeat(129)).expect_err("129 characters");
```

and change the `"x".repeat(128)` expect message to `"128 characters is the boundary, inclusive"`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p cf-gears-usage-collector-sdk models_tests`
Expected: FAIL. The two new tests fail: no cap, byte counting, and wrong field.

- [ ] **Step 3: Implement.** In `error.rs`, after `invalid_reason_code`:

```rust
    /// An attribution component exceeded its character cap. `field` is the
    /// component's dotted path, e.g. `resource_ref.resource_id`.
    #[must_use]
    pub fn attribution_too_long(field: &str, max: usize) -> Self {
        Self::newtype_validation(field, format!("{field} must be at most {max} characters"))
    }
```

In `models.rs`, above `pub struct ResourceRef`:

```rust
/// Character cap on each attribution component, from the `ResourceRef` and
/// `SubjectRef` schemas (`maxLength: 256`) in `docs/usage-collector-v1.yaml`.
const MAX_ATTRIBUTION_LEN: usize = 256;

/// Refuses an attribution component longer than [`MAX_ATTRIBUTION_LEN`]
/// characters, naming its dotted path.
fn cap_attribution(field: &str, value: &str) -> Result<(), UsageCollectorError> {
    if value.chars().count() > MAX_ATTRIBUTION_LEN {
        return Err(UsageCollectorError::attribution_too_long(field, MAX_ATTRIBUTION_LEN));
    }
    Ok(())
}
```

In `ResourceRef::new`, after each NUL check, add
`cap_attribution("resource_ref.resource_id", &resource_id)?;` and
`cap_attribution("resource_ref.resource_type", &resource_type)?;` respectively.

In `SubjectRef::new`, after the `subject_id` NUL check add
`cap_attribution("subject_ref.subject_id", &subject_id)?;`. After the
`subject_type` NUL check add `cap_attribution("subject_ref.subject_type", &s)?;`.

Update both `# Errors` docs to add "or longer than 256 characters".

In `IdempotencyKey::new`, replace `raw.len() > MAX_IDEMPOTENCY_KEY_LEN` with
`raw.chars().count() > MAX_IDEMPOTENCY_KEY_LEN` and the message with
`"idempotency_key must be at most 256 characters"`. Change "bytes" to
"characters" in `MAX_IDEMPOTENCY_KEY_LEN`'s neighbourhood docs: the
`# Validation` list and the `# Errors` text.

In `ReasonCode::new`, the same change with `MAX_REASON_CODE_LEN` and
`"reason_code must be at most 128 characters"`. Replace the doc comment on
`MAX_REASON_CODE_LEN` with:

```rust
/// Character cap on an invalidation reason code, from the wire contract's
/// `ReasonCode` schema (`maxLength: 128`).
```

Change "bytes" to "characters" in `ReasonCode`'s `# Validation` and `# Errors` docs.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p cf-gears-usage-collector-sdk models_tests`
Expected: PASS.

- [ ] **Step 5: Lint, run the full unit suite, commit**

Run `$LINT` and `$UNIT`. Expected: all PASS. If a gateway or plugin test
asserted field `resource_ref` for an over-long value, update it to the dotted
path.

```bash
git add -u gears/system/usage-collector
git commit -s -m "fix(usage-collector-sdk): cap attribution strings and keys in characters"
```

---

### Task 3: Rename `value` → `quantity: UsageQuantity` across the workspace

This commit is wide and mechanical. Work through the file groups below in
order, inside one commit. **The compiler is the completeness check:** keep
running `cargo check --all-targets --all-features` on the four packages until
it is clean. Do NOT rename `AggregationBucket.value`, aggregate-result
`value` fields, the rollup's `sum_value`/`count_value` columns, or any
`serde_json::Value`/`value` local unrelated to the record quantity.

**Files:** SDK `models.rs`, `models_tests.rs`, `error.rs:96` doc,
`contract/{fixtures.rs,reference.rs,checks/*.rs}`, `contract_mutants.rs`,
`contract_tests.rs`. Gateway: `dto.rs`, `dto_tests.rs`,
`handlers/usage_records.rs`, `handlers/usage_records_tests.rs`,
`domain/invalidation.rs`, `invalidation_tests.rs`, `test_support.rs`,
`service_tests.rs`, `service_metrics_tests.rs`, `authz*.rs`, `validation_tests.rs`,
`local_client_tests.rs`, `routes/*_tests.rs`, `infra/*_tests.rs`. Plugins:
TimescaleDB `migrations/0001_init.sql`, `0002_usage_rollup.sql`,
`src/infra/storage/{entity.rs,mapper.rs,record_store.rs,migration_probe.rs,query/aggregate.rs,query/rollup.rs,rollup_maintenance.rs}`
and their tests, `tests/**`. Noop `src/plugin_tests.rs`. E2E
`testing/e2e/suites/usage_collector/{conftest.py,test_integration_seams.py}`.

**Interfaces:**
- Consumes: `UsageQuantity` (Task 1).
- Produces:
  - `UsageRecord.quantity: UsageQuantity` and `CreateUsageRecord.quantity: UsageQuantity`.
  - REST `CreateUsageRecordRequest.quantity: String` and `UsageRecordDto.quantity: String`.
  - TimescaleDB column `quantity numeric NOT NULL`, with `UsageRecordRow.quantity: Decimal`.
  - `invalidation::COMPARED_FIELDS` names `"quantity"`.
  - `test_support::qty(&str) -> UsageQuantity`.
  - Contract `fixture_record(key, quantity: UsageQuantity, ws, we)`.

- [ ] **Step 1: Write the failing tests first**

In `usage-collector/src/api/rest/dto_tests.rs`
`usage_record_dto_serialises_exactly_the_declared_wire_keys`:
- replace `"value",` with `"quantity",` in **both** expected key vectors, and
  re-sort each vector alphabetically, since `quantity` sorts after `origin`;
- replace the comment block (lines starting "This is what the gear emits…")
  with:

```rust
    // This is what the gear emits, checked against `usage-collector-v1.yaml`'s
    // `UsageRecord`.
```

Append this handler test to `usage-collector/src/api/rest/handlers/usage_records_tests.rs`.
Reuse the file's existing `HAPPY_RECORD_GTS_ID`, `recent_window_start/end`
and `service_with_sentinel_pdp` helpers:

```rust
#[tokio::test]
async fn a_quantity_outside_the_published_range_rejects_only_its_entry() {
    let (service, _resolver) = service_with_sentinel_pdp();
    let entry = |quantity: &str, idem: &str| CreateUsageRecordRequest {
        gts_type_id: HAPPY_RECORD_GTS_ID.to_owned(),
        tenant_id: Uuid::from_u128(2),
        resource_ref: ResourceRefDto {
            resource_id: "rsc-q".to_owned(),
            resource_type: "compute.vm".to_owned(),
        },
        subject_ref: None,
        metadata: BTreeMap::new(),
        quantity: quantity.to_owned(),
        idempotency_key: idem.to_owned(),
        invalidates: None,
        reason_code: None,
        window_start: recent_window_start(),
        window_end: recent_window_end(),
    };
    let response = handle_create_usage_records(
        Extension(SecurityContext::anonymous()),
        Extension(service),
        Json(CreateUsageRecordsRequest {
            records: vec![entry("1e3", "idem-q-0")],
        }),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::MULTI_STATUS);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap();
    let violation = &body["results"][0]["error"]["context"]["field_violations"][0];
    assert_eq!(violation["field"], "quantity");
    assert_eq!(violation["reason"], "QUANTITY_OUT_OF_RANGE");
}
```

The `idempotency_key: idem.to_owned()` field becomes `Some(idem.to_owned())`
in Task 5. That task updates this test.

Replace the quantity-reading line in
`usage-collector-sdk/src/contract/checks/quantity_round_trip.rs` `read_back`:
`.map(|item| item.value.to_string())` becomes `.map(|item| item.quantity.to_string())`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo check -p cf-gears-usage-collector --all-targets`
Expected: FAIL (no field `quantity`).

- [ ] **Step 3a: SDK model** (`usage-collector-sdk/src/models.rs`)

- On `UsageRecord` and `CreateUsageRecord`, replace the `value: Decimal` field
  with `quantity: UsageQuantity`. The `UsageRecord` doc becomes:

```rust
    /// The measured quantity, in the canonical unit of the entry's GTS type.
    /// Validated against the published range at construction and carried on
    /// the wire as a JSON string (see [`crate::UsageQuantity`]). The sign is
    /// never constrained and carries no structural meaning: a negative
    /// quantity records real consumption, and an invalidation echoes the
    /// quantity it withdraws rather than negating it
    /// (`cpt-cf-usage-collector-adr-append-only-invalidation`).
    ///
    /// The wire encoding (RFC 3339 for the covered-period bounds, and which
    /// fields are omitted when empty) is declared on this type's two shadow
    /// structs in the wire-codec section, because the type serializes
    /// through them.
    pub quantity: UsageQuantity,
```

  The `CreateUsageRecord` doc becomes `/// The measured quantity. Same encoding and sign rules as [`UsageRecord::quantity`].`
- In all four shadows (`UsageRecordWire`, `UsageRecordWireRef`,
  `CreateUsageRecordWire`, `CreateUsageRecordWireRef`), replace
  `#[serde(with = "rust_decimal::serde::str")] value: Decimal,` with
  `quantity: UsageQuantity,`. Rename `value` to `quantity` in the four
  destructures and struct literals. In the two `Serialize` impls, use
  `quantity: *quantity`.
- `try_into_usage_record`: `value: self.value` becomes `quantity: self.quantity`.
- Add `use crate::quantity::UsageQuantity;`. Remove the `use rust_decimal::Decimal;`
  import only if the compiler reports it unused.
- `error.rs` `InvalidArgument.field` doc: `(`value`, `records`, `metadata`, …)`
  becomes `(`quantity`, `records`, `metadata`, …)`.
- In `models_tests.rs`, add `fn qty(s: &str) -> crate::UsageQuantity { crate::UsageQuantity::parse(s).expect("test quantity") }`
  and replace every record/submission `value: Decimal::from(N)` with
  `quantity: qty("N")`. Replace any `Decimal::from_str("X")` used as a record
  quantity with `qty("X")`. Leave `AggregationBucket` tests unchanged.

- [ ] **Step 3b: SDK contract harness**

`contract/fixtures.rs`:
- `fixture_record`, `fixture_record_for_tenant` and `fixture_invalidation`
  take `quantity: UsageQuantity` instead of `value: Decimal`;
- the struct literal uses `quantity`;
- the doc's "the quantity is what `quantity-round-trip` probes" stays.

`quantity_round_trip.rs`: build each corner's fixture from
`UsageQuantity::parse(corner)`. On `Err`, return the check's existing
"harness fault" string, formatted as
`format!("the check's own corner `{corner}` is not a valid quantity: {err}")`.
Update every other check in `contract/checks/*.rs`, plus `contract_mutants.rs`
and `contract_tests.rs`: replace `Decimal::from(N)` passed as a fixture
quantity with `UsageQuantity::parse("N").expect("fixture quantity")` in test
code. In non-test check code, use `map_err` into the check's `String` error.

`reference.rs`: rename any `.value` read on a record to `.quantity`. Where it
sums quantities for a fold, use `.quantity.as_decimal()`.

- [ ] **Step 3c: Gateway**

`dto.rs` `CreateUsageRecordRequest`: replace the `value` field with:

```rust
    /// The measured quantity as its wire text (a JSON string). Parsed into
    /// [`usage_collector_sdk::UsageQuantity`] where the DTO is folded into the
    /// domain type, so an out-of-range value rejects its own entry rather
    /// than the whole batch.
    pub quantity: String,
```

`UsageRecordDto`: replace the `value` field with:

```rust
    /// The persisted quantity as a JSON string, digit for digit as stored.
    pub quantity: String,
```

In `From<UsageRecord> for UsageRecordDto`, `value: value.value` becomes
`quantity: value.quantity.to_string()`. Remove the unused `Decimal` import.

`handlers/usage_records.rs` `record_request_into_domain`: after the
`subject_ref` block add

```rust
    let quantity = UsageQuantity::parse(&req.quantity)
        .map_err(|err| Problem::from(usage_collector_error_to_canonical(err)))?;
```

and in the struct literal replace `value: req.value` with `quantity`. Add
`UsageQuantity` to the `usage_collector_sdk::{…}` import.

`domain/invalidation.rs`:
- in `COMPARED_FIELDS`, replace `"value"` with `"quantity"`;
- in both destructures, `value` becomes `quantity` and `value: target_value`
  becomes `quantity: target_quantity`;
- the departure row becomes `("quantity", quantity != target_quantity)`;
- in the comment block, replace "`value` is compared for equality" with
  "`quantity` is compared for equality";
- replace "That equality is `rust_decimal`'s" with "That equality is
  `UsageQuantity`'s, derived from `rust_decimal`'s";
- replace "a `value` mismatch against a value it already echoed" with "a
  `quantity` mismatch against a quantity it already echoed".

`domain/test_support.rs`: add

```rust
/// A test quantity. Panics on an invalid literal, which is a test bug.
pub(crate) fn qty(text: &str) -> usage_collector_sdk::UsageQuantity {
    usage_collector_sdk::UsageQuantity::parse(text).expect("test quantity literal")
}
```

Then, across every gateway test file the compiler names:
- `value: rust_decimal::Decimal::from(N)` / `value: Decimal::from(N)` on SDK
  types becomes `quantity: qty("N")`;
- on the DTO it becomes `quantity: "N".to_owned()`;
- `record.value` becomes `record.quantity`;
- JSON literals `"value": "N"` in request bodies become `"quantity": "N"`.

Import `crate::domain::test_support::qty` where needed.

- [ ] **Step 3d: TimescaleDB plugin**

`migrations/0001_init.sql`: `value               numeric     NOT NULL,` becomes
`quantity            numeric     NOT NULL,`.
`migrations/0002_usage_rollup.sql`: `THEN value ELSE -value END` becomes
`THEN quantity ELSE -quantity END`, and `+value/+1 … -value/-1` in the header
becomes `+quantity/+1 … -quantity/-1`.

`entity.rs`: the field becomes `/// `quantity` — signed `numeric` quantity.` then
`pub quantity: Decimal,`.

`record_store.rs`:
- in both `RECORD_COLUMNS` and `INSERT_COLUMNS`, `value` becomes `quantity`;
- the single insert bind becomes `.bind(record.quantity.as_decimal())`;
- `InsertColumns.values: Vec<Decimal>` becomes `quantities: Vec<Decimal>`, in
  the struct, `build` (`quantities: Vec::with_capacity(..)`,
  `cols.quantities.push(r.quantity.as_decimal())`) and the batch bind
  `.bind(&cols.quantities)`;
- `canonical_equal`: `row.value == incoming.value` becomes
  `row.quantity == incoming.quantity.as_decimal()`, and the doc's `value`
  mention becomes `quantity`.

`mapper.rs` `record_row_to_model`: before `Ok(UsageRecord {` add

```rust
    let quantity = UsageQuantity::try_from(row.quantity).map_err(|e| {
        UsageCollectorPluginError::internal(format!(
            "stored row `{id}` (window_end {window_end}): quantity invalid: {e}"
        ))
    })?;
```

In the literal, `value: row.value` becomes `quantity`. Add `quantity` to the
`# Errors` newtype list, and import `UsageQuantity`.

`query/aggregate.rs`, `query/rollup.rs`, `rollup_maintenance.rs`: in SQL
string fragments, replace the ledger column reference `r.value` (and bare
`value` where it names the `usage_records` column) with `r.quantity`/`quantity`.
Leave `sum_value`, `count_value` and Rust `value` locals as they are.
`migration_probe.rs`: rename a `("value", "numeric")` entry in the expected
ledger columns to `("quantity", "numeric")`, if present.

`tests/common/mod.rs`: keep the `entry(…, value: Decimal)` signatures, but
build the record with `quantity: UsageQuantity::try_from(value).expect("fixture quantity")`.
Rename other `.value` record reads in `tests/*.rs` and `src/**/*_tests.rs` to
`.quantity`. Where a test compares it with a `Decimal`, compare
`.quantity.as_decimal()`.

- [ ] **Step 3e: Noop plugin and E2E**

`plugins/noop-usage-collector-plugin/src/plugin_tests.rs`: rename the
record's `value` field as in 3a.

`testing/e2e/suites/usage_collector/conftest.py` `record_payload`:
- the parameter `value: str = "1"` becomes `quantity: str = "1"`;
- the payload key `"value": value` becomes `"quantity": quantity`;
- in the docstring, `` `value` is a STRING `` becomes `` `quantity` is a STRING ``.

`test_integration_seams.py`: `value="…"` kwargs become `quantity="…"`, and
`body["value"]` (line ~53, record response) becomes `body["quantity"]`. Leave
the aggregate bucket `b["value"]` unchanged.

- [ ] **Step 4: Verify**

Run: `cargo check` on the four packages with `--all-targets --all-features`, until clean.
Run `$UNIT`. Expected: PASS, including
`a_quantity_outside_the_published_range_rejects_only_its_entry` and the
DTO key-set test.
Run `$PG`. Expected: PASS (or report "Docker unavailable, not run").
Run `$LINT`. Expected: clean.
Run: `rg -n '\bvalue\b' gears/system/usage-collector/usage-collector/src/api/rest/dto.rs`
and check that only aggregate/bucket DTO hits remain.

- [ ] **Step 5: Commit**

```bash
git add -u gears/system/usage-collector testing/e2e/suites/usage_collector
git commit -s -m "feat(usage-collector)!: carry the entry quantity as a validated UsageQuantity" -m "BREAKING CHANGE: the record wire field \`value\` is renamed \`quantity\` and is validated against the published range; out-of-range values, scientific notation and negative zero are rejected with QUANTITY_OUT_OF_RANGE."
```

---

### Task 4: Server-stamped `accepted_at`

**Files:**
- SDK: `models.rs`, `models_tests.rs`, `contract/fixtures.rs`, `contract/reference.rs`, `contract_mutants.rs`/`contract_tests.rs` (compiler-driven)
- Gateway: `domain/service.rs`, `service_tests.rs`, `test_support.rs`, `dto.rs`, `dto_tests.rs`, plus compiler-driven test files
- TimescaleDB: `migrations/0001_init.sql`, `entity.rs`, `mapper.rs`, `mapper_tests.rs`, `record_store.rs`, `record_store_tests.rs`, `migration_probe.rs`, `tests/schema_integration_pg.rs`, `tests/common/mod.rs`, `tests/records_ingest_integration_pg.rs`
- Noop: `plugin_tests.rs`

**Interfaces:**
- Consumes: Task 3 shapes.
- Produces:
  - `UsageRecord.accepted_at: time::OffsetDateTime`, declared between `idempotency_key` and `origin`.
  - `CreateUsageRecord::try_into_usage_record(self, origin: RecordOrigin, accepted_at: OffsetDateTime)`.
  - `UsageRecordDto.accepted_at: OffsetDateTime`, RFC 3339.
  - Gateway-private `fn acceptance_instant() -> OffsetDateTime`.
  - Contract `pub const CONTRACT_ACCEPTED_AT: OffsetDateTime`.
  - TimescaleDB column `accepted_at timestamptz NOT NULL`, with no default.

- [ ] **Step 1: Write the failing tests**

`models_tests.rs`: add

```rust
const SAMPLE_ACCEPTED_AT: time::OffsetDateTime =
    SAMPLE_WINDOW_END.saturating_add(time::Duration::minutes(5));

#[test]
fn try_into_usage_record_stamps_the_given_acceptance_instant() {
    let record = sample_create_usage_record(None, None)
        .try_into_usage_record(RecordOrigin::Live, SAMPLE_ACCEPTED_AT)
        .expect("projects");
    assert_eq!(record.accepted_at, SAMPLE_ACCEPTED_AT);
    let encoded = serde_json::to_value(&record).expect("serializes");
    assert_eq!(encoded["accepted_at"], json!("1970-01-01T01:05:00Z"));
}

#[test]
fn a_submission_carrying_accepted_at_is_refused() {
    let mut body = serde_json::to_value(sample_create_usage_record(None, None)).unwrap();
    body["accepted_at"] = json!("1970-01-01T01:05:00Z");
    serde_json::from_value::<CreateUsageRecord>(body)
        .expect_err("accepted_at is server-assigned and must be refused as unknown");
}
```

In `sample_usage_record`, add `accepted_at: SAMPLE_ACCEPTED_AT,` after the key
field. Pass `SAMPLE_ACCEPTED_AT` to every existing `try_into_usage_record(..)`
call as the second argument.

`service_tests.rs`: add a module:

```rust
#[cfg(test)]
mod acceptance_stamp_tests {
    use std::collections::BTreeMap;

    use toolkit_gts::gts_id;
    use usage_collector_sdk::{CreateUsageRecord, IdempotencyKey, MeterTypeId, ResourceRef};
    use uuid::Uuid;

    use crate::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, qty, recent_window_end,
        recent_window_start,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn submission(idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            gts_type_id: MeterTypeId::new(GTS_ID).unwrap(),
            tenant_id: Uuid::from_u128(1),
            resource_ref: ResourceRef::new("rsc-stamp", "compute.vm").unwrap(),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: qty("1"),
            idempotency_key: IdempotencyKey::new(idem).unwrap(),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    #[tokio::test]
    async fn every_entry_of_one_request_carries_one_microsecond_acceptance_instant() {
        let plugin = HappyPathPlugin::new();
        let service = ServiceFixture::default().build(plugin.clone(), "stamp");
        let before = time::OffsetDateTime::now_utc();
        let _ = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![submission("stamp-a"), submission("stamp-b"), submission("stamp-c")],
            )
            .await;
        let after = time::OffsetDateTime::now_utc();

        let dispatched = plugin
            .last_create_records_input()
            .expect("the batch reached the plugin");
        assert_eq!(dispatched.len(), 3);
        let stamp = dispatched[0].accepted_at;
        assert!(dispatched.iter().all(|r| r.accepted_at == stamp), "one instant per request");
        assert_eq!(stamp.nanosecond() % 1_000, 0, "stamped at microsecond precision");
        let floor = before.replace_microsecond(before.microsecond()).unwrap();
        assert!(floor <= stamp && stamp <= after, "stamped during the call");
    }
}
```

If `HappyPathPlugin` or `ServiceFixture::build` need a declaration source to
reach dispatch, copy the setup used by an existing `create_usage_records`
happy-path test in `service_tests.rs` (for example `.with_source(...)`).
Search for `last_create_records_input()` and reuse that test's builder
verbatim.

`dto_tests.rs` key-set test: add `"accepted_at",` to both expected vectors,
keeping them sorted. Add `accepted_at` to the `sample_persisted_record`/`sample_persisted_invalidation` literals.

`tests/records_ingest_integration_pg.rs`: add

```rust
#[tokio::test]
async fn an_absorbed_retry_returns_the_first_acceptance_instant() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let first = common::entry(&meter, Uuid::from_u128(71), "accepted-at-retry", Decimal::from(5));
    let first_at = first.accepted_at;
    let stored = store.create_usage_record(first.clone()).await.expect("first write");
    assert_eq!(stored.accepted_at, first_at);

    let retry = UsageRecord {
        accepted_at: first_at + Duration::minutes(3),
        ..first
    };
    let absorbed = store.create_usage_record(retry).await.expect("absorbed retry");
    assert_eq!(absorbed.accepted_at, first_at, "a replay reports the stored acceptance instant");
}
```

The `RecordStore` port method may be named differently in `domain/ports.rs`.
Use that trait's single-record create method, as the other tests in this file
do.

- [ ] **Step 2: Run to verify failure**

Run: `cargo check -p cf-gears-usage-collector-sdk --all-targets --all-features`
Expected: FAIL (no field `accepted_at`, wrong arity).

- [ ] **Step 3a: SDK model**

`UsageRecord`: add between `idempotency_key` and `origin`:

```rust
    /// Gear-assigned instant of acceptance, stamped once per request by the
    /// Ingestion Gateway at microsecond precision. Never caller-supplied:
    /// [`CreateUsageRecord`] has no such field and its wire shadow refuses
    /// one. An absorbed retry reports the stored entry's instant, which is
    /// how a caller tells a replay from a first write. Not an input to
    /// [`Self::id`]'s derivation and not compared on a dedup collision.
    pub accepted_at: time::OffsetDateTime,
```

`UsageRecordWire` and `UsageRecordWireRef`: add
`#[serde(with = "time::serde::rfc3339")] accepted_at: time::OffsetDateTime,`
after `idempotency_key`. Thread it through both destructures and struct
literals. The `Serialize` impl uses `accepted_at: *accepted_at`.

`try_into_usage_record`: the signature becomes
`(self, origin: RecordOrigin, accepted_at: time::OffsetDateTime)`. Add
`accepted_at,` after `idempotency_key` in the literal. In the doc, replace
"`origin` is the exception: it is the one field not forwarded…" with "`origin`
and `accepted_at` are the exceptions: both are server-assigned and stamped
from the arguments, and neither is an input to the derivation."

The `RecordOrigin` doc sentence "It joins `id` and `accepted_at` in DESIGN
§3.1's server-assigned group" stays.

- [ ] **Step 3b: Contract harness**

`fixtures.rs`: add

```rust
/// The acceptance instant every fixture entry carries. Fixed rather than read
/// from the clock so a check's expected entries compare equal to what it
/// wrote; no slice-A check varies it.
pub const CONTRACT_ACCEPTED_AT: time::OffsetDateTime =
    time::macros::datetime!(2026-01-01 00:00:00 UTC);
```

If the `time` `macros` feature is not enabled for the SDK, use
`time::OffsetDateTime::UNIX_EPOCH.saturating_add(time::Duration::days(20_454))`.
Pass it as the second argument of `try_into_usage_record` in `fixture_record_for_tenant`.

`reference.rs` `admit`: replace the absorb comparison with

```rust
        // The `id` is the `UUIDv5` of the five dedup-identity attributes, so
        // a collision already means those five agree; what is compared here
        // is everything else the entry carries, except the acceptance
        // instant, which the gateway stamps afresh on every submission and
        // which a replay reports from the stored entry.
        let replay = UsageRecord {
            accepted_at: stored.accepted_at,
            ..record.clone()
        };
        if *stored == replay {
            return Ok(stored.clone());
        }
```

- [ ] **Step 3c: Gateway**

`service.rs`: add near `project_and_admit`:

```rust
/// The acceptance instant of one request: now, truncated to the microsecond.
///
/// Truncated because every conforming store keeps microseconds (Postgres
/// `timestamptz` does), so a finer stamp would read back different from what
/// was acknowledged. The same instant judges the covered-period bounds, so
/// the stamp and the late-arrival decision cannot disagree.
fn acceptance_instant() -> OffsetDateTime {
    let now = OffsetDateTime::now_utc();
    now.replace_microsecond(now.microsecond()).unwrap_or(now)
}
```

Replace both `let now = OffsetDateTime::now_utc();` sites in
`create_usage_record_inner` and `create_usage_records_inner` with
`let now = acceptance_instant();`. In `project_and_admit`, call
`submission.try_into_usage_record(origin, now)?`, and add to its doc: "`now`
is also the entry's `accepted_at`."

`test_support.rs` `projected`/`projected_with_origin`: pass
`time::OffsetDateTime::UNIX_EPOCH` as `accepted_at`, and add a
`projected_at(&CreateUsageRecord, OffsetDateTime)` variant if a test needs to
match a dispatched stamp. For tests comparing a dispatched record with
`projected(..)`, compare with `accepted_at` copied from the dispatched record:
`UsageRecord { accepted_at: dispatched.accepted_at, ..projected(&s) }`.

`dto.rs` `UsageRecordDto`: add after `idempotency_key`:

```rust
    /// Gear-assigned instant of acceptance (RFC 3339). On an absorbed retry,
    /// the stored entry's instant.
    #[serde(with = "time::serde::rfc3339")]
    pub accepted_at: OffsetDateTime,
```

In `From<UsageRecord>`, add `accepted_at: value.accepted_at,`.

- [ ] **Step 3d: TimescaleDB**

`0001_init.sql`: replace `ingested_at         timestamptz NOT NULL DEFAULT now(),` with

```sql
    -- Gear-assigned acceptance instant, stamped by the Ingestion Gateway and
    -- written as given. An absorbed retry returns this stored value.
    accepted_at         timestamptz NOT NULL,
```

`entity.rs`:
- the last field becomes `/// `accepted_at` — gear-assigned acceptance instant.` then `pub accepted_at: OffsetDateTime,`;
- in the struct doc, "Two of the columns below have no counterpart…" becomes
  "One column below has no counterpart on the SDK's `UsageRecord`:
  `acceptance_sequence`…";
- delete the `ingested_at` clause.

`record_store.rs`:
- `RECORD_COLUMNS`: `ingested_at` becomes `accepted_at`.
- `INSERT_COLUMNS`: insert `accepted_at, ` immediately before `metadata`, which
  stays last.
- `INSERT_COLUMN_ARRAY_TYPES`: the length becomes `18`, with `"timestamptz",`
  inserted before the final `"text",`.
- Replace the `INSERT_COLUMNS` doc's first sentence with "The columns every
  insert writes: the same set as [`RECORD_COLUMNS`], ordered so `metadata` is
  last. `entry_type` is generated and appears in neither."
- Single insert: `.bind(record.accepted_at)` goes immediately before
  `.bind(metadata)`.
- `InsertColumns`: add `accepted_ats: Vec<OffsetDateTime>` before `metadata`,
  with `Vec::with_capacity(reps.len())`, `cols.accepted_ats.push(r.accepted_at);`,
  and in the batch bind `.bind(&cols.accepted_ats)` before `.bind(&cols.metadata)`.
- Update the "seventeen" doc mentions to "eighteen".
- The comment near line 453, "`ingested_at` is left to its DEFAULT", becomes
  "`accepted_at` is bound from the record".
- The `canonical_equal` doc: its `ingested_at` exclusion mention becomes
  `accepted_at`.

`mapper.rs`: `accepted_at: row.accepted_at,` in the literal, and drop
`ingested_at` from the doc ("Two of the row's columns…" becomes "One of the
row's columns, `acceptance_sequence`, is read and deliberately dropped").
`migration_probe.rs` `insertable_columns`: the filter becomes
`!matches!(*name, "entry_type")`, and the doc "`ingested_at` is `DEFAULT
now()`" is deleted, leaving one exclusion.
`tests/schema_integration_pg.rs` (~279–303), `record_store_tests.rs`
(~242, 444–450, 1496), `mapper_tests.rs` (~200), `aggregate_tests.rs`,
`translate_tests.rs`: rename `ingested_at` to `accepted_at`, and remove
assertions that it has a `DEFAULT now()`.
`tests/common/mod.rs`: records built by `entry`/`entry_over`/`rederive` pass
`fixture_window_end()` as `accepted_at` (to `try_into_usage_record` or the
struct literal).

- [ ] **Step 3e: Everything else the compiler names**

- Every remaining `try_into_usage_record(origin)` call gets a second
  argument: `recent_window_end()` in gateway tests,
  `SAMPLE_ACCEPTED_AT`/`CONTRACT_ACCEPTED_AT` in the SDK, and
  `fixture_window_end()` in plugin tests.
- Every `UsageRecord { … }` literal gets an `accepted_at`.

- [ ] **Step 4: Verify**

Run `$UNIT`. Expected: PASS, including
`every_entry_of_one_request_carries_one_microsecond_acceptance_instant`.
Run `$PG`. Expected: PASS, including
`an_absorbed_retry_returns_the_first_acceptance_instant` and the
migration probe.
Run `$LINT`. Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add -u gears/system/usage-collector
git commit -s -m "feat(usage-collector)!: stamp and persist accepted_at on every entry" -m "BREAKING CHANGE: UsageRecord and the REST UsageRecordDto carry a required accepted_at; the TimescaleDB ledger column ingested_at is replaced by accepted_at written from the gateway."
```

---

### Task 5: Idempotency-key rules for invalidations and the `inv:` prefix

**Files:**
- SDK: `models.rs`, `models_tests.rs`, `error.rs`, `id.rs`, `id_tests.rs`, `contract/fixtures.rs`, `contract/checks/*.rs`, `contract_mutants.rs`, `contract_tests.rs`
- Gateway: `dto.rs`, `handlers/usage_records.rs`, `handlers/usage_records_tests.rs`, `domain/invalidation.rs`, `invalidation_tests.rs`, `service_tests.rs`, `test_support.rs`
- TimescaleDB: `mapper.rs`, `tests/common/mod.rs`
- E2E: `conftest.py`

**Interfaces:**
- Consumes: Tasks 1–4.
- Produces:
  - `CreateUsageRecord.idempotency_key: Option<IdempotencyKey>`.
  - `IdempotencyKey::new`, which rejects `inv:`.
  - `IdempotencyKey::for_invalidation(target: Uuid) -> IdempotencyKey`.
  - `IdempotencyKey::from_stored(impl Into<String>) -> Result<IdempotencyKey, UsageCollectorError>`.
  - `pub const INVALIDATION_KEY_PREFIX: &str = "inv:"`.
  - `UsageCollectorError::{reserved_idempotency_key_prefix, idempotency_key_on_invalidation, missing_idempotency_key}() -> Self`.
  - REST `CreateUsageRecordRequest.idempotency_key: Option<String>`.
  - Contract `fixture_invalidation(quantity, ws, we, target)`, with no key parameter.

- [ ] **Step 1: Write the failing tests**

`models_tests.rs`:

```rust
fn reason_of(err: &UsageCollectorError) -> &ValidationReason {
    match err {
        UsageCollectorError::InvalidArgument { reason, .. } => reason,
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

#[test]
fn a_caller_key_may_not_use_the_reserved_invalidation_prefix() {
    let err = IdempotencyKey::new("inv:anything").expect_err("reserved prefix");
    assert_eq!(field_of(&err), "idempotency_key");
    assert_eq!(reason_of(&err), &ValidationReason::ReservedKeyPrefix);
    IdempotencyKey::new("INV:upper-is-not-reserved").expect("the prefix is case-sensitive");
    serde_json::from_value::<IdempotencyKey>(json!("inv:x")).expect_err("deserialize routes through new");
}

#[test]
fn a_stored_key_may_carry_the_prefix_and_is_otherwise_validated() {
    let stored = IdempotencyKey::from_stored("inv:33333333-3333-3333-3333-333333333333")
        .expect("stored invalidation key");
    assert_eq!(stored.as_str(), "inv:33333333-3333-3333-3333-333333333333");
    IdempotencyKey::from_stored("").expect_err("still non-empty");
    IdempotencyKey::from_stored("a\u{1f}b").expect_err("still control-character free");
}

#[test]
fn an_invalidation_key_is_the_prefix_and_the_lowercase_hyphenated_target() {
    let target = Uuid::parse_str("ABCDEF01-2345-6789-ABCD-EF0123456789").unwrap();
    assert_eq!(
        IdempotencyKey::for_invalidation(target).as_str(),
        "inv:abcdef01-2345-6789-abcd-ef0123456789",
    );
}

#[test]
fn a_record_without_a_key_is_refused_at_projection() {
    let mut s = sample_create_usage_record(None, None);
    s.idempotency_key = None;
    let err = s.try_into_usage_record(RecordOrigin::Live, SAMPLE_ACCEPTED_AT).expect_err("missing key");
    assert_eq!(field_of(&err), "idempotency_key");
    assert_eq!(reason_of(&err), &ValidationReason::Validation);
}

#[test]
fn an_invalidation_carrying_a_key_is_refused_at_projection() {
    let mut s = sample_create_usage_record(None, Some(target_id()));
    s.idempotency_key = Some(IdempotencyKey::new("caller-key").unwrap());
    let err = s.try_into_usage_record(RecordOrigin::Live, SAMPLE_ACCEPTED_AT).expect_err("key on invalidation");
    assert_eq!(field_of(&err), "idempotency_key");
    assert_eq!(reason_of(&err), &ValidationReason::KeyOnInvalidation);
}

#[test]
fn an_invalidation_is_stored_under_its_derived_key() {
    let record = sample_create_usage_record(None, Some(target_id()))
        .try_into_usage_record(RecordOrigin::Live, SAMPLE_ACCEPTED_AT)
        .expect("projects");
    assert_eq!(record.idempotency_key, IdempotencyKey::for_invalidation(target_id()));
    assert_eq!(
        record.id,
        crate::derive_usage_record_id(
            record.tenant_id,
            &record.gts_type_id,
            &IdempotencyKey::for_invalidation(target_id()),
            record.window_start,
            record.window_end,
        ),
    );
}
```

Update the fixtures:
- `sample_create_usage_record` sets
  `idempotency_key: invalidates.is_none().then(|| IdempotencyKey::new("k-1").expect("valid idempotency key"))`;
- `sample_usage_record` sets `idempotency_key` to
  `IdempotencyKey::for_invalidation(t)` when `invalidates` is `Some(t)`, and to
  `IdempotencyKey::new("k-1")` otherwise.

`id_tests.rs`:

```rust
fn target() -> Uuid {
    Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap()
}

#[test]
fn derive_matches_golden_vector_for_an_invalidation_key() {
    // UUIDv5(NS, "11111111-1111-1111-1111-111111111111" 0x1F
    //            "gts.cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~" 0x1F
    //            "inv:33333333-3333-3333-3333-333333333333" 0x1F
    //            "2023-11-14T22:13:20.000000Z" 0x1F
    //            "2023-11-14T23:13:20.000000Z")
    //
    // Computed independently of this crate. DO NOT hand-edit.
    assert_eq!(
        derive_usage_record_id(tenant(), &gts(), &IdempotencyKey::for_invalidation(target()), ws(), we()),
        expect(INV_GOLDEN),
    );
}

#[test]
fn an_invalidation_differs_from_its_target_only_through_the_key() {
    let record_id = derive_usage_record_id(tenant(), &gts(), &key("idem-1"), ws(), we());
    let inv_id = derive_usage_record_id(tenant(), &gts(), &IdempotencyKey::for_invalidation(record_id), ws(), we());
    assert_ne!(record_id, inv_id);
    // Every invalidation of one entry derives one identifier.
    assert_eq!(
        inv_id,
        derive_usage_record_id(tenant(), &gts(), &IdempotencyKey::for_invalidation(record_id), ws(), we()),
    );
}

#[test]
fn an_inv_measurement_key_is_rejected_before_derivation() {
    IdempotencyKey::new("inv:33333333-3333-3333-3333-333333333333")
        .expect_err("a measurement key cannot carry the reserved prefix");
}
```

Add at the top of `id_tests.rs`:

```rust
/// Golden vector for the `inv:` key, computed with Python's `uuid.uuid5` over
/// the pre-image in `derive_matches_golden_vector_for_an_invalidation_key`.
/// The same computation reproduces the existing `idem-1` vector
/// (`5b075acb-e2e8-55a8-aedf-4c7d01b60284`).
const INV_GOLDEN: &str = "55fc111f-59c9-5542-905a-f642a6644b08";
```

Gateway `handlers/usage_records_tests.rs`: add two tests. Build the request
with the Task 3 closure pattern, using `idempotency_key: Option<String>`.

```rust
#[tokio::test]
async fn an_invalidation_with_a_key_and_a_record_without_one_reject_only_their_entries() {
    let (service, _resolver) = service_with_sentinel_pdp();
    let base = |idem: Option<&str>, invalidates: Option<Uuid>| CreateUsageRecordRequest {
        gts_type_id: HAPPY_RECORD_GTS_ID.to_owned(),
        tenant_id: Uuid::from_u128(2),
        resource_ref: ResourceRefDto { resource_id: "rsc-k".to_owned(), resource_type: "compute.vm".to_owned() },
        subject_ref: None,
        metadata: BTreeMap::new(),
        quantity: "1".to_owned(),
        idempotency_key: idem.map(str::to_owned),
        invalidates,
        reason_code: invalidates.map(|_| "E2E_CORRECTION".to_owned()),
        window_start: recent_window_start(),
        window_end: recent_window_end(),
    };
    let response = handle_create_usage_records(
        Extension(SecurityContext::anonymous()),
        Extension(service),
        Json(CreateUsageRecordsRequest {
            records: vec![
                base(Some("caller"), Some(Uuid::from_u128(9))),
                base(None, None),
                base(Some("inv:forged"), None),
            ],
        }),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::MULTI_STATUS);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap();
    let reason = |i: usize| body["results"][i]["error"]["context"]["field_violations"][0]["reason"].clone();
    assert_eq!(reason(0), "KEY_ON_INVALIDATION");
    assert_eq!(reason(1), "VALIDATION");
    assert_eq!(reason(2), "RESERVED_KEY_PREFIX");
}
```

If the handler's gate rejects entries 0–1 before the PDP, as today, these
rejections come from the service projection rather than the handler. Either
way each surfaces per entry. If `service_with_sentinel_pdp` denies before
projection, use the permit-based service builder that the file's existing
207 tests use.

`service_tests.rs`: add a test asserting the dispatched key of an invalidation
equals `IdempotencyKey::for_invalidation(target)`. Model it on an existing
invalidation happy-path test: search for `verify_invalidation_target` usage in
`service_tests.rs` and copy its arrangement. Build the submission with
`idempotency_key: None`, then assert
`plugin.last_create_record_input().unwrap().idempotency_key == IdempotencyKey::for_invalidation(target_id)`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo nextest run -p cf-gears-usage-collector-sdk models_tests id_tests`
Expected: compile FAIL (`for_invalidation`, `from_stored` missing; `Option` mismatch).

- [ ] **Step 3a: SDK**

`error.rs`, after `invalid_reason_code`:

```rust
    /// A caller key began with the reserved `inv:` prefix. `field` is
    /// `idempotency_key`.
    #[must_use]
    pub fn reserved_idempotency_key_prefix() -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "idempotency_key".to_owned(),
            reason: ValidationReason::ReservedKeyPrefix,
            detail: "idempotency_key must not begin with `inv:`, which is reserved for the \
                     keys the gateway derives for invalidations"
                .to_owned(),
        }
    }

    /// An invalidation carried an idempotency key. `field` is
    /// `idempotency_key`.
    #[must_use]
    pub fn idempotency_key_on_invalidation() -> Self {
        Self::InvalidArgument {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            resource_name: None,
            field: "idempotency_key".to_owned(),
            reason: ValidationReason::KeyOnInvalidation,
            detail: "an invalidation carries no idempotency_key: the gateway derives \
                     `inv:<invalidates>`, so omit it"
                .to_owned(),
        }
    }

    /// A record carried no idempotency key. `field` is `idempotency_key`.
    #[must_use]
    pub fn missing_idempotency_key() -> Self {
        Self::newtype_validation("idempotency_key", "idempotency_key is required on a record")
    }
```

`models.rs`, in the idempotency-key section:

```rust
/// Prefix of every derived invalidation key. A caller key may not begin with
/// it, so a measurement key and an invalidation key can never coincide.
pub const INVALIDATION_KEY_PREFIX: &str = "inv:";
```

Restructure `IdempotencyKey`:
- Move the existing body of `new` (empty, length, control checks) into
  `fn validate_stored_form(raw: &str) -> Result<(), UsageCollectorError>`.
- `new` runs `validate_stored_form(&raw)?`, then
  `if raw.starts_with(INVALIDATION_KEY_PREFIX) { return Err(UsageCollectorError::reserved_idempotency_key_prefix()); }`,
  then `Ok(Self(raw))`. Its doc adds "or begins with the reserved `inv:`
  prefix ([`INVALIDATION_KEY_PREFIX`])".
- Add:

```rust
    /// The key an invalidation of `target` is stored under: `inv:` followed
    /// by the target id, lowercase and hyphenated
    /// (`cpt-cf-usage-collector-adr-record-identity-derivation`). Every
    /// invalidation of one entry therefore shares one dedup identity.
    #[must_use]
    pub fn for_invalidation(target: Uuid) -> Self {
        Self(format!("{INVALIDATION_KEY_PREFIX}{}", target.hyphenated()))
    }

    /// Rebuilds a key read back from storage, where an invalidation's
    /// derived `inv:` key is legitimate. Applies every check [`Self::new`]
    /// does except the reserved-prefix rule.
    ///
    /// # Errors
    ///
    /// [`UsageCollectorError::InvalidArgument`] when the stored value is
    /// empty, longer than 256 characters, or carries an ASCII control
    /// character.
    pub fn from_stored(value: impl Into<String>) -> Result<Self, UsageCollectorError> {
        let raw = value.into();
        validate_stored_form(&raw)?;
        Ok(Self(raw))
    }
```

`Uuid::hyphenated()` renders lowercase. `Deserialize for IdempotencyKey` keeps
calling `new`.

`UsageRecordWire.idempotency_key` must accept the `inv:` form. Change its type
to `String` and build it in `TryFrom<UsageRecordWire>` with
`IdempotencyKey::from_stored(idempotency_key).map_err(|e| e.to_string())?`.

`CreateUsageRecord`:
- the field becomes

```rust
    /// The caller's key, required on a record and forbidden on an
    /// invalidation, whose key the projection derives as
    /// [`IdempotencyKey::for_invalidation`].
    pub idempotency_key: Option<IdempotencyKey>,
```

- `CreateUsageRecordWire`: `#[serde(default)] idempotency_key: Option<IdempotencyKey>`;
- `CreateUsageRecordWireRef`: `#[serde(skip_serializing_if = "Option::is_none")] idempotency_key: Option<&'a IdempotencyKey>`,
  with `idempotency_key.as_ref()` in `Serialize`.

In `try_into_usage_record`, after the inverted-period check:

```rust
        let idempotency_key = match (&self.invalidation, self.idempotency_key) {
            (Some(invalidation), None) => IdempotencyKey::for_invalidation(invalidation.target),
            (Some(_), Some(_)) => return Err(UsageCollectorError::idempotency_key_on_invalidation()),
            (None, Some(key)) => key,
            (None, None) => return Err(UsageCollectorError::missing_idempotency_key()),
        };
```

Pass `&idempotency_key` to `derive_usage_record_id`, and use `idempotency_key`
in the literal. Replace the doc's last paragraph ("The fourth does not: at
most one invalidation per record is the **store's**…") with: "The key rules
are checked here, before the derivation: a record must carry a key, and an
invalidation must not, because its key is derived from its target." Add both
key rejections to `# Errors`.

In the `UsageRecord.invalidation` doc, replace the two paragraphs about
"reusing the target's key across the pair collides…" with:

```rust
    /// [`Invalidation::target`] is not an input to the derived identity
    /// directly; it enters through the derived key `inv:<target>`, so every
    /// invalidation of one entry shares one dedup identity and a second one
    /// collides instead of producing a second withdrawal.
```

`id.rs` `derive_usage_record_id` doc: replace the paragraph starting "The
parameter list is the enforcement of the entry-type exclusion…" with: "Entry
type is not among the inputs. An invalidation derives over the key
`inv:<target>` ([`IdempotencyKey::for_invalidation`]), so its identifier is a
function of its target, and it departs from the target's only through that
key." In the module header, replace "admitting it would let one idempotency
key stand for both…" through the end of that paragraph with: "the reserved
`inv:` key prefix keeps a measurement and an invalidation apart instead."

`contract/fixtures.rs`: `fixture_invalidation(quantity: UsageQuantity, ws, we, target: Uuid)`
builds the `CreateUsageRecord` directly with `idempotency_key: None` and
`invalidation: Some(Invalidation { target, reason })`, then projects with
`try_into_usage_record(RecordOrigin::Live, CONTRACT_ACCEPTED_AT)`. Replace its
doc with: "Builds one invalidation of `target`. Its key is derived as
`inv:<target>`, so every invalidation of one target shares one identity."
`fixture_record_for_tenant` sets `idempotency_key: Some(idempotency_key.clone())`.

At every `fixture_invalidation(&key, …)` call site in
`contract/checks/*.rs`, `contract_mutants.rs` and `contract_tests.rs`, drop the
key argument. Where `at_most_one_invalidation.rs` builds two withdrawals of one
target with distinct keys, they now share one identity:
- if the same reason code is used, the second is absorbed by the reference
  plugin, which breaks the check's current `AlreadyInvalidated` expectation;
- **do not rewrite the check's semantics (slice E).** Instead, give the second
  withdrawal a *different* `reason` so the plugin still refuses it. Under the
  reference plugin that is now `IdempotencyConflict` on the duplicate id.
- If the check then fails on the expected error variant, widen its accepted
  outcomes to `AlreadyInvalidated | IdempotencyConflict`, and add a code
  comment: `// Slice E rewrites this check on the derived inv: key (SPEC-DIFF 8.3).`

- [ ] **Step 3b: Gateway**

`dto.rs` `CreateUsageRecordRequest.idempotency_key`:

```rust
    /// The caller's key, required on a record and forbidden on an
    /// invalidation: an invalidation's key is derived by the gateway as
    /// `inv:` followed by `invalidates`. A record key may not begin with
    /// `inv:`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
```

`handlers/usage_records.rs` `record_request_into_domain`:

```rust
    let idempotency_key = req
        .idempotency_key
        .map(IdempotencyKey::new)
        .transpose()
        .map_err(|err| Problem::from(usage_collector_error_to_canonical(err)))?;
```

The SDK projection enforces exclusivity. That projection runs in the service,
so the rejection reaches the batch response through `per_record_outcome` at
the entry's own index.

`domain/invalidation.rs` `faithful_copy_mismatch`: in both destructures, keep
`idempotency_key: _` and replace the comments:
- submission side: `// Not compared: an invalidation carries no caller key; its key is derived from the target.`
- target side: `// Not compared: the target's key is its own; the invalidation's is derived.`

Gateway tests: the compiler names every `CreateUsageRecord` literal.
- Records: `idempotency_key: Some(IdempotencyKey::new(..).unwrap())`.
- Invalidations: `idempotency_key: None`.
- DTO literals: `Some("…".to_owned())` / `None`.
- Tests that expected an invalidation's own caller key on the dispatched
  record now expect `IdempotencyKey::for_invalidation(target)`.

- [ ] **Step 3c: TimescaleDB and E2E**

`mapper.rs`: `IdempotencyKey::new(row.idempotency_key)` becomes
`IdempotencyKey::from_stored(row.idempotency_key)`.
`tests/common/mod.rs` `withdrawal_of(target, idem)`: build the withdrawal with
`idempotency_key: IdempotencyKey::for_invalidation(target.id)`, and re-derive
`id` (via `rederive`). Keep the `idem` parameter unused-prefixed (`_idem`) only
if removing it would cascade beyond this crate's tests; otherwise remove it and
update call sites. Where an integration test relied on two withdrawals of one
target with distinct keys, apply the same rule as the contract check in 3a:
distinct reason codes. If an at-most-one assertion then sees the dedup index
fire before the partial invalidation index, accept either error in that test,
with the comment `// Slice B removes the store-side rule (SPEC-DIFF 2.1).`

`testing/e2e/suites/usage_collector/conftest.py` `record_payload`: send the key
only for records.

```python
    payload = {
        "gts_type_id": gts_type_id,
        "tenant_id": tenant_id,
        "resource_ref": {"resource_id": resource_id, "resource_type": "compute.vm"},
        "quantity": quantity,
        "window_start": window_start,
        "window_end": window_end,
        "metadata": metadata or {},
    }
    if invalidates is not None:
        payload["invalidates"] = invalidates
        payload["reason_code"] = reason_code or "E2E_CORRECTION"
    else:
        payload["idempotency_key"] = idempotency_key or f"e2e-idem-{unique_suffix()}"
    return payload
```

Add to the docstring: "An invalidation carries no `idempotency_key`; the gateway
derives `inv:<invalidates>`."

- [ ] **Step 4: Verify**

Run `$UNIT`. Expected: PASS.
Run `$PG`. Expected: PASS, or "Docker unavailable, not run".
Run `$LINT`. Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add -u gears/system/usage-collector testing/e2e/suites/usage_collector
git commit -s -m "feat(usage-collector)!: derive invalidation keys and reserve the inv: prefix" -m "BREAKING CHANGE: idempotency_key is optional on submissions — required on a record, rejected (KEY_ON_INVALIDATION) on an invalidation, whose key is derived as inv:<invalidates>; a record key beginning inv: is rejected (RESERVED_KEY_PREFIX)."
```

---

### Task 6: Configurable batch cap and per-entry REST decoding

**Files:**
- Modify: `usage-collector/src/config.rs`, `config_tests.rs`
- Modify: `usage-collector/src/domain/service.rs` (cap constant, constructor, cap check, docs)
- Modify: `usage-collector/src/domain/test_support.rs`, `service_tests.rs`
  (`batch_size_cap_tests`), `service_metrics_tests.rs` (`new_with_metrics`
  call sites)
- Modify: `usage-collector/src/infra/metrics.rs`, `metrics_tests.rs`
- Modify: `usage-collector/src/module.rs`
- Modify: `usage-collector/src/api/rest/dto.rs`,
  `handlers/usage_records.rs`, `handlers/usage_records_tests.rs`

**Interfaces:**
- Consumes: Tasks 3–5 DTO shapes.
- Produces:
  - `UsageCollectorConfig.max_batch_records: usize`.
  - `crate::domain::service::DEFAULT_MAX_BATCH_RECORDS: usize = 100`.
  - `Service::new_with_metrics(hub, vendor, enforcer, metrics, type_resolver, metadata_size_cap_bytes, covered_period_bounds, max_batch_records: usize)`.
  - `Service::max_batch_records(&self) -> usize`.
  - `UcMetricsMeter::new(meter: &Meter, prefix: &str, max_batch_records: usize)`.
  - `build_default_adapter(prefix: &str, max_batch_records: usize)`.
  - `infra::metrics::ingestion_batch_size_buckets(cap: usize) -> Vec<f64>`.
  - `ServiceFixture::with_max_batch_records(usize)`.
  - `CreateUsageRecordsRequest.records: Vec<serde_json::Value>`.

- [ ] **Step 1: Write the failing tests**

`config_tests.rs`:

```rust
#[test]
fn the_batch_cap_defaults_to_100() {
    let cfg: UsageCollectorConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(cfg.max_batch_records, 100);
}

#[test]
fn a_zero_batch_cap_is_rejected() {
    let cfg = UsageCollectorConfig {
        max_batch_records: 0,
        ..Default::default()
    };
    let err = cfg.validate().expect_err("zero batch cap must be rejected");
    assert!(err.to_string().contains("max_batch_records"), "got: {err}");
}
```

`metrics_tests.rs`: the existing bounds assertion stays, since the default cap
is 100. Add:

```rust
#[test]
fn batch_size_buckets_end_at_the_configured_cap() {
    use crate::infra::metrics::ingestion_batch_size_buckets;
    assert_eq!(ingestion_batch_size_buckets(100), vec![1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0]);
    assert_eq!(ingestion_batch_size_buckets(30), vec![1.0, 2.0, 5.0, 10.0, 20.0, 30.0]);
    assert_eq!(ingestion_batch_size_buckets(1), vec![1.0]);
    assert_eq!(ingestion_batch_size_buckets(500), vec![1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 500.0]);
}
```

`service_tests.rs` `batch_size_cap_tests`: replace the
`use crate::domain::service::MAX_BATCH_RECORDS;` import and every use of
`MAX_BATCH_RECORDS` with a local `const CAP: usize = 3;`. Build the service
with `ServiceFixture::default().with_max_batch_records(CAP)`. Then add:

```rust
    #[tokio::test]
    async fn the_configured_cap_is_the_one_enforced() {
        let plugin = HappyPathPlugin::new();
        let service = ServiceFixture::default()
            .with_max_batch_records(CAP)
            .build(plugin, "cap-config");
        assert_eq!(service.max_batch_records(), CAP);
        let over: Vec<_> = (0..=CAP).map(|i| input_record(&format!("cap-{i}"))).collect();
        let err = service
            .create_usage_records(&authenticated_ctx(), over)
            .await
            .expect_err("CAP + 1 must be refused");
        assert_invalid_batch_size(&err);
    }
```

`handlers/usage_records_tests.rs`:
- the two cap tests: build `records` as `Vec<serde_json::Value>`; the empty
  one becomes `records: Vec::new()`;
- the over-cap test uses `service.max_batch_records() + 1` JSON entries
  produced by
  `serde_json::to_value(CreateUsageRecordRequest { … }).unwrap()`, with
  `CreateUsageRecordRequest` keeping `Serialize` via `api_dto(request)`;
  otherwise build `json!({...})`;
- in `create_with_batch_above_cap_rejects_without_iterating_records`, make one
  entry malformed (`json!({"bogus": true})`) to pin cap-before-decode.

Add:

```rust
#[tokio::test]
async fn a_malformed_entry_rejects_only_its_own_index() {
    let (service, _resolver) = service_with_sentinel_pdp();
    let good = serde_json::json!({
        "gts_type_id": HAPPY_RECORD_GTS_ID,
        "tenant_id": Uuid::from_u128(2),
        "resource_ref": {"resource_id": "rsc-m", "resource_type": "compute.vm"},
        "quantity": "1",
        "idempotency_key": "idem-m-0",
        "window_start": recent_window_start().format(&time::format_description::well_known::Rfc3339).unwrap(),
        "window_end": recent_window_end().format(&time::format_description::well_known::Rfc3339).unwrap(),
    });
    let mut unknown_field = good.clone();
    unknown_field["idempotency_key"] = "idem-m-1".into();
    unknown_field["bogus"] = true.into();
    let mut missing_field = good.clone();
    missing_field["idempotency_key"] = "idem-m-2".into();
    missing_field.as_object_mut().unwrap().remove("tenant_id");
    let not_an_object = serde_json::json!(42);

    let response = handle_create_usage_records(
        Extension(SecurityContext::anonymous()),
        Extension(service),
        Json(CreateUsageRecordsRequest {
            records: vec![good, unknown_field, missing_field, not_an_object],
        }),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::MULTI_STATUS);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap();
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 4);
    let violation = |i: usize| results[i]["error"]["context"]["field_violations"][0].clone();
    assert_eq!(results[1]["outcome"], "rejected");
    assert_eq!(violation(1)["field"], "bogus");
    assert_eq!(violation(1)["reason"], "VALIDATION");
    assert_eq!(violation(2)["field"], "tenant_id");
    assert_eq!(violation(3)["field"], "records");
    for i in 1..4 {
        assert_eq!(results[i]["index"], i);
    }
}
```

Entry 0's outcome depends on the sentinel PDP. Assert only that it is not a
decode failure: it is `accepted`, or it is rejected with a reason other than
`VALIDATION` on `records`. If the sentinel PDP rejects, leave entry 0
unasserted.

- [ ] **Step 2: Run to verify failure**

Run: `cargo nextest run -p cf-gears-usage-collector config_tests metrics_tests batch_size_cap usage_records_tests`
Expected: compile FAIL (`max_batch_records`, `ingestion_batch_size_buckets`, `Vec<Value>`).

- [ ] **Step 3a: Config and service**

`service.rs`: replace the `MAX_BATCH_RECORDS` const and its doc with:

```rust
/// Default per-request entry cap, used when `[usage_collector].max_batch_records`
/// is not configured. The cap is operator configuration (DESIGN §3.2), so the
/// wire schema publishes no `maxItems`; it bounds what one caller sends, not
/// what one backend write holds.
pub const DEFAULT_MAX_BATCH_RECORDS: usize = 100;
```

- In the `PDP_CONCURRENCY` doc, "[`MAX_BATCH_RECORDS`] batch takes
  `ceil(100 / 8) × PDP_RTT`" becomes "cap-sized batch takes
  `ceil(cap / 8) × PDP_RTT`".
- Add the field `max_batch_records: usize` to `Service`. `Service::new` passes
  `DEFAULT_MAX_BATCH_RECORDS`. `new_with_metrics` gains the trailing parameter
  `max_batch_records: usize`; document it next to `covered_period_bounds`.
- Add:

```rust
    /// The per-request entry cap this service enforces.
    #[must_use]
    pub const fn max_batch_records(&self) -> usize {
        self.max_batch_records
    }
```

- In `create_usage_records_for_origin`, replace `MAX_BATCH_RECORDS` with
  `self.max_batch_records` (both uses).
- Fix every remaining doc mention of `MAX_BATCH_RECORDS` the compiler or
  `rg MAX_BATCH_RECORDS` shows: "the configured cap (`max_batch_records`)".

`config.rs`: add the field after `backfill_window_secs`:

```rust
    /// Per-request entry cap on both ingestion routes. An empty or over-cap
    /// submission is rejected whole, before any entry is validated.
    ///
    /// Defaults to
    /// [`DEFAULT_MAX_BATCH_RECORDS`](crate::domain::service::DEFAULT_MAX_BATCH_RECORDS)
    /// (`100`).
    pub max_batch_records: usize,
```

- `Default`: `max_batch_records: crate::domain::service::DEFAULT_MAX_BATCH_RECORDS,`.
- In `validate`, after the capacity check:

```rust
        if self.max_batch_records == 0 {
            anyhow::bail!(
                "[usage_collector].max_batch_records must be greater than 0: a zero cap \
                 refuses every ingestion request"
            );
        }
```

- Add "`max_batch_records` is zero" to the `# Errors` list.

`test_support.rs`: `ServiceFixture` gains `max_batch_records: Option<usize>` and

```rust
    /// Configure a non-default per-request entry cap.
    #[must_use]
    pub(crate) fn with_max_batch_records(mut self, cap: usize) -> Self {
        self.max_batch_records = Some(cap);
        self
    }
```

`build_service` passes
`params.max_batch_records.unwrap_or(crate::domain::service::DEFAULT_MAX_BATCH_RECORDS)`
as the new last argument. The other `new_with_metrics` call sites
(`test_support.rs` ~979, `service_tests.rs` 448/744,
`service_metrics_tests.rs` 1506) pass `DEFAULT_MAX_BATCH_RECORDS`.

- [ ] **Step 3b: Metrics and module**

`infra/metrics.rs`: replace `INGESTION_BATCH_SIZE_BUCKETS` with:

```rust
/// The fixed ladder `uc_ingestion_batch_size` buckets are cut from.
const INGESTION_BATCH_SIZE_LADDER: [f64; 7] = [1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0];

/// Buckets for `uc_ingestion_batch_size` (records per request): the ladder
/// entries below the configured cap, then the cap itself, so the upper bucket
/// is always the cap (DESIGN §3.11.5).
#[must_use]
pub fn ingestion_batch_size_buckets(cap: usize) -> Vec<f64> {
    // A cap is a count of entries, far below 2^52, so the cast is exact.
    #[allow(clippy::cast_precision_loss)]
    let cap = cap as f64;
    INGESTION_BATCH_SIZE_LADDER
        .iter()
        .copied()
        .filter(|bound| *bound < cap)
        .chain(std::iter::once(cap))
        .collect()
}
```

- `UcMetricsMeter::new(meter: &Meter, prefix: &str, max_batch_records: usize)`
  uses `.with_boundaries(ingestion_batch_size_buckets(max_batch_records))`.
- `build_default_adapter(prefix: &str, max_batch_records: usize)` passes it
  through.
- Update callers:
  - `module.rs`: `build_default_adapter(cfg.metrics.effective_prefix(), cfg.max_batch_records)`,
    and pass `cfg.max_batch_records` as the last `Service::new_with_metrics`
    argument;
  - `test_support::local_metrics`: `UcMetricsMeter::new(&provider.meter("usage-collector"), "uc", DEFAULT_MAX_BATCH_RECORDS)`;
  - `metrics_tests.rs` `meter(..)`: the same;
  - every other caller the compiler names, including `type_resolver/resolver_tests.rs`
    if it builds `UcMetricsMeter`.
- Fix the `infra/metrics.rs:264` comment "batch sizes (1..=100)" to "batch
  sizes (1..=cap)".

- [ ] **Step 3c: Per-entry decoding**

`dto.rs`:

```rust
/// Batch create request body for `POST /usage-collector/v1/records`.
///
/// `records` is decoded one entry at a time by the handler, after the cap
/// check, so one malformed entry rejects only its own index. The schema
/// still publishes the entry shape.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct CreateUsageRecordsRequest {
    #[schema(value_type = Vec<CreateUsageRecordRequest>)]
    pub records: Vec<serde_json::Value>,
}
```

`handlers/usage_records.rs`:
- remove the `use crate::domain::service::MAX_BATCH_RECORDS;` import;
- both handlers pass `service.max_batch_records()` into
  `dispatch_usage_record_batch`, as a new second parameter `max_batch_records: usize`,
  cloning or borrowing `service` before the dispatch closure moves it;
- in `dispatch_usage_record_batch`, the cap check uses `max_batch_records`;
- replace the comment above it with:
  `// Cap first, before any entry is decoded (DESIGN §3.2): an empty or over-cap submission is rejected whole.`
- the per-record loop becomes:

```rust
    for (index, raw) in req.records.into_iter().enumerate() {
        let decoded = decode_record_entry(raw).and_then(record_request_into_domain);
        match decoded {
            Ok(record) => eligible.push((index, record)),
            Err(problem) => indexed_results.push((
                index,
                CreateUsageRecordResultDto::Rejected {
                    index,
                    error: problem,
                },
            )),
        }
    }
```

Add:

```rust
/// Decodes one raw batch entry into the request DTO, or the `Problem` a
/// single-entry submission with the same body would receive.
///
/// The violation names the offending property when serde reports one
/// (unknown or missing field) and `records` otherwise.
#[allow(clippy::result_large_err)]
fn decode_record_entry(raw: serde_json::Value) -> Result<CreateUsageRecordRequest, Problem> {
    serde_json::from_value::<CreateUsageRecordRequest>(raw).map_err(|err| {
        let message = err.to_string();
        let field = serde_field_name(&message).unwrap_or("records");
        Problem::from(
            UsageRecordResource::invalid_argument()
                .with_field_violation(field, message.as_str(), VALIDATION)
                .create(),
        )
    })
}

/// The property a serde error names: the backticked token after
/// "unknown field " or "missing field ".
fn serde_field_name(message: &str) -> Option<&str> {
    ["unknown field `", "missing field `"]
        .iter()
        .find_map(|marker| message.split_once(marker))
        .and_then(|(_, rest)| rest.split_once('`'))
        .map(|(field, _)| field)
}
```

Import `UsageRecordResource` and `VALIDATION` the way `parse_record_id` in the
same file does. It builds `.with_field_violation("id", …, "VALIDATION")`, so
mirror its imports and builder calls exactly. If the builder takes `String`
arguments, adapt.

- [ ] **Step 4: Verify**

Run `$UNIT`. Expected: PASS.
Run `$LINT`. Expected: clean.
Run `rg -n 'MAX_BATCH_RECORDS' gears/system/usage-collector`. Expected: no hits.

- [ ] **Step 5: Commit**

```bash
git add -u gears/system/usage-collector
git commit -s -m "feat(usage-collector): make the batch cap configurable and decode entries one by one"
```

---

### Task 7: Regenerate OpenAPI and verify the slice end to end

**Files:**
- Modify: `docs/api/api.json` (generated)
- Possibly modify: `usage-collector/src/api/rest/routes/openapi_contract_tests.rs`

- [ ] **Step 1: Regenerate**

Run: `make openapi` (repo root).
Expected: `docs/api/api.json` changes, and `git diff docs/api/api.json` shows:
- `CreateUsageRecordRequest` has `quantity` and no `value`, and
  `idempotency_key` is no longer required;
- `UsageRecordDto` has `quantity` and `accepted_at`, both required;
- `CreateUsageRecordsRequest.records` items still reference
  `CreateUsageRecordRequest`.

If `records` became a bare object array, the `#[schema(value_type = …)]` in
Task 6 did not take effect. Fix the attribute and rerun.

- [ ] **Step 2: Contract test**

Run: `cargo nextest run -p cf-gears-usage-collector openapi_contract`
Expected: PASS. If it fails because an exception list entry now resolves
(for example a schema drift this slice fixed), remove that entry. Do not add
new exceptions. Report any new failure instead.

- [ ] **Step 3: Full verification**

Run each command and record its outcome:
- `cargo fmt --all -- --check`: clean.
- `$LINT`: clean.
- `$UNIT`: all PASS.
- `$PG`: all PASS, or "Docker unavailable, not run".
- E2E: check `testing/e2e/suites/usage_collector/e2e.yaml` and the Makefile
  for the suite's local target (for example `make e2e-local SUITE=usage_collector`).
  Run it if the environment supports it; otherwise report "e2e not run" and why.

- [ ] **Step 4: Commit**

```bash
git add docs/api/api.json gears/system/usage-collector/usage-collector/src/api/rest/routes/openapi_contract_tests.rs
git commit -s -m "chore(usage-collector): regenerate OpenAPI for the slice-A record shape"
```

---

## Self-Review Notes

Coverage of spec sections, by task:

| Spec | Task |
| --- | --- |
| §3.1 `UsageQuantity` | 1 |
| §3.2 model (`quantity`) | 3 |
| §3.2 model (`accepted_at`) | 4 |
| §3.3 keys | 5 |
| §3.4 string limits | 2 |
| §3.5 reasons | 1 |
| §3.6 golden vectors | 5 |
| §4.1 batch cap | 6 |
| §4.2 batch decoding | 6 |
| §4.3 stamping | 4 |
| §4.4 invalidation path | 3, 5 |
| §5.1 TimescaleDB | 3, 4, 5 |
| §5.2 noop plugin | 3–5 |
| §5.3 contract harness | 3–5 |
| §6 e2e | 3, 5 |
| §6 OpenAPI | 7 |
| §8 verification | each task, 7 |

Spec changes made during planning: negative zero is rejected, and
`CONTRACT_ACCEPTED_AT` is a constant.

Known soft spots for the executor. Each has an explicit fallback in its step:
- the `time` macros feature;
- `service_with_sentinel_pdp` denying before projection;
- `at_most_one_invalidation` expectations under derived keys;
- the TimescaleDB `withdrawal_of` signature;
- the `RecordStore` single-create method name.
