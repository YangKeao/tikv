// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use std::{error::Error, fmt};

use super::native_type_name::NativeTypeNameCode;

/// Validates decimal precision and scale only for the native decimal type.
pub const fn native_decimal_metadata_valid(
    code: NativeTypeNameCode,
    decimal: i64,
    flen: i64,
) -> bool {
    if !matches!(code, NativeTypeNameCode::Known(246)) {
        return true;
    }
    decimal >= 0 && decimal <= 30 && flen > 0 && flen <= 65 && flen >= decimal
}

/// Computes scale separately so callers can retain its original update order.
pub const fn native_update_decimal_scale(
    code: NativeTypeNameCode,
    old_decimal: i64,
    decimal_delta: i64,
) -> Option<i64> {
    if !matches!(code, NativeTypeNameCode::Known(246)) {
        return None;
    }
    Some(if old_decimal < 0 {
        30
    } else {
        old_decimal + decimal_delta
    })
}

/// Computes precision without moving its arithmetic before a scale update.
pub const fn native_update_decimal_flen(old_decimal: i64, old_flen: i64, flen_delta: i64) -> i64 {
    if old_flen < 0 {
        65
    } else {
        let flen = old_flen + flen_delta + if old_decimal < 0 { 30 } else { 0 };
        if flen > 65 { 65 } else { flen }
    }
}

/// The value representation used to evaluate a built-in function.
///
/// This is the single Rust type for both `pkg/parser/types.EvalType` and the
/// alias exported by `pkg/types`. Keeping the alias surface as constants of
/// this type preserves Go's identity relationship without a second enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum NativeEvalType {
    /// Go `ETInt`.
    Int = 0,
    /// Go `ETReal`.
    Real = 1,
    /// Go `ETDecimal`.
    Decimal = 2,
    /// Go `ETString`.
    String = 3,
    /// Go `ETDatetime`.
    Datetime = 4,
    /// Go `ETTimestamp`.
    Timestamp = 5,
    /// Go `ETDuration`.
    Duration = 6,
    /// Go `ETJson`.
    Json = 7,
    /// Go `ETVectorFloat32`.
    VectorFloat32 = 8,
}

impl NativeEvalType {
    /// Every valid source discriminant in declaration order.
    pub const ALL: [Self; 9] = [
        Self::Int,
        Self::Real,
        Self::Decimal,
        Self::String,
        Self::Datetime,
        Self::Timestamp,
        Self::Duration,
        Self::Json,
        Self::VectorFloat32,
    ];

    /// Mirrors `EvalType.IsStringKind`.
    ///
    /// Vector values intentionally belong to this source-defined family even
    /// though they also have their own vector classification.
    pub const fn is_string_kind(self) -> bool {
        matches!(
            self,
            Self::String
                | Self::Datetime
                | Self::Timestamp
                | Self::Duration
                | Self::Json
                | Self::VectorFloat32
        )
    }

    /// Mirrors `EvalType.IsVectorKind`.
    pub const fn is_vector_kind(self) -> bool {
        matches!(self, Self::VectorFloat32)
    }

    /// Returns the exact text emitted by Go's `EvalType.String`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Int => "Int",
            Self::Real => "Real",
            Self::Decimal => "Decimal",
            Self::String => "String",
            Self::Datetime => "Datetime",
            Self::Timestamp => "Timestamp",
            Self::Duration => "Time",
            Self::Json => "Json",
            Self::VectorFloat32 => "VectorFloat32",
        }
    }
}

impl fmt::Display for NativeEvalType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<NativeEvalType> for u8 {
    fn from(eval_type: NativeEvalType) -> Self {
        eval_type as Self
    }
}

/// A byte outside the source-defined `EvalType` discriminant range.
///
/// Go can construct such a byte and panics only when formatting it. Rust
/// rejects it at the numeric boundary, so every constructed [`NativeEvalType`]
/// is safe to classify and display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidEvalType(u8);

// Keep the original derived Debug name while exposing the SDK boundary alias.
pub use InvalidEvalType as NativeInvalidEvalType;

impl NativeInvalidEvalType {
    /// Returns the rejected source byte.
    pub const fn value(self) -> u8 {
        self.0
    }
}

impl fmt::Display for NativeInvalidEvalType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid EvalType {}", self.0)
    }
}

impl Error for NativeInvalidEvalType {}

impl TryFrom<u8> for NativeEvalType {
    type Error = NativeInvalidEvalType;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Int),
            1 => Ok(Self::Real),
            2 => Ok(Self::Decimal),
            3 => Ok(Self::String),
            4 => Ok(Self::Datetime),
            5 => Ok(Self::Timestamp),
            6 => Ok(Self::Duration),
            7 => Ok(Self::Json),
            8 => Ok(Self::VectorFloat32),
            invalid => Err(NativeInvalidEvalType(invalid)),
        }
    }
}

// `pkg/types/eval_type.go` aliases both the type and every constant from
// `pkg/parser/types`; these constants reproduce that public alias surface while
// retaining exactly one Rust enum.
/// The `pkg/types.ETInt` alias.
pub const ET_INT: NativeEvalType = NativeEvalType::Int;
/// The `pkg/types.ETReal` alias.
pub const ET_REAL: NativeEvalType = NativeEvalType::Real;
/// The `pkg/types.ETDecimal` alias.
pub const ET_DECIMAL: NativeEvalType = NativeEvalType::Decimal;
/// The `pkg/types.ETString` alias.
pub const ET_STRING: NativeEvalType = NativeEvalType::String;
/// The `pkg/types.ETDatetime` alias.
pub const ET_DATETIME: NativeEvalType = NativeEvalType::Datetime;
/// The `pkg/types.ETTimestamp` alias.
pub const ET_TIMESTAMP: NativeEvalType = NativeEvalType::Timestamp;
/// The `pkg/types.ETDuration` alias.
pub const ET_DURATION: NativeEvalType = NativeEvalType::Duration;
/// The `pkg/types.ETJson` alias.
pub const ET_JSON: NativeEvalType = NativeEvalType::Json;
/// The `pkg/types.ETVectorFloat32` alias.
pub const ET_VECTOR_FLOAT32: NativeEvalType = NativeEvalType::VectorFloat32;

/// Mirrors the native parser FieldType.EvalType table. The caller supplies its
/// effective code (including ARRAY-to-JSON projection) without losing Unknown
/// identity, and the complete source flag word.
pub const fn native_field_eval_type(code: NativeTypeNameCode, flags: u64) -> NativeEvalType {
    let NativeTypeNameCode::Known(code) = code else {
        return NativeEvalType::String;
    };
    match code {
        1 | 2 | 3 | 8 | 9 | 13 | 16 => NativeEvalType::Int,
        4 | 5 => NativeEvalType::Real,
        246 => NativeEvalType::Decimal,
        10 | 12 => NativeEvalType::Datetime,
        7 => NativeEvalType::Timestamp,
        11 => NativeEvalType::Duration,
        245 => NativeEvalType::Json,
        225 => NativeEvalType::VectorFloat32,
        247 | 248 if flags & (1_u64 << 21) != 0 => NativeEvalType::Int,
        _ => NativeEvalType::String,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeBoundTemporalKind {
    Date,
    DateTime,
    Timestamp,
}
/// Actual bound payload, leaving only native datum/temporal/decimal constructor
/// projection outside this policy. FLOAT payloads are already narrowed to f32.
#[derive(Clone, Debug, PartialEq)]
pub enum NativeBoundValue {
    Null,
    Int(i64),
    UInt(u64),
    Real {
        value: f64,
        float32: bool,
    },
    StringByte(u8),
    DecimalText(String),
    DurationNanos(i64),
    Temporal {
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        microsecond: u32,
        kind: NativeBoundTemporalKind,
    },
}
pub fn native_type_bound(
    code: NativeTypeNameCode,
    flen: i64,
    decimal: i64,
    unsigned: bool,
    maximum: bool,
) -> NativeBoundValue {
    use NativeBoundValue as V;

    use super::{
        native_decimal_convert::native_bound_decimal_text,
        native_duration_convert::NATIVE_MAX_DURATION_NANOS,
        native_float_convert::native_get_max_float,
        native_integer_convert::{
            native_integer_signed_lower_bound, native_integer_signed_upper_bound,
            native_integer_unsigned_upper_bound,
        },
    };
    let NativeTypeNameCode::Known(raw) = code else {
        return V::Null;
    };
    match raw {
        1 | 2 | 9 | 3 | 8 => {
            if unsigned {
                V::UInt(if maximum {
                    native_integer_unsigned_upper_bound(code)
                } else {
                    0
                })
            } else {
                V::Int(if maximum {
                    native_integer_signed_upper_bound(code)
                } else {
                    native_integer_signed_lower_bound(code)
                })
            }
        }
        4 | 5 => {
            let value = native_get_max_float(flen as i32, decimal as i32);
            let value = if raw == 4 {
                f64::from(value as f32)
            } else {
                value
            };
            V::Real {
                value: if maximum { value } else { -value },
                float32: raw == 4,
            }
        }
        15 | 253 | 252 | 249 | 250 | 251 | 254 => V::StringByte(if maximum { 250 } else { 1 }),
        246 => V::DecimalText(native_bound_decimal_text(flen, decimal, maximum)),
        11 => V::DurationNanos(if maximum {
            NATIVE_MAX_DURATION_NANOS
        } else {
            -NATIVE_MAX_DURATION_NANOS
        }),
        10 | 12 => {
            let kind = if raw == 10 {
                NativeBoundTemporalKind::Date
            } else {
                NativeBoundTemporalKind::DateTime
            };
            if maximum {
                V::Temporal {
                    year: 9999,
                    month: 12,
                    day: 31,
                    hour: 23,
                    minute: 59,
                    second: 59,
                    microsecond: 999_999,
                    kind,
                }
            } else {
                V::Temporal {
                    year: 1,
                    month: 1,
                    day: 1,
                    hour: 0,
                    minute: 0,
                    second: 0,
                    microsecond: 0,
                    kind,
                }
            }
        }
        7 => {
            let kind = NativeBoundTemporalKind::Timestamp;
            if maximum {
                V::Temporal {
                    year: 2038,
                    month: 1,
                    day: 19,
                    hour: 3,
                    minute: 14,
                    second: 7,
                    microsecond: 999_999,
                    kind,
                }
            } else {
                V::Temporal {
                    year: 1970,
                    month: 1,
                    day: 1,
                    hour: 0,
                    minute: 0,
                    second: 1,
                    microsecond: 0,
                    kind,
                }
            }
        }
        _ => V::Null,
    }
}

/// Source FieldType.SetFlenUnderLimit. The caller supplies its effective code
/// (including ARRAY's JSON view); negative sentinels are not lower-clamped.
pub fn native_field_flen_under_limit(code: NativeTypeNameCode, flen: i64) -> i64 {
    if code == NativeTypeNameCode::Known(246) {
        flen.min(crate::MAX_DECIMAL_WIDTH as i64)
    } else {
        flen
    }
}
/// Source FieldType.SetDecimalUnderLimit, without interpreting an Unknown byte
/// as its identically numbered known type or normalizing negative metadata.
pub fn native_field_decimal_under_limit(code: NativeTypeNameCode, decimal: i64) -> i64 {
    if code == NativeTypeNameCode::Known(246) {
        decimal.min(crate::codec::mysql::decimal::MAX_FRACTION as i64)
    } else {
        decimal
    }
}

#[cfg(test)]
mod bound_tests {
    use NativeBoundTemporalKind as K;
    use NativeBoundValue as V;
    use NativeTypeNameCode::{Known, Unknown};

    use super::*;
    #[test]
    fn native_type_bounds_keep_known_identity_unsigned_float_metadata_and_temporal_payloads() {
        for (code, low, high, upper) in [
            (1, -128, 127, 255),
            (2, -32768, 32767, 65535),
            (9, -8388608, 8388607, 16777215),
            (3, -2147483648, 2147483647, 4294967295),
            (8, i64::MIN, i64::MAX, u64::MAX),
        ] {
            assert_eq!(
                native_type_bound(Known(code), -1, -1, false, false),
                V::Int(low)
            );
            assert_eq!(
                native_type_bound(Known(code), -1, -1, false, true),
                V::Int(high)
            );
            assert_eq!(
                native_type_bound(Known(code), -1, -1, true, false),
                V::UInt(0)
            );
            assert_eq!(
                native_type_bound(Known(code), -1, -1, true, true),
                V::UInt(upper)
            );
        }
        for maximum in [false, true] {
            for unsigned in [false, true] {
                assert_eq!(
                    native_type_bound(Known(4), 3, 1, unsigned, maximum),
                    V::Real {
                        value: if maximum {
                            f64::from(99.9f32)
                        } else {
                            -f64::from(99.9f32)
                        },
                        float32: true
                    }
                );
                assert_eq!(
                    native_type_bound(
                        Known(5),
                        (1i64 << 32) + 3,
                        (1i64 << 32) + 1,
                        unsigned,
                        maximum
                    ),
                    V::Real {
                        value: if maximum { 99.9 } else { -99.9 },
                        float32: false
                    }
                );
                for code in [15, 253, 252, 249, 250, 251, 254] {
                    assert_eq!(
                        native_type_bound(Known(code), 0, -1, unsigned, maximum),
                        V::StringByte(if maximum { 250 } else { 1 })
                    );
                }
                assert_eq!(
                    native_type_bound(Known(246), 1, 3, unsigned, maximum),
                    V::DecimalText((if maximum { "9.999" } else { "-9.999" }).into())
                );
            }
        }
        assert_eq!(
            native_type_bound(Known(4), 100, 0, false, true),
            V::Real {
                value: f64::INFINITY,
                float32: true
            }
        );
        assert_eq!(
            native_type_bound(Known(4), 100, 0, true, false),
            V::Real {
                value: f64::NEG_INFINITY,
                float32: true
            }
        );
        assert_eq!(
            native_type_bound(Known(5), -1, -1, true, true),
            V::Real {
                value: -9.0,
                float32: false
            }
        );
        for code in [4, 5] {
            let V::Real { value, .. } = native_type_bound(Known(code), 0, 0, true, false) else {
                panic!("floating bound");
            };
            assert_eq!(value.to_bits(), (-0.0f64).to_bits());
        }
        for (code, kind) in [(10, K::Date), (12, K::DateTime)] {
            assert_eq!(
                native_type_bound(Known(code), -1, -1, false, true),
                V::Temporal {
                    year: 9999,
                    month: 12,
                    day: 31,
                    hour: 23,
                    minute: 59,
                    second: 59,
                    microsecond: 999_999,
                    kind
                }
            );
            assert_eq!(
                native_type_bound(Known(code), -1, -1, true, false),
                V::Temporal {
                    year: 1,
                    month: 1,
                    day: 1,
                    hour: 0,
                    minute: 0,
                    second: 0,
                    microsecond: 0,
                    kind
                }
            );
        }
        assert_eq!(
            native_type_bound(Known(7), -1, -1, false, true),
            V::Temporal {
                year: 2038,
                month: 1,
                day: 19,
                hour: 3,
                minute: 14,
                second: 7,
                microsecond: 999_999,
                kind: K::Timestamp
            }
        );
        assert_eq!(
            native_type_bound(Known(7), -1, -1, true, false),
            V::Temporal {
                year: 1970,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 1,
                microsecond: 0,
                kind: K::Timestamp
            }
        );
        use super::super::native_duration_convert::{
            NATIVE_MAX_DURATION_NANOS, native_number_to_duration,
        };
        assert_eq!(NATIVE_MAX_DURATION_NANOS, 3_020_399_000_000_000);
        for maximum in [false, true] {
            let expected = if maximum {
                NATIVE_MAX_DURATION_NANOS
            } else {
                -NATIVE_MAX_DURATION_NANOS
            };
            assert_eq!(
                native_type_bound(Known(11), -1, 6, true, maximum),
                V::DurationNanos(expected)
            );
            assert_eq!(
                native_number_to_duration(if maximum { 8_385_960 } else { i64::MIN }, 0)
                    .unwrap()
                    .value
                    .nanoseconds,
                expected
            );
        }
        for code in 0..=255 {
            for maximum in [false, true] {
                assert_eq!(
                    native_type_bound(Unknown(code), 3, 1, true, maximum),
                    V::Null
                );
                if !matches!(
                    code,
                    1 | 2
                        | 3
                        | 4
                        | 5
                        | 7
                        | 8
                        | 9
                        | 10
                        | 11
                        | 12
                        | 15
                        | 246
                        | 249
                        | 250
                        | 251
                        | 252
                        | 253
                        | 254
                ) {
                    assert_eq!(native_type_bound(Known(code), 3, 1, true, maximum), V::Null);
                }
            }
        }
    }
}

#[cfg(test)]
mod limit_tests {
    use super::*;
    #[test]
    fn field_limits_cap_only_actual_decimal_and_preserve_negative_metadata() {
        let decimal = NativeTypeNameCode::Known(246);
        for (value, width, scale) in [
            (i64::MIN, i64::MIN, i64::MIN),
            (-2, -2, -2),
            (-1, -1, -1),
            (0, 0, 0),
            (30, 30, 30),
            (31, 31, 30),
            (65, 65, 30),
            (66, 65, 30),
            (i64::MAX, 65, 30),
        ] {
            assert_eq!(native_field_flen_under_limit(decimal, value), width);
            assert_eq!(native_field_decimal_under_limit(decimal, value), scale);
        }
        for code in [
            NativeTypeNameCode::Known(245),
            NativeTypeNameCode::Known(3),
            NativeTypeNameCode::Known(253),
            NativeTypeNameCode::Unknown(246),
        ] {
            for value in [i64::MIN, -1, 0, 66, i64::MAX] {
                assert_eq!(native_field_flen_under_limit(code, value), value);
                assert_eq!(native_field_decimal_under_limit(code, value), value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_eval_type_preserves_source_identity_and_field_classification() {
        let aliases = [
            ET_INT,
            ET_REAL,
            ET_DECIMAL,
            ET_STRING,
            ET_DATETIME,
            ET_TIMESTAMP,
            ET_DURATION,
            ET_JSON,
            ET_VECTOR_FLOAT32,
        ];
        let names = [
            "Int",
            "Real",
            "Decimal",
            "String",
            "Datetime",
            "Timestamp",
            "Time",
            "Json",
            "VectorFloat32",
        ];
        for (index, value) in NativeEvalType::ALL.into_iter().enumerate() {
            assert_eq!(u8::from(value), index as u8);
            assert_eq!(NativeEvalType::try_from(index as u8), Ok(value));
            assert_eq!(aliases[index], value);
            assert_eq!(value.as_str(), names[index]);
            assert_eq!(value.to_string(), names[index]);
            assert_eq!(value.is_string_kind(), index >= 3);
            assert_eq!(value.is_vector_kind(), index == 8);
        }
        for byte in 9..=u8::MAX {
            let error = NativeEvalType::try_from(byte).unwrap_err();
            assert_eq!(error.value(), byte);
            assert_eq!(error.to_string(), format!("invalid EvalType {byte}"));
            let error: &dyn Error = &error;
            assert!(error.source().is_none());
            assert_eq!(format!("{error:?}"), format!("InvalidEvalType({byte})"));
        }
        const ENUM_INT: NativeEvalType =
            native_field_eval_type(NativeTypeNameCode::Known(247), 1 << 21);
        assert_eq!(ENUM_INT, NativeEvalType::Int);
        for (codes, expected) in [
            (&[1, 2, 3, 8, 9, 13, 16][..], NativeEvalType::Int),
            (&[4, 5][..], NativeEvalType::Real),
            (&[246][..], NativeEvalType::Decimal),
            (&[10, 12][..], NativeEvalType::Datetime),
            (&[7][..], NativeEvalType::Timestamp),
            (&[11][..], NativeEvalType::Duration),
            (&[245][..], NativeEvalType::Json),
            (&[225][..], NativeEvalType::VectorFloat32),
            (
                &[0, 6, 14, 15, 249, 250, 251, 252, 253, 254, 255][..],
                NativeEvalType::String,
            ),
        ] {
            for &code in codes {
                for flags in [0, 1 << 21, 1 << 63, u64::MAX] {
                    assert_eq!(
                        native_field_eval_type(NativeTypeNameCode::Known(code), flags),
                        expected
                    );
                }
            }
        }
        for code in [247, 248] {
            assert_eq!(
                native_field_eval_type(NativeTypeNameCode::Known(code), 1 << 63),
                NativeEvalType::String
            );
            assert_eq!(
                native_field_eval_type(NativeTypeNameCode::Known(code), (1 << 63) | (1 << 21)),
                NativeEvalType::Int
            );
        }
        for code in 0..=u8::MAX {
            assert_eq!(
                native_field_eval_type(NativeTypeNameCode::Unknown(code), 0),
                NativeEvalType::String
            );
            assert_eq!(
                native_field_eval_type(NativeTypeNameCode::Unknown(code), u64::MAX),
                NativeEvalType::String
            );
        }
    }

    #[test]
    fn field_decimal_meta_validity_and_delta_controller_preserve_limits_and_identity() {
        use NativeTypeNameCode::{Known, Unknown};
        for (decimal, flen, valid) in [
            (0, 1, true),
            (30, 65, true),
            (-1, 10, false),
            (31, 65, false),
            (1, 0, false),
            (2, 1, false),
            (1, 66, false),
        ] {
            assert_eq!(
                native_decimal_metadata_valid(Known(246), decimal, flen),
                valid
            );
        }
        for code in [Known(3), Known(13), Unknown(246)] {
            assert!(native_decimal_metadata_valid(code, -1, 0));
            assert_eq!(native_update_decimal_scale(code, 1, 2), None);
        }
        assert_eq!(native_update_decimal_scale(Known(246), 1, 2), Some(3));
        assert_eq!(native_update_decimal_scale(Known(246), -1, 99), Some(30));
        assert_eq!(native_update_decimal_flen(1, 10, 2), 12);
        assert_eq!(native_update_decimal_flen(-1, 10, 2), 42);
        assert_eq!(native_update_decimal_flen(1, -1, 99), 65);
        assert_eq!(native_update_decimal_flen(1, 64, 99), 65);
    }
}
