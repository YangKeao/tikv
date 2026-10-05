// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use std::{error::Error, fmt};

use super::native_type_name::NativeTypeNameCode;

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
}
