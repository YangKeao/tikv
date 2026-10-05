// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Actual-value signed conversion. Hybrid ordinals are data, not string names.
use std::str::Utf8Error;

use chrono::TimeZone;

use super::{
    mysql::{
        NativeDecimalParseRef, NativeVectorFloat32, binary_literal::native_binary_literal_to_int,
        time::NativeTemporalValue,
    },
    native_duration_convert::{NativeDurationParts, native_round_duration_fsp},
    native_integer_convert::{
        NativeIntegerConverted, NativeIntegerEvent, native_convert_uint_to_int,
        native_json_to_int64, native_str_to_int,
    },
    native_temporal_convert::native_time_round_frac,
    native_temporal_number::{native_duration_to_number, native_time_to_number},
    native_type_name::NativeTypeNameCode,
};

#[derive(Clone, Copy, Debug)]
pub enum NativeNumericInput<'a> {
    Int(i64),
    UInt(u64),
    Real(f64),
    Float32(f64),
    Decimal(NativeDecimalParseRef<'a>),
    String(&'a [u8]),
    Bytes(&'a [u8]),
    BinaryLiteral(&'a [u8]),
    Bit(&'a [u8]),
    Duration(NativeDurationParts),
    Enum(u64),
    Set(u64),
    Time(NativeTemporalValue),
    Json { type_code: u8, value: &'a [u8] },
    Raw(&'a [u8]),
    VectorFloat32(&'a NativeVectorFloat32),
    Null,
    MinNotNull,
    MaxValue,
}

#[derive(Debug)]
pub enum NativeNumericError {
    InvalidUtf8(Utf8Error),
    Comparison(String),
    Unsupported,
}

fn exact(value: i64) -> NativeIntegerConverted<i64> {
    NativeIntegerConverted { value, event: None }
}

fn decimal_to_i64(value: NativeDecimalParseRef<'_>) -> NativeIntegerConverted<i64> {
    match value.round_to_i64() {
        Some(value) => exact(value),
        None => NativeIntegerConverted {
            value: value.round_to_i64_saturating(),
            event: Some(NativeIntegerEvent::Truncated),
        },
    }
}

/// Native Datum.ToInt64, not explicit SQL CAST's separate unsigned wrapping.
pub fn native_datum_to_i64<TZ: TimeZone>(
    input: NativeNumericInput<'_>,
    zone: &TZ,
) -> Result<NativeIntegerConverted<i64>, NativeNumericError> {
    use NativeNumericInput as I;
    Ok(match input {
        I::Int(value) => exact(value),
        I::UInt(value) => NativeIntegerConverted {
            value: value.min(i64::MAX as u64) as i64,
            event: (value > i64::MAX as u64).then_some(NativeIntegerEvent::Truncated),
        },
        I::Real(value) | I::Float32(value) => {
            let rounded = value.round_ties_even();
            NativeIntegerConverted {
                value: rounded.clamp(i64::MIN as f64, i64::MAX as f64) as i64,
                event: (!(i64::MIN as f64..=i64::MAX as f64).contains(&rounded))
                    .then_some(NativeIntegerEvent::Truncated),
            }
        }
        I::String(value) | I::Bytes(value) => native_str_to_int(
            std::str::from_utf8(value).map_err(NativeNumericError::InvalidUtf8)?,
            false,
        ),
        I::Time(value) => {
            let rounded = native_time_round_frac(value, 0, zone)
                .map_err(|error| NativeNumericError::Comparison(error.to_string()))?;
            decimal_to_i64(native_time_to_number(rounded).as_ref())
        }
        I::Duration(value) => {
            let rounded = native_round_duration_fsp(value.nanoseconds, value.fsp, 0)
                .map_err(|error| NativeNumericError::Comparison(error.to_string()))?;
            decimal_to_i64(native_duration_to_number(rounded).as_ref())
        }
        I::Decimal(value) => decimal_to_i64(value),
        I::Enum(value) | I::Set(value) => exact(value.min(i64::MAX as u64) as i64),
        // DefaultStmtFlags: AllowNegativeToUnsigned | IgnoreZeroDateErr.
        I::Json { type_code, value } => {
            native_json_to_int64(type_code, value, false, (1 << 2) | (1 << 3))
        }
        I::BinaryLiteral(bytes) => {
            let outcome = native_binary_literal_to_int(bytes);
            if outcome.is_truncated() {
                NativeIntegerConverted {
                    value: 0,
                    event: Some(NativeIntegerEvent::Truncated),
                }
            } else {
                match native_convert_uint_to_int(
                    outcome.value(),
                    i64::MAX,
                    NativeTypeNameCode::Known(8),
                ) {
                    Ok(value) => exact(value),
                    Err((value, error)) => NativeIntegerConverted {
                        value,
                        event: Some(NativeIntegerEvent::Overflow(error)),
                    },
                }
            }
        }
        I::Bit(bytes) => {
            let outcome = native_binary_literal_to_int(bytes);
            NativeIntegerConverted {
                value: outcome.value() as i64,
                event: outcome
                    .is_truncated()
                    .then_some(NativeIntegerEvent::Truncated),
            }
        }
        I::Null | I::MinNotNull | I::MaxValue | I::Raw(_) | I::VectorFloat32(_) => {
            return Err(NativeNumericError::Unsupported);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_numeric_keeps_hybrid_float_and_literal_distinctions() {
        let convert = |value| native_datum_to_i64(value, &chrono::Utc).unwrap();
        let uint = convert(NativeNumericInput::UInt(u64::MAX));
        assert_eq!(uint.value, i64::MAX);
        assert!(matches!(uint.event, Some(NativeIntegerEvent::Truncated)));
        for input in [
            NativeNumericInput::Enum(u64::MAX),
            NativeNumericInput::Set(u64::MAX),
        ] {
            let result = convert(input);
            assert_eq!(result.value, i64::MAX);
            assert!(result.event.is_none());
        }
        assert_eq!(
            convert(NativeNumericInput::Float32(16777217.0)).value,
            16777217
        );
        let nan = convert(NativeNumericInput::Real(f64::NAN));
        assert_eq!(nan.value, 0);
        assert!(matches!(nan.event, Some(NativeIntegerEvent::Truncated)));
        let bytes = [255; 8];
        let literal = convert(NativeNumericInput::BinaryLiteral(&bytes));
        assert_eq!(literal.value, i64::MAX);
        assert!(matches!(
            literal.event,
            Some(NativeIntegerEvent::Overflow(_))
        ));
        let bit = convert(NativeNumericInput::Bit(&bytes));
        assert_eq!(bit.value, -1);
        assert!(bit.event.is_none());
        let wide = [1; 9];
        assert_eq!(convert(NativeNumericInput::BinaryLiteral(&wide)).value, 0);
        assert_eq!(convert(NativeNumericInput::Bit(&wide)).value, -1);
        assert!(matches!(
            native_datum_to_i64(NativeNumericInput::String(&[255]), &chrono::Utc),
            Err(NativeNumericError::InvalidUtf8(_))
        ));
        for input in [
            NativeNumericInput::Null,
            NativeNumericInput::MinNotNull,
            NativeNumericInput::MaxValue,
            NativeNumericInput::Raw(b"1"),
        ] {
            assert!(matches!(
                native_datum_to_i64(input, &chrono::Utc),
                Err(NativeNumericError::Unsupported)
            ));
        }
    }
}
