// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native Datum.ToDecimal and text/JSON conversion. JSON float conversion uses
//! its existing math constructor, not the plain-float text/event path. This
//! selector has no context effects and does not round temporal inputs first.
use smallvec::SmallVec;

use super::{
    mysql::{
        Decimal, NativeDecimalParseError as DecimalParseError, NativeDecimalParseValue,
        binary_literal::native_binary_literal_to_int, json::native_binary_json_string_bytes,
        native_decimal_parse_mysql,
    },
    native_numeric::{NativeNumericError, NativeNumericInput},
    native_temporal_number::{native_duration_to_number, native_time_to_number},
};

/// Original display-length arithmetic, including its negative metadata and
/// unchecked i32 operation domain. Do not replace with SQL shape clamping.
pub const fn native_decimal_length_to_precision(
    mut length: i32,
    scale: i32,
    unsigned: bool,
) -> i32 {
    if scale > 0 {
        length -= 1;
    }
    if unsigned || length > 0 {
        length -= 1;
    }
    length
}
pub const fn native_precision_to_length_no_truncation(
    mut length: i32,
    scale: i32,
    unsigned: bool,
) -> i32 {
    if scale > 0 {
        length += 1;
    }
    if unsigned || length > 0 {
        length += 1;
    }
    length
}

/// Native GetMaxValue/GetMinValue's literal payload before Decimal
/// construction. Preserve the original integer.max(1) nine for scale>0,
/// including flen<=scale; zero-width integral bounds remain empty text (and a
/// lone '-' for minimum).
pub fn native_bound_decimal_text(flen: i64, decimal: i64, maximum: bool) -> String {
    let flen = flen.max(0) as usize;
    let scale = decimal.max(0) as usize;
    let integer = flen.saturating_sub(scale);
    let text = if scale == 0 {
        "9".repeat(integer)
    } else {
        format!("{}.{}", "9".repeat(integer.max(1)), "9".repeat(scale))
    };
    if maximum { text } else { format!("-{text}") }
}

#[derive(Clone, Debug)]
pub struct NativeDecimalConverted {
    pub value: NativeDecimalParseValue,
    pub event: Option<NativeDecimalConversionEvent>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeDecimalConversionEvent {
    Truncated,
    /// The native error target is always NewDecimal; retain the original input.
    Overflow {
        value: String,
    },
}
fn exact(value: NativeDecimalParseValue) -> NativeDecimalConverted {
    NativeDecimalConverted { value, event: None }
}
fn truncated_zero() -> NativeDecimalConverted {
    NativeDecimalConverted {
        value: NativeDecimalParseValue::from_int(0),
        event: Some(NativeDecimalConversionEvent::Truncated),
    }
}

/// The native nine-word parser owns prefix acceptance, exponent limits and
/// partial values. Only the original conversion-event projection lives here.
pub fn native_decimal_from_text(text: &str) -> NativeDecimalConverted {
    let (value, error) = native_decimal_parse_mysql(text, 9);
    let event = match error {
        None => None,
        Some(DecimalParseError::Overflow) => Some(NativeDecimalConversionEvent::Overflow {
            value: text.to_owned(),
        }),
        Some(_) => Some(NativeDecimalConversionEvent::Truncated),
    };
    NativeDecimalConverted { value, event }
}

/// Raw native JSON accessors retain their width/string expect panics. JSON
/// strings discard invalid UTF-8 wholesale, unlike Datum byte-string
/// conversion.
pub fn native_json_to_decimal(type_code: u8, value: &[u8]) -> NativeDecimalConverted {
    if matches!(type_code, 0x01 | 0x03 | 0x0d | 0x0e | 0x0f | 0x10 | 0x11) {
        return truncated_zero();
    }
    match type_code {
        0x04 => match value.first().copied() {
            Some(2) => exact(NativeDecimalParseValue::from_int(0)),
            Some(0) | None => truncated_zero(),
            Some(_) => exact(NativeDecimalParseValue::from_int(1)),
        },
        0x09 => {
            let value = <&[u8; 8]>::try_from(value)
                .ok()
                .map(|bytes| i64::from_le_bytes(*bytes))
                .expect("validated binary JSON integer");
            exact(NativeDecimalParseValue::from_int(value))
        }
        0x0a => {
            let value = <&[u8; 8]>::try_from(value)
                .ok()
                .map(|bytes| u64::from_le_bytes(*bytes))
                .expect("validated binary JSON integer");
            exact(NativeDecimalParseValue::from_uint(value))
        }
        0x0b => {
            let value = <&[u8; 8]>::try_from(value)
                .ok()
                .map(|bytes| f64::from_bits(u64::from_le_bytes(*bytes)))
                .expect("validated binary JSON float");
            Decimal::native_from_f64(value).map_or_else(truncated_zero, |value| {
                exact(
                    NativeDecimalParseValue::from_shared(value)
                        .expect("shared native float decimal materialization failed"),
                )
            })
        }
        0x0c => {
            let text = std::str::from_utf8(
                native_binary_json_string_bytes(type_code, value)
                    .expect("validated binary JSON string"),
            )
            .unwrap_or("");
            native_decimal_from_text(text)
        }
        _ => truncated_zero(),
    }
}

/// Actual numeric storage selection. Datum strings intentionally use the
/// original lossy UTF-8 bridge before the prefix parser, not signed
/// conversion's checked UTF-8 policy. InvalidUtf8/Comparison are not produced
/// by this selector.
pub fn native_datum_to_decimal(
    input: NativeNumericInput<'_>,
) -> Result<NativeDecimalConverted, NativeNumericError> {
    use NativeNumericInput as I;
    Ok(match input {
        I::Int(value) => exact(NativeDecimalParseValue::from_int(value)),
        I::UInt(value) => exact(NativeDecimalParseValue::from_uint(value)),
        I::Real(value) => native_decimal_from_text(&Decimal::native_format_float_g_shortest(value)),
        I::Float32(value) => native_decimal_from_text(&Decimal::native_format_float_g_shortest(
            f64::from(value as f32),
        )),
        I::String(bytes) | I::Bytes(bytes) => {
            native_decimal_from_text(&String::from_utf8_lossy(bytes))
        }
        I::Time(value) => exact(native_time_to_number(value)),
        I::Duration(value) => exact(native_duration_to_number(value)),
        I::Decimal(value) => exact(NativeDecimalParseValue::from_raw_parts(
            value.negative,
            SmallVec::from_slice(value.digits),
            value.scale,
            value.storage_scale,
            value.declared_shape,
        )),
        I::Enum(value) | I::Set(value) => exact(NativeDecimalParseValue::from_uint(value)),
        I::Json { type_code, value } => native_json_to_decimal(type_code, value),
        I::BinaryLiteral(bytes) | I::Bit(bytes) => {
            let outcome = native_binary_literal_to_int(bytes);
            NativeDecimalConverted {
                value: NativeDecimalParseValue::from_uint(outcome.value()),
                event: outcome
                    .is_truncated()
                    .then_some(NativeDecimalConversionEvent::Truncated),
            }
        }
        I::Null | I::MinNotNull | I::MaxValue | I::Raw(_) | I::VectorFloat32(_) => {
            return Err(NativeNumericError::Unsupported);
        }
    })
}

#[cfg(test)]
mod bound_tests {
    use super::*;
    #[test]
    fn native_decimal_bound_text_preserves_zero_shapes_excess_scale_and_sign_prefix() {
        for (flen, scale, maximum, minimum) in [
            (5, 2, "999.99", "-999.99"),
            (3, 0, "999", "-999"),
            (0, 0, "", "-"),
            (-1, -1, "", "-"),
            (i64::MIN, i64::MIN, "", "-"),
            (2, 2, "9.99", "-9.99"),
            (1, 3, "9.999", "-9.999"),
            (-2, 2, "9.99", "-9.99"),
            (2, -2, "99", "-99"),
        ] {
            assert_eq!(native_bound_decimal_text(flen, scale, true), maximum);
            assert_eq!(native_bound_decimal_text(flen, scale, false), minimum);
        }
    }
}

#[cfg(test)]
mod numeric_helper_tests {
    use super::*;
    #[test]
    fn native_numeric_helpers_precision_keep_const_i32_sign_and_scale_arithmetic() {
        const PRECISION: i32 = native_decimal_length_to_precision(12, 2, false);
        const LENGTH: i32 = native_precision_to_length_no_truncation(PRECISION, 2, false);
        assert_eq!((PRECISION, LENGTH), (10, 12));
        for (length, scale, unsigned, precision, display) in [
            (0, 0, false, 0, 0),
            (0, 0, true, -1, 1),
            (1, 1, false, 0, 3),
            (0, 1, false, -1, 2),
            (0, 1, true, -2, 2),
            (-1, 1, false, -2, 0),
            (-1, 1, true, -3, 1),
            (-1, -1, false, -1, -1),
            (-1, -1, true, -2, 0),
            (12, 2, false, 10, 14),
            (12, -1, false, 11, 13),
            (i32::MIN, 0, false, i32::MIN, i32::MIN),
        ] {
            assert_eq!(
                native_decimal_length_to_precision(length, scale, unsigned),
                precision
            );
            assert_eq!(
                native_precision_to_length_no_truncation(length, scale, unsigned),
                display
            );
        }
        assert_eq!(
            native_decimal_length_to_precision(i32::MAX, 0, false),
            i32::MAX - 1
        );
        assert_eq!(
            native_precision_to_length_no_truncation(i32::MAX - 1, 0, false),
            i32::MAX
        );
    }
}

#[cfg(test)]
mod tests {
    use NativeNumericInput as I;

    use super::{
        super::{
            mysql::{
                NativeDecimalParseRef, NativeVectorFloat32, Time,
                time::{NativeTemporalValue, TimeType},
            },
            native_duration_convert::NativeDurationParts,
        },
        *,
    };
    fn text(value: &NativeDecimalConverted) -> String {
        let value = value.value.as_ref();
        Decimal::native_format_visible(
            value.negative,
            value.digits,
            value.scale,
            value.storage_scale,
        )
    }
    #[test]
    fn decimal_datum_selector_keeps_actual_storage_ordinals_and_temporal_fractions() {
        let calendar = NativeTemporalValue {
            raw: Time::native_core_from_fields(2020, 1, 2, 3, 4, 5, 500000),
            kind: TimeType::DateTime,
            fsp: 1,
        };
        for (input, expected) in [
            (I::Int(i64::MIN), "-9223372036854775808"),
            (I::UInt(u64::MAX), "18446744073709551615"),
            (I::Real(1.25), "1.25"),
            (I::Float32(16_777_217.0), "16777216"),
            (I::String(b"12.5"), "12.5"),
            (I::Bytes(b"12.5"), "12.5"),
            (I::Time(calendar), "20200102030405.5"),
            (
                I::Duration(NativeDurationParts {
                    nanoseconds: 1_500_000_000,
                    fsp: 1,
                }),
                "1.5",
            ),
            (I::Enum(u64::MAX), "18446744073709551615"),
            (I::Set(u64::MAX), "18446744073709551615"),
            (I::BinaryLiteral(&[255; 8]), "18446744073709551615"),
            (I::Bit(&[255; 8]), "18446744073709551615"),
            (
                I::Json {
                    type_code: 4,
                    value: &[1],
                },
                "1",
            ),
        ] {
            let converted = native_datum_to_decimal(input).unwrap();
            assert_eq!(text(&converted), expected);
            assert_eq!(converted.event, None);
        }
        let original = NativeDecimalParseRef {
            negative: true,
            digits: b"000125",
            scale: 1,
            storage_scale: 2,
            declared_shape: Some((11, 1)),
        };
        let cloned = native_datum_to_decimal(I::Decimal(original)).unwrap();
        let actual = cloned.value.as_ref();
        assert_eq!(
            (
                actual.negative,
                actual.digits,
                actual.scale,
                actual.storage_scale,
                actual.declared_shape
            ),
            (true, &b"000125"[..], 1, 2, Some((11, 1)))
        );
        assert_eq!(cloned.event, None);
        for input in [I::BinaryLiteral(&[1; 9]), I::Bit(&[1; 9])] {
            let converted = native_datum_to_decimal(input).unwrap();
            // ToDecimal retains ToInt's u64::MAX payload, unlike the signed
            // BinaryLiteral conversion's separate truncation-to-zero policy.
            assert_eq!(text(&converted), "18446744073709551615");
            assert_eq!(
                converted.event,
                Some(NativeDecimalConversionEvent::Truncated)
            );
        }
        for input in [I::String(b"12\xff"), I::Bytes(b"12\xff")] {
            let converted = native_datum_to_decimal(input).unwrap();
            assert_eq!(text(&converted), "12");
            assert_eq!(
                converted.event,
                Some(NativeDecimalConversionEvent::Truncated)
            );
        }
        let vector = NativeVectorFloat32::must_create(vec![1.0]);
        for input in [
            I::Null,
            I::MinNotNull,
            I::MaxValue,
            I::Raw(b"1"),
            I::VectorFloat32(&vector),
        ] {
            assert!(matches!(
                native_datum_to_decimal(input),
                Err(NativeNumericError::Unsupported)
            ));
        }
    }
    #[test]
    fn decimal_text_and_json_keep_prefix_events_float_policy_and_raw_accessor_failures() {
        for (input, expected) in [("123abc", "123"), ("1,999.00", "1"), ("", "0")] {
            let converted = native_decimal_from_text(input);
            assert_eq!(text(&converted), expected);
            assert_eq!(
                converted.event,
                Some(NativeDecimalConversionEvent::Truncated)
            );
        }
        let overflow = native_decimal_from_text("1e100");
        assert_eq!(text(&overflow), "9".repeat(81));
        assert_eq!(
            overflow.event,
            Some(NativeDecimalConversionEvent::Overflow {
                value: "1e100".into()
            })
        );
        let bytes = 1e100f64.to_le_bytes();
        let json = native_json_to_decimal(11, &bytes);
        assert_eq!(text(&json), "9".repeat(81));
        assert_eq!(json.event, None); // from_f64 retains the math parser's partial value without reporting its status
        let plain = native_datum_to_decimal(I::Real(1e100)).unwrap();
        assert_eq!(text(&plain), "9".repeat(81));
        assert_eq!(
            plain.event,
            Some(NativeDecimalConversionEvent::Overflow {
                value: "1e+100".into()
            })
        );
        for bytes in [f64::INFINITY.to_le_bytes(), f64::NAN.to_le_bytes()] {
            let converted = native_json_to_decimal(11, &bytes);
            assert_eq!(text(&converted), "0");
            assert_eq!(
                converted.event,
                Some(NativeDecimalConversionEvent::Truncated)
            );
        }
        let invalid = native_json_to_decimal(12, b"\x0312\xff");
        assert_eq!(text(&invalid), "0");
        assert_eq!(invalid.event, Some(NativeDecimalConversionEvent::Truncated));
        let prefix = native_json_to_decimal(12, b"\x06123abc");
        assert_eq!(text(&prefix), "123");
        assert_eq!(prefix.event, Some(NativeDecimalConversionEvent::Truncated));
        let literal = native_json_to_decimal(4, &[255]);
        assert_eq!(text(&literal), "1");
        assert_eq!(literal.event, None);
        for (tag, bytes) in [
            (4, &[][..]),
            (4, &[0][..]),
            (1, &[255][..]),
            (3, &[255][..]),
            (13, &[255][..]),
            (14, &[255][..]),
            (15, &[255][..]),
            (16, &[255][..]),
            (17, &[255][..]),
            (255, &[][..]),
        ] {
            let converted = native_json_to_decimal(tag, bytes);
            assert_eq!(text(&converted), "0");
            assert_eq!(
                converted.event,
                Some(NativeDecimalConversionEvent::Truncated)
            );
        }
        let unsigned = native_json_to_decimal(10, &[255; 8]);
        assert_eq!(text(&unsigned), "18446744073709551615");
        assert_eq!(unsigned.event, None);
        for (tag, message) in [
            (9, "validated binary JSON integer"),
            (10, "validated binary JSON integer"),
            (11, "validated binary JSON float"),
            (12, "validated binary JSON string"),
        ] {
            let panic = std::panic::catch_unwind(|| native_json_to_decimal(tag, &[])).unwrap_err();
            let actual = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied());
            assert_eq!(actual, Some(message));
        }
    }
}
