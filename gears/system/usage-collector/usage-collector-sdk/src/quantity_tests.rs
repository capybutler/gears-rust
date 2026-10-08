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
        assert_eq!(
            quantity.to_string(),
            text,
            "`{text}` must render back unchanged"
        );
    }
}

#[test]
fn forms_outside_the_published_pattern_are_rejected() {
    for text in [
        "",
        "-",
        "+1",
        "1e3",
        "1E3",
        "1_000",
        "01",
        "-01",
        "00.5",
        ".5",
        "5.",
        "1.2.3",
        " 1",
        "1 ",
        "0x10",
        "NaN",
        "inf",
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
fn equality_is_textual_and_as_decimal_exposes_the_numeric_value() {
    let short = UsageQuantity::parse("42.5").unwrap();
    let long = UsageQuantity::parse("42.500").unwrap();
    assert_ne!(
        short, long,
        "equality is digit for digit: 42.5 and 42.500 are two quantities"
    );
    assert_eq!(short, UsageQuantity::parse("42.5").unwrap());
    assert_eq!(
        short.as_decimal(),
        long.as_decimal(),
        "the numeric value stays reachable through as_decimal"
    );
    assert_eq!(long.as_decimal(), Decimal::from_str("42.5").unwrap());
}

#[test]
fn hashing_agrees_with_textual_equality() {
    let set: std::collections::HashSet<UsageQuantity> = ["42.5", "42.500", "42.5", "-1", "0"]
        .into_iter()
        .map(|text| UsageQuantity::parse(text).unwrap())
        .collect();
    assert_eq!(
        set.len(),
        4,
        "equal texts hash together, differently scaled ones apart"
    );
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
    let too_wide = Decimal::MAX;
    assert!(UsageQuantity::try_from(too_wide).is_err());
    // A sign-flagged zero `Decimal` (unlike `Decimal::from_str_exact("-0")`,
    // which normalizes the sign away on parse) Displays as `-0`, and
    // `try_from` routes through that text form — so it hits the same
    // negative-zero rejection a stored `-0` would.
    assert!(UsageQuantity::try_from(-Decimal::ZERO).is_err());
}

#[test]
fn serde_is_a_json_string_only() {
    let q = UsageQuantity::parse("42.500").unwrap();
    assert_eq!(serde_json::to_value(q).unwrap(), json!("42.500"));
    assert_eq!(
        serde_json::from_value::<UsageQuantity>(json!("42.500")).unwrap(),
        q
    );
    serde_json::from_value::<UsageQuantity>(json!(42.5)).expect_err("a JSON number is refused");
    serde_json::from_value::<UsageQuantity>(json!("1e3")).expect_err("routes through parse");
}
