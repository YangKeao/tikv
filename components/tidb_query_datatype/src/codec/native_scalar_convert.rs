// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native Datum boolean/float conversions. JSON truth compares the actual JSON
//! value with parsed numeric zero, not with a numeric JSON conversion. Float32
//! storage narrows only on the float path; boolean conversion reads its raw
//! f64.
use std::cmp::Ordering;

use super::{
    mysql::{
        binary_literal::native_binary_literal_to_int,
        json::{compare_native_binary_json, native_binary_json_string_bytes},
    },
    native_float_parse::{NativeFloatConversion, native_str_to_float_reported},
    native_json_parse::native_json_parse,
    native_numeric::{NativeNumericError, NativeNumericInput},
    native_temporal_number::{native_duration_to_number, native_time_to_number},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeBoolConversion {
    pub value: i64,
    pub truncated: bool,
}
fn exact_float(value: f64) -> NativeFloatConversion {
    NativeFloatConversion {
        value,
        truncated: false,
    }
}
fn exact_bool(value: bool) -> NativeBoolConversion {
    NativeBoolConversion {
        value: i64::from(value),
        truncated: false,
    }
}
fn parse_float(text: &str) -> NativeFloatConversion {
    native_str_to_float_reported(text, false, |_| {})
}
fn float_bool(value: NativeFloatConversion) -> NativeBoolConversion {
    NativeBoolConversion {
        value: i64::from(value.value != 0.0),
        truncated: value.truncated,
    }
}

/// Native ConvertJSONToFloat retains malformed scalar accessor panics, literal
/// fallback rules and nonfinite float payloads. Invalid string UTF-8 becomes
/// empty input, whereas Datum strings are checked before parsing.
pub fn native_json_to_float(type_code: u8, value: &[u8]) -> NativeFloatConversion {
    let truncated_zero = || NativeFloatConversion {
        value: 0.0,
        truncated: true,
    };
    if matches!(type_code, 0x01 | 0x03 | 0x0d | 0x0e | 0x0f | 0x10 | 0x11) {
        return truncated_zero();
    }
    match type_code {
        0x04 => match value.first().copied() {
            Some(2) => exact_float(0.0),
            Some(0) | None => truncated_zero(),
            Some(_) => exact_float(1.0),
        },
        0x09 => exact_float(
            <&[u8; 8]>::try_from(value)
                .ok()
                .map(|bytes| i64::from_le_bytes(*bytes))
                .expect("validated binary JSON integer") as f64,
        ),
        0x0a => exact_float(
            <&[u8; 8]>::try_from(value)
                .ok()
                .map(|bytes| u64::from_le_bytes(*bytes))
                .expect("validated binary JSON integer") as f64,
        ),
        0x0b => exact_float(
            <&[u8; 8]>::try_from(value)
                .ok()
                .map(|bytes| f64::from_bits(u64::from_le_bytes(*bytes)))
                .expect("validated binary JSON float"),
        ),
        0x0c => {
            let text = std::str::from_utf8(
                native_binary_json_string_bytes(type_code, value)
                    .expect("validated binary JSON string"),
            )
            .unwrap_or("");
            parse_float(text)
        }
        _ => truncated_zero(),
    }
}

/// Actual storage conversion without statement diagnostics or extra temporal
/// rounding. The existing visible decimal formatter owns decimal-to-f64 policy.
pub fn native_datum_to_f64(
    input: NativeNumericInput<'_>,
) -> Result<NativeFloatConversion, NativeNumericError> {
    use NativeNumericInput as I;
    Ok(match input {
        I::Int(value) => exact_float(value as f64),
        I::UInt(value) => exact_float(value as f64),
        I::Real(value) => exact_float(value),
        I::Float32(value) => exact_float(f64::from(value as f32)),
        I::String(bytes) | I::Bytes(bytes) => {
            parse_float(std::str::from_utf8(bytes).map_err(NativeNumericError::InvalidUtf8)?)
        }
        I::Time(value) => exact_float(native_time_to_number(value).as_ref().to_f64()),
        I::Duration(value) => exact_float(native_duration_to_number(value).as_ref().to_f64()),
        I::Decimal(value) => exact_float(value.to_f64()),
        I::Enum(value) | I::Set(value) => exact_float(value as f64),
        I::BinaryLiteral(bytes) | I::Bit(bytes) => {
            let outcome = native_binary_literal_to_int(bytes);
            NativeFloatConversion {
                value: outcome.value() as f64,
                truncated: outcome.is_truncated(),
            }
        }
        I::Json { type_code, value } => native_json_to_float(type_code, value),
        I::Raw(_) | I::VectorFloat32(_) | I::Null | I::MinNotNull | I::MaxValue => {
            return Err(NativeNumericError::Unsupported);
        }
    })
}

/// Native ToBool keeps decimal raw-coefficient zero semantics and vector's
/// empty-storage zero definition. No JSON numeric-cast policy participates.
pub fn native_datum_to_bool(
    input: NativeNumericInput<'_>,
) -> Result<NativeBoolConversion, NativeNumericError> {
    use NativeNumericInput as I;
    Ok(match input {
        I::Int(value) => exact_bool(value != 0),
        I::UInt(value) => exact_bool(value != 0),
        I::Real(value) | I::Float32(value) => exact_bool(value != 0.0),
        I::String(bytes) | I::Bytes(bytes) => float_bool(parse_float(
            std::str::from_utf8(bytes).map_err(NativeNumericError::InvalidUtf8)?,
        )),
        I::Time(value) => exact_bool(value.raw != 0),
        I::Duration(value) => exact_bool(value.nanoseconds != 0),
        I::Decimal(value) => exact_bool(!value.is_zero()),
        I::Enum(value) | I::Set(value) => exact_bool(value != 0),
        I::BinaryLiteral(bytes) | I::Bit(bytes) => {
            let outcome = native_binary_literal_to_int(bytes);
            NativeBoolConversion {
                value: i64::from(outcome.value() != 0),
                truncated: outcome.is_truncated(),
            }
        }
        I::Json { type_code, value } => {
            // The constant document is intrinsically valid; still use its real
            // native construction path rather than substituting a numeric cast
            // or a hand-built tag/payload representation.
            let (zero_type, zero) = native_json_parse("0").expect("constant JSON zero is valid");
            exact_bool(
                compare_native_binary_json(type_code, value, zero_type, &zero) != Ordering::Equal,
            )
        }
        I::VectorFloat32(value) => exact_bool(!value.is_zero_value()),
        I::Raw(_) | I::Null | I::MinNotNull | I::MaxValue => {
            return Err(NativeNumericError::Unsupported);
        }
    })
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
    #[test]
    fn scalar_datum_selectors_keep_raw_zero_narrowing_utf8_and_temporal_rules() {
        let decimal = NativeDecimalParseRef {
            negative: false,
            digits: b"125",
            scale: 2,
            storage_scale: 2,
            declared_shape: None,
        };
        let time = NativeTemporalValue {
            raw: Time::native_core_from_fields(2020, 1, 2, 3, 4, 5, 500000),
            kind: TimeType::DateTime,
            fsp: 1,
        };
        for (input, expected) in [
            (I::Int(-2), -2.0),
            (I::UInt(u64::MAX), 18_446_744_073_709_551_616.0),
            (I::Real(1.25), 1.25),
            (I::Float32(16_777_217.0), 16_777_216.0),
            (I::Decimal(decimal), 1.25),
            (I::String(b"1.25"), 1.25),
            (I::Bytes(b"1.25"), 1.25),
            (I::Time(time), 20_200_102_030_405.5),
            (
                I::Duration(NativeDurationParts {
                    nanoseconds: 1_500_000_000,
                    fsp: 1,
                }),
                1.5,
            ),
            (I::Enum(u64::MAX), 18_446_744_073_709_551_616.0),
            (I::Set(1), 1.0),
            (I::Bit(&[1]), 1.0),
            (I::BinaryLiteral(&[1]), 1.0),
        ] {
            assert_eq!(
                native_datum_to_f64(input).unwrap(),
                NativeFloatConversion {
                    value: expected,
                    truncated: false
                }
            );
            assert_eq!(
                native_datum_to_bool(input).unwrap(),
                NativeBoolConversion {
                    value: 1,
                    truncated: false
                }
            );
        }
        assert_eq!(
            native_datum_to_f64(I::Float32(1e-50)).unwrap(),
            exact_float(0.0)
        );
        assert_eq!(
            native_datum_to_bool(I::Float32(1e-50)).unwrap(),
            exact_bool(true)
        );
        assert_eq!(
            native_datum_to_bool(I::Real(f64::NAN)).unwrap(),
            exact_bool(true)
        );
        let negative_zero = native_datum_to_f64(I::Real(-0.0)).unwrap();
        assert_eq!(negative_zero.value.to_bits(), (-0.0f64).to_bits());
        assert!(!negative_zero.truncated);
        assert_eq!(
            native_datum_to_bool(I::Real(-0.0)).unwrap(),
            exact_bool(false)
        );
        for input in [I::BinaryLiteral(&[1; 9]), I::Bit(&[1; 9])] {
            assert_eq!(
                native_datum_to_f64(input).unwrap(),
                NativeFloatConversion {
                    value: 18_446_744_073_709_551_616.0,
                    truncated: true
                }
            );
            assert_eq!(
                native_datum_to_bool(input).unwrap(),
                NativeBoolConversion {
                    value: 1,
                    truncated: true
                }
            );
        }
        for input in [I::String(b"12x"), I::Bytes(b"12x")] {
            assert_eq!(
                native_datum_to_f64(input).unwrap(),
                NativeFloatConversion {
                    value: 12.0,
                    truncated: true
                }
            );
            assert_eq!(
                native_datum_to_bool(input).unwrap(),
                NativeBoolConversion {
                    value: 1,
                    truncated: true
                }
            );
        }
        for input in [I::String(b"12\xff"), I::Bytes(b"12\xff")] {
            assert!(matches!(
                native_datum_to_f64(input),
                Err(NativeNumericError::InvalidUtf8(_))
            ));
            assert!(matches!(
                native_datum_to_bool(input),
                Err(NativeNumericError::InvalidUtf8(_))
            ));
        }
        let empty = NativeVectorFloat32::default();
        let zeros = NativeVectorFloat32::must_create(vec![0.0]);
        assert_eq!(
            native_datum_to_bool(I::VectorFloat32(&empty)).unwrap(),
            exact_bool(false)
        );
        assert_eq!(
            native_datum_to_bool(I::VectorFloat32(&zeros)).unwrap(),
            exact_bool(true)
        );
        assert!(matches!(
            native_datum_to_f64(I::VectorFloat32(&zeros)),
            Err(NativeNumericError::Unsupported)
        ));
        for input in [I::Null, I::MinNotNull, I::MaxValue, I::Raw(b"1")] {
            assert!(matches!(
                native_datum_to_f64(input),
                Err(NativeNumericError::Unsupported)
            ));
            assert!(matches!(
                native_datum_to_bool(input),
                Err(NativeNumericError::Unsupported)
            ));
        }
        for digits in [&b""[..], &b"000"[..]] {
            let raw = NativeDecimalParseRef {
                negative: true,
                digits,
                scale: u32::MAX,
                storage_scale: u32::MAX,
                declared_shape: Some((-1, -1)),
            };
            assert_eq!(
                native_datum_to_bool(I::Decimal(raw)).unwrap(),
                exact_bool(false)
            );
        }
        let hidden = NativeDecimalParseRef {
            negative: false,
            digits: b"01",
            scale: 0,
            storage_scale: 2,
            declared_shape: None,
        };
        assert_eq!(
            native_datum_to_bool(I::Decimal(hidden)).unwrap(),
            exact_bool(true)
        );
        let invalid = NativeDecimalParseRef {
            digits: &[255],
            ..decimal
        };
        assert!(std::panic::catch_unwind(|| native_datum_to_bool(I::Decimal(invalid))).is_err());
    }
    #[test]
    fn json_float_accessors_and_boolean_comparison_keep_distinct_native_domains() {
        for (tag, value, bool_value, float_value, truncated) in [
            (9, &[0; 8][..], 0, 0.0, false),
            (10, &[0; 8][..], 0, 0.0, false),
            (4, &[2][..], 1, 0.0, false),
            (4, &[0][..], 1, 0.0, true),
            (4, &[][..], 1, 0.0, true),
            (4, &[255][..], 1, 1.0, false),
            (12, &b"\x010"[..], 1, 0.0, false),
            (12, &b"\x0312x"[..], 1, 12.0, true),
            (12, &b"\x0312\xff"[..], 1, 0.0, true),
            (1, &[255][..], 1, 0.0, true),
            (3, &[255][..], 1, 0.0, true),
            (255, &[][..], 1, 0.0, true),
        ] {
            assert_eq!(
                native_json_to_float(tag, value),
                NativeFloatConversion {
                    value: float_value,
                    truncated
                }
            );
            assert_eq!(
                native_datum_to_bool(I::Json {
                    type_code: tag,
                    value
                })
                .unwrap(),
                NativeBoolConversion {
                    value: bool_value,
                    truncated: false
                }
            );
        }
        let negative_zero = (-0.0f64).to_le_bytes();
        assert_eq!(
            native_datum_to_bool(I::Json {
                type_code: 11,
                value: &negative_zero
            })
            .unwrap(),
            exact_bool(false)
        );
        let value = native_json_to_float(11, &negative_zero);
        assert_eq!(value.value.to_bits(), (-0.0f64).to_bits());
        assert!(!value.truncated);
        for raw in [
            f64::INFINITY.to_le_bytes(),
            f64::NEG_INFINITY.to_le_bytes(),
            f64::NAN.to_le_bytes(),
        ] {
            let value = native_json_to_float(11, &raw);
            assert_eq!(value.value.to_bits(), u64::from_le_bytes(raw));
            assert!(!value.truncated);
            assert_eq!(
                native_datum_to_bool(I::Json {
                    type_code: 11,
                    value: &raw
                })
                .unwrap(),
                exact_bool(true)
            );
        }
        assert_eq!(
            native_json_to_float(12, b"\x051e999"),
            NativeFloatConversion {
                value: f64::MAX,
                truncated: true
            }
        );
        for (tag, message) in [
            (9, "validated binary JSON integer"),
            (10, "validated binary JSON integer"),
            (11, "validated binary JSON float"),
            (12, "validated binary JSON string"),
        ] {
            let panic = std::panic::catch_unwind(|| native_json_to_float(tag, &[])).unwrap_err();
            let actual = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied());
            assert_eq!(actual, Some(message));
            // Boolean comparison has raw-byte/rank fallbacks, not these expects.
            assert_eq!(
                native_datum_to_bool(I::Json {
                    type_code: tag,
                    value: &[]
                })
                .unwrap(),
                exact_bool(true)
            );
        }
    }
}
