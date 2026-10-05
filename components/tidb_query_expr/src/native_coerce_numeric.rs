// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Existing scalar evaluator integer/truth coercions. These retain actual
//! integral storage and do not turn other numeric kinds into integer guesses.
use std::cmp::Ordering;

use tidb_query_datatype::codec::{
    mysql::{NativeDecimalParseValue, binary_literal::native_binary_literal_to_int},
    native_numeric::NativeNumericInput,
    native_scalar_convert::native_datum_to_bool,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeInteger {
    Signed(i64),
    Unsigned(u64),
}
pub fn native_integer_of(
    input: NativeNumericInput<'_>,
) -> Result<Option<NativeInteger>, &'static str> {
    use NativeNumericInput as I;
    Ok(match input {
        I::Int(value) => Some(NativeInteger::Signed(value)),
        I::UInt(value) => Some(NativeInteger::Unsigned(value)),
        I::BinaryLiteral(bytes) | I::Bit(bytes) => Some(NativeInteger::Unsigned(
            native_binary_literal_to_int(bytes).value(),
        )),
        I::Enum(value) | I::Set(value) => Some(NativeInteger::Unsigned(value)),
        I::String(_)
        | I::Bytes(_)
        | I::Decimal(_)
        | I::Real(_)
        | I::Float32(_)
        | I::Duration(_)
        | I::Time(_)
        | I::Json { .. }
        | I::Raw(_)
        | I::VectorFloat32(_)
        | I::Null => None,
        I::MinNotNull | I::MaxValue => return Err("range sentinel integer coercion"),
    })
}
pub fn native_integer_cmp(lhs: NativeInteger, rhs: NativeInteger) -> Ordering {
    use NativeInteger as I;
    match (lhs, rhs) {
        (I::Signed(a), I::Signed(b)) => a.cmp(&b),
        (I::Unsigned(a), I::Unsigned(b)) => a.cmp(&b),
        (I::Signed(a), I::Unsigned(_)) if a < 0 => Ordering::Less,
        (I::Signed(a), I::Unsigned(b)) => (a as u64).cmp(&b),
        (I::Unsigned(_), I::Signed(b)) if b < 0 => Ordering::Greater,
        (I::Unsigned(a), I::Signed(b)) => a.cmp(&(b as u64)),
    }
}
pub fn native_integer_bits(value: NativeInteger) -> u64 {
    match value {
        NativeInteger::Signed(value) => value as u64,
        NativeInteger::Unsigned(value) => value,
    }
}
pub fn native_integer_to_decimal(value: NativeInteger) -> NativeDecimalParseValue {
    match value {
        NativeInteger::Signed(value) => NativeDecimalParseValue::from_int(value),
        NativeInteger::Unsigned(value) => NativeDecimalParseValue::from_uint(value),
    }
}
pub fn native_integer_to_f64(value: NativeInteger) -> f64 {
    match value {
        NativeInteger::Signed(value) => value as f64,
        NativeInteger::Unsigned(value) => value as f64,
    }
}
/// NULL alone is unknown. Conversion errors share the original static error;
/// the datatype truncation event does not change truth or emit an effect here.
pub fn native_truthy(input: NativeNumericInput<'_>) -> Result<Option<bool>, &'static str> {
    if matches!(input, NativeNumericInput::Null) {
        return Ok(None);
    }
    match native_datum_to_bool(input) {
        Ok(converted) => Ok(Some(converted.value != 0)),
        Err(_) => Err("truth coercion of a non-SQL datum"),
    }
}

#[cfg(test)]
mod tests {
    use NativeInteger as I;
    use NativeNumericInput as N;
    use tidb_query_datatype::codec::{
        mysql::{
            Decimal, NativeDecimalParseRef, NativeVectorFloat32,
            time::{NativeTemporalValue, TimeType},
        },
        native_duration_convert::NativeDurationParts,
    };

    use super::*;
    #[test]
    fn native_integer_coercion_retains_ordinal_bits_signed_order_and_literal_partial_value() {
        for (input, expected) in [
            (N::Int(-1), I::Signed(-1)),
            (N::UInt(u64::MAX), I::Unsigned(u64::MAX)),
            (N::Enum(37), I::Unsigned(37)),
            (N::Set(u64::MAX), I::Unsigned(u64::MAX)),
            (N::BinaryLiteral(&[]), I::Unsigned(0)),
            (N::Bit(&[0, 1]), I::Unsigned(1)),
            (N::Bit(&[1; 9]), I::Unsigned(u64::MAX)),
            (N::BinaryLiteral(&[1; 9]), I::Unsigned(u64::MAX)),
        ] {
            assert_eq!(native_integer_of(input), Ok(Some(expected)));
        }
        let decimal = NativeDecimalParseRef {
            negative: false,
            digits: b"1",
            scale: 0,
            storage_scale: 0,
            declared_shape: None,
        };
        let vector = NativeVectorFloat32::default();
        for input in [
            N::String(b"1"),
            N::Bytes(b"1"),
            N::Decimal(decimal),
            N::Real(1.0),
            N::Float32(1.0),
            N::Duration(NativeDurationParts {
                nanoseconds: 1,
                fsp: 0,
            }),
            N::Time(NativeTemporalValue {
                raw: 1,
                kind: TimeType::DateTime,
                fsp: 0,
            }),
            N::Json {
                type_code: 9,
                value: &1i64.to_le_bytes(),
            },
            N::Raw(b"1"),
            N::VectorFloat32(&vector),
            N::Null,
        ] {
            assert_eq!(native_integer_of(input), Ok(None));
        }
        for input in [N::MinNotNull, N::MaxValue] {
            assert_eq!(
                native_integer_of(input),
                Err("range sentinel integer coercion")
            );
        }
        for (a, b, expected) in [
            (I::Signed(i64::MIN), I::Signed(0), Ordering::Less),
            (I::Unsigned(0), I::Unsigned(u64::MAX), Ordering::Less),
            (I::Signed(-1), I::Unsigned(0), Ordering::Less),
            (I::Signed(0), I::Unsigned(0), Ordering::Equal),
            (I::Signed(i64::MAX), I::Unsigned(u64::MAX), Ordering::Less),
            (I::Unsigned(0), I::Signed(-1), Ordering::Greater),
            (
                I::Unsigned(u64::MAX),
                I::Signed(i64::MAX),
                Ordering::Greater,
            ),
        ] {
            assert_eq!(native_integer_cmp(a, b), expected);
            assert_eq!(native_integer_cmp(b, a), expected.reverse());
        }
        assert_eq!(native_integer_bits(I::Signed(-1)), u64::MAX);
        assert_eq!(native_integer_bits(I::Signed(i64::MIN)), 1u64 << 63);
        assert_eq!(native_integer_bits(I::Unsigned(u64::MAX)), u64::MAX);
        for (input, text, float) in [
            (I::Signed(-1), "-1", -1.0),
            (
                I::Unsigned(u64::MAX),
                "18446744073709551615",
                18_446_744_073_709_551_616.0,
            ),
        ] {
            let decimal = native_integer_to_decimal(input);
            let value = decimal.as_ref();
            assert_eq!(
                Decimal::native_format_visible(
                    value.negative,
                    value.digits,
                    value.scale,
                    value.storage_scale
                ),
                text
            );
            assert_eq!(
                (value.scale, value.storage_scale, value.declared_shape),
                (0, 0, None)
            );
            assert_eq!(native_integer_to_f64(input), float);
        }
    }
    #[test]
    fn native_truth_keeps_null_only_unknown_json_comparison_event_discard_and_error_mapping() {
        assert_eq!(native_truthy(N::Null), Ok(None));
        let vector = NativeVectorFloat32::default();
        let zeros = NativeVectorFloat32::must_create(vec![0.0]);
        for input in [
            N::Int(0),
            N::UInt(0),
            N::Real(-0.0),
            N::String(b"0x"),
            N::String(b""),
            N::Bytes(b"abc"),
            N::Json {
                type_code: 9,
                value: &[0; 8],
            },
            N::VectorFloat32(&vector),
        ] {
            assert_eq!(native_truthy(input), Ok(Some(false)));
        }
        for input in [
            N::Int(-1),
            N::Float32(1e-50),
            N::Real(f64::NAN),
            N::String(b"1x"),
            N::Bytes(b".5"),
            N::Bit(&[1; 9]),
            N::Json {
                type_code: 4,
                value: &[0],
            },
            N::Json {
                type_code: 4,
                value: &[2],
            },
            N::VectorFloat32(&zeros),
        ] {
            assert_eq!(native_truthy(input), Ok(Some(true)));
        }
        for input in [
            N::MinNotNull,
            N::MaxValue,
            N::Raw(b"1"),
            N::String(b"1\xff"),
            N::Bytes(b"1\xff"),
        ] {
            assert_eq!(
                native_truthy(input),
                Err("truth coercion of a non-SQL datum")
            );
        }
        let invalid = NativeDecimalParseRef {
            negative: false,
            digits: &[255],
            scale: 0,
            storage_scale: 0,
            declared_shape: None,
        };
        assert!(std::panic::catch_unwind(|| native_truthy(N::Decimal(invalid))).is_err());
    }
}
