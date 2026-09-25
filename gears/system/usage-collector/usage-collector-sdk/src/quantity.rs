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
//! rejected. Negative zero is rejected too. `Decimal::from_str_exact("-0")`
//! normalizes the sign away on parse, so a plain reparse of `-0` can't
//! survive it; only a sign-flagged zero `Decimal`, built without going
//! through that parse (e.g. `-Decimal::ZERO`), Displays as `-0`. Postgres
//! `numeric` has no negative zero either, so no path — wire or storage —
//! can return one digit for digit.
//!
//! Equality is textual: `42.5` and `42.500` are different quantities (SPEC-DIFF decision S-B7). Use [`UsageQuantity::as_decimal`] for numeric comparison.

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
#[derive(Debug, Clone, Copy)]
pub struct UsageQuantity(Decimal);

/// Digit for digit: the mantissa and the scale, so `42.5 != 42.500`. Negative
/// zero never parses, so a zero has one representation per scale.
impl PartialEq for UsageQuantity {
    fn eq(&self, other: &Self) -> bool {
        self.0.mantissa() == other.0.mantissa() && self.0.scale() == other.0.scale()
    }
}

impl Eq for UsageQuantity {}

impl core::hash::Hash for UsageQuantity {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.0.mantissa().hash(state);
        self.0.scale().hash(state);
    }
}

impl UsageQuantity {
    /// Parses a quantity from its wire text.
    ///
    /// # Errors
    ///
    /// [`UsageCollectorError::InvalidArgument`] with reason
    /// `QUANTITY_OUT_OF_RANGE` on field `quantity` when the text does not
    /// match the published pattern, carries more than 28 significant digits,
    /// or is a negative zero.
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is 144 bytes because Conflict carries invalidated_by/reason_code (SPEC-DIFF 2.2); callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
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
