// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native metadata policies for TiDB field values.

use super::native_type_name::NativeTypeNameCode;

const UNSPECIFIED_LENGTH: i64 = -1;
const MAX_DECIMAL_WIDTH: i64 = 65;
const MAX_DECIMAL_SCALE: i64 = 30;

/// Value shapes accepted by TiDB's default-field-type policies.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum NativeFieldValue {
    Null,
    Bool,
    Signed(i64),
    Unsigned(u64),
    StringLen(usize),
    Float32(f32),
    Float64(f64),
    BytesLen(usize),
    BitLiteralLen(usize),
    HexLiteralLen(usize),
    BinaryLiteralLen(usize),
    Date,
    Datetime {
        fsp: i64,
    },
    Timestamp {
        fsp: i64,
    },
    Duration {
        display_len: i64,
        fsp: i64,
    },
    Decimal {
        display_len: i64,
        fraction_digits: i64,
    },
    EnumLen(usize),
    SetLen(usize),
    Json,
    VectorFloat32,
    Unsupported,
}

/// How the caller applies charset and collation metadata to the result.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NativeFieldCharsetPolicy {
    Binary,
    Input,
    Utf8,
    Preserve,
}

/// Field metadata produced independently of protobuf-backed field types.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct NativeFieldTypeSpec {
    pub code: NativeTypeNameCode,
    pub flen: i64,
    pub decimal: i64,
    pub flags: u32,
    pub charset_policy: NativeFieldCharsetPolicy,
}

/// Mirrors `pkg/types.DefaultTypeForValue`.
pub fn native_default_field_type_for_value(
    value: NativeFieldValue,
    not_null_flag: u32,
    binary_flag: u32,
    unsigned_flag: u32,
    is_boolean_flag: u32,
) -> NativeFieldTypeSpec {
    let flags = if matches!(value, NativeFieldValue::Null) {
        0
    } else {
        not_null_flag
    };

    match value {
        NativeFieldValue::Null => spec(
            6,
            0,
            0,
            flags | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::Bool => spec(
            8,
            1,
            0,
            flags | is_boolean_flag | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::Signed(value) => spec(
            8,
            signed_display_len(value),
            0,
            flags | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::Unsigned(value) => spec(
            8,
            unsigned_display_len(value),
            0,
            flags | unsigned_flag | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::StringLen(len) => spec(
            253,
            len as i64,
            UNSPECIFIED_LENGTH,
            flags,
            NativeFieldCharsetPolicy::Input,
        ),
        NativeFieldValue::Float32(value) => spec(
            4,
            go_fixed_shortest_f32_len(value),
            UNSPECIFIED_LENGTH,
            flags | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::Float64(value) => spec(
            5,
            go_fixed_shortest_f64_len(value),
            UNSPECIFIED_LENGTH,
            flags | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::BytesLen(len) => spec(
            252,
            len as i64,
            UNSPECIFIED_LENGTH,
            flags | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::BitLiteralLen(len) => spec(
            253,
            (len * 3) as i64,
            0,
            flags | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::HexLiteralLen(len) => spec(
            253,
            (len * 3) as i64,
            0,
            flags | unsigned_flag | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::BinaryLiteralLen(len) => spec(
            253,
            len as i64,
            0,
            (flags | unsigned_flag | binary_flag) & !binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::Date => spec(
            10,
            10,
            UNSPECIFIED_LENGTH,
            flags | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::Datetime { fsp } => spec(
            12,
            19 + if fsp > 0 { fsp + 1 } else { 0 },
            fsp,
            flags | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::Timestamp { fsp } => spec(
            7,
            19 + if fsp > 0 { fsp + 1 } else { 0 },
            fsp,
            flags | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::Duration { display_len, fsp } => spec(
            11,
            if fsp > 0 { fsp + 1 } else { display_len },
            fsp,
            flags | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::Decimal {
            display_len,
            fraction_digits,
        } => spec(
            246,
            (display_len + 1).min(MAX_DECIMAL_WIDTH),
            fraction_digits.min(MAX_DECIMAL_SCALE),
            flags | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::EnumLen(len) => spec(
            247,
            len as i64,
            UNSPECIFIED_LENGTH,
            flags | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::SetLen(len) => spec(
            248,
            len as i64,
            UNSPECIFIED_LENGTH,
            flags | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::Json => spec(
            245,
            UNSPECIFIED_LENGTH,
            0,
            flags,
            NativeFieldCharsetPolicy::Utf8,
        ),
        NativeFieldValue::VectorFloat32 => spec(
            225,
            UNSPECIFIED_LENGTH,
            0,
            flags | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::Unsupported => spec(
            0,
            UNSPECIFIED_LENGTH,
            UNSPECIFIED_LENGTH,
            flags,
            NativeFieldCharsetPolicy::Utf8,
        ),
    }
}

/// Mirrors `pkg/parser/test_driver.DefaultTypeForValue`.
pub fn native_parser_default_field_type_for_value(
    value: NativeFieldValue,
    binary_flag: u32,
    unsigned_flag: u32,
    is_boolean_flag: u32,
) -> NativeFieldTypeSpec {
    match value {
        NativeFieldValue::Null => spec(6, 0, 0, binary_flag, NativeFieldCharsetPolicy::Binary),
        NativeFieldValue::Bool => spec(
            8,
            1,
            0,
            is_boolean_flag | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::Signed(value) => spec(
            8,
            signed_display_len(value),
            0,
            binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::Unsigned(value) => spec(
            8,
            unsigned_display_len(value),
            0,
            unsigned_flag | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::StringLen(len) => spec(
            253,
            len as i64,
            UNSPECIFIED_LENGTH,
            0,
            NativeFieldCharsetPolicy::Input,
        ),
        NativeFieldValue::Float32(value) => spec(
            4,
            go_fixed_shortest_f32_len(value),
            UNSPECIFIED_LENGTH,
            binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::Float64(value) => spec(
            5,
            go_fixed_shortest_f64_len(value),
            UNSPECIFIED_LENGTH,
            binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::BytesLen(len) => spec(
            252,
            len as i64,
            UNSPECIFIED_LENGTH,
            binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::BitLiteralLen(len) => spec(
            253,
            len as i64,
            0,
            binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::HexLiteralLen(len) => spec(
            253,
            (len * 3) as i64,
            0,
            unsigned_flag | binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::BinaryLiteralLen(len) => spec(
            16,
            (len * 8) as i64,
            0,
            (unsigned_flag | binary_flag) & !binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        NativeFieldValue::Decimal {
            display_len,
            fraction_digits,
        } => spec(
            246,
            display_len,
            fraction_digits,
            binary_flag,
            NativeFieldCharsetPolicy::Binary,
        ),
        _ => spec(
            0,
            UNSPECIFIED_LENGTH,
            UNSPECIFIED_LENGTH,
            0,
            NativeFieldCharsetPolicy::Preserve,
        ),
    }
}

const fn spec(
    code: u8,
    flen: i64,
    decimal: i64,
    flags: u32,
    charset_policy: NativeFieldCharsetPolicy,
) -> NativeFieldTypeSpec {
    NativeFieldTypeSpec {
        code: NativeTypeNameCode::Known(code),
        flen,
        decimal,
        flags,
        charset_policy,
    }
}

fn go_fixed_shortest_f32_len(value: f32) -> i64 {
    if value.is_nan() {
        3
    } else if value == f32::INFINITY || value == f32::NEG_INFINITY {
        4
    } else {
        value.to_string().len() as i64
    }
}

fn go_fixed_shortest_f64_len(value: f64) -> i64 {
    if value.is_nan() {
        3
    } else if value == f64::INFINITY || value == f64::NEG_INFINITY {
        4
    } else {
        value.to_string().len() as i64
    }
}

const fn signed_display_len(value: i64) -> i64 {
    if value == 0 {
        return 1;
    }
    let negative = value < 0;
    let mut magnitude = value.unsigned_abs();
    let mut digits = if negative { 1 } else { 0 };
    while magnitude != 0 {
        digits += 1;
        magnitude /= 10;
    }
    digits
}

const fn unsigned_display_len(mut value: u64) -> i64 {
    if value == 0 {
        return 1;
    }
    let mut digits = 0;
    while value != 0 {
        digits += 1;
        value /= 10;
    }
    digits
}

#[cfg(test)]
mod tests {
    use NativeFieldCharsetPolicy::{Binary, Input, Preserve, Utf8};
    use NativeTypeNameCode::Known;

    use super::*;

    const NOT_NULL: u32 = 1;
    const BINARY: u32 = 2;
    const UNSIGNED: u32 = 4;
    const IS_BOOLEAN: u32 = 8;

    fn runtime(value: NativeFieldValue) -> NativeFieldTypeSpec {
        native_default_field_type_for_value(value, NOT_NULL, BINARY, UNSIGNED, IS_BOOLEAN)
    }

    fn parser(value: NativeFieldValue) -> NativeFieldTypeSpec {
        native_parser_default_field_type_for_value(value, BINARY, UNSIGNED, IS_BOOLEAN)
    }

    #[test]
    fn field_value_policy_preserves_runtime_parser_width_flag_charset_and_caps() {
        assert_eq!(
            runtime(NativeFieldValue::Null),
            spec(6, 0, 0, BINARY, Binary)
        );
        assert_eq!(
            runtime(NativeFieldValue::Bool),
            spec(8, 1, 0, NOT_NULL | BINARY | IS_BOOLEAN, Binary)
        );
        assert_eq!(runtime(NativeFieldValue::Signed(i64::MIN)).flen, 20);
        assert_eq!(runtime(NativeFieldValue::Unsigned(u64::MAX)).flen, 20);
        assert_eq!(
            runtime(NativeFieldValue::StringLen(3)),
            spec(253, 3, -1, NOT_NULL, Input)
        );
        assert_eq!(runtime(NativeFieldValue::Float32(f32::NAN)).flen, 3);
        assert_eq!(runtime(NativeFieldValue::Float64(f64::INFINITY)).flen, 4);
        assert_eq!(runtime(NativeFieldValue::BitLiteralLen(2)).flen, 6);
        assert_eq!(parser(NativeFieldValue::BitLiteralLen(2)).flen, 2);
        assert_eq!(
            runtime(NativeFieldValue::BinaryLiteralLen(2)),
            spec(253, 2, 0, NOT_NULL | UNSIGNED, Binary)
        );
        assert_eq!(
            parser(NativeFieldValue::BinaryLiteralLen(2)),
            spec(16, 16, 0, UNSIGNED, Binary)
        );
        assert_eq!(runtime(NativeFieldValue::Date).code, Known(10));
        assert_eq!(parser(NativeFieldValue::Date), spec(0, -1, -1, 0, Preserve));
        assert_eq!(
            runtime(NativeFieldValue::Decimal {
                display_len: 100,
                fraction_digits: 40,
            }),
            spec(246, 65, 30, NOT_NULL | BINARY, Binary)
        );
        assert_eq!(
            parser(NativeFieldValue::Decimal {
                display_len: 100,
                fraction_digits: 40,
            }),
            spec(246, 100, 40, BINARY, Binary)
        );
        assert_eq!(
            runtime(NativeFieldValue::Unsupported),
            spec(0, -1, -1, NOT_NULL, Utf8)
        );
        assert_eq!(
            parser(NativeFieldValue::Unsupported),
            spec(0, -1, -1, 0, Preserve)
        );
    }
}
