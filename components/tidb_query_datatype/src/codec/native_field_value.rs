// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native metadata policies for TiDB field values.

use super::native_type_name::NativeTypeNameCode;

// Go 1.25's 64-bit allocator size classes. TiDB's supported server targets
// are 64-bit; `growslice` rounding is defined by these byte classes and, for
// scanned allocations only, the runtime's 8-byte malloc-header threshold.
const GO_64_SIZE_CLASSES: &[usize] = &[
    8, 16, 24, 32, 48, 64, 80, 96, 112, 128, 144, 160, 176, 192, 208, 224, 240, 256, 288, 320, 352,
    384, 416, 448, 480, 512, 576, 640, 704, 768, 896, 1024, 1152, 1280, 1408, 1536, 1792, 2048,
    2304, 2688, 3072, 3200, 3456, 4096, 4864, 5376, 6144, 6528, 6784, 6912, 8192, 9472, 9728,
    10240, 10880, 12288, 13568, 14336, 16384, 18432, 19072, 20480, 21760, 24576, 27264, 28672,
    32768,
];

/// Whether a Go slice's element type contains pointers and therefore uses a
/// scanned allocation. Above the malloc-header threshold, scanned and noscan
/// slices with the same element width can have different observable caps.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NativeGoSliceElementLayout {
    /// The element type contains no pointers.
    NoPointers,
    /// The element type contains at least one pointer.
    PointerBearing,
}

fn native_go_64_round_allocation(bytes: usize, layout: NativeGoSliceElementLayout) -> usize {
    const MALLOC_HEADER: usize = 8;
    const MIN_HEADER_SIZE: usize = 8 * 64;
    const MAX_SMALL_SIZE: usize = 32_768;
    const PAGE_SIZE: usize = 8_192;

    if bytes <= MAX_SMALL_SIZE - MALLOC_HEADER {
        let header = usize::from(
            layout == NativeGoSliceElementLayout::PointerBearing && bytes > MIN_HEADER_SIZE,
        ) * MALLOC_HEADER;
        let requested = bytes + header;
        return GO_64_SIZE_CLASSES
            .iter()
            .copied()
            .find(|class| *class >= requested)
            .expect("small Go allocation has a size class")
            - header;
    }
    bytes
        .checked_add(PAGE_SIZE - 1)
        .expect("Go slice allocation overflow")
        & !(PAGE_SIZE - 1)
}

/// Computes Go 1.25's next 64-bit slice capacity for a concrete element
/// width/layout.
pub fn native_go_64_next_slice_capacity(
    new_len: usize,
    old_capacity: usize,
    element_size: usize,
    layout: NativeGoSliceElementLayout,
) -> usize {
    let double_capacity = old_capacity
        .checked_mul(2)
        .expect("Go slice capacity overflow");
    let mut candidate = if new_len > double_capacity {
        new_len
    } else if old_capacity < 256 {
        double_capacity
    } else {
        let mut grown = old_capacity;
        loop {
            grown = grown
                .checked_add((grown + 3 * 256) >> 2)
                .expect("Go slice capacity overflow");
            if grown >= new_len {
                break grown;
            }
        }
    };
    if candidate < new_len {
        candidate = new_len;
    }
    let bytes = candidate
        .checked_mul(element_size)
        .expect("Go slice allocation overflow");
    native_go_64_round_allocation(bytes, layout) / element_size
}

/// Computes the capacity reached while Go's array decoder exposes elements
/// one at a time.
pub fn native_go_64_slice_decode_capacity(
    mut capacity: usize,
    decoded_len: usize,
    element_size: usize,
    layout: NativeGoSliceElementLayout,
) -> usize {
    while capacity < decoded_len {
        capacity = native_go_64_next_slice_capacity(capacity + 1, capacity, element_size, layout);
    }
    capacity
}

const UNSPECIFIED_LENGTH: i64 = -1;
const MAX_DECIMAL_WIDTH: i64 = 65;
const MAX_DECIMAL_SCALE: i64 = 30;

/// JSON field names recognized by TiDB's field metadata decoder.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NativeFieldJsonTag {
    Tp,
    Flag,
    Flen,
    Decimal,
    Charset,
    Collate,
    Elems,
    ElemsIsBinaryLit,
    Array,
    Unknown,
}

/// Classifies a JSON field name using Go's `bytes.EqualFold` semantics.
pub const fn native_field_json_tag(incoming: &str) -> NativeFieldJsonTag {
    if ascii_tag_equal_fold(incoming, b"Tp") {
        NativeFieldJsonTag::Tp
    } else if ascii_tag_equal_fold(incoming, b"Flag") {
        NativeFieldJsonTag::Flag
    } else if ascii_tag_equal_fold(incoming, b"Flen") {
        NativeFieldJsonTag::Flen
    } else if ascii_tag_equal_fold(incoming, b"Decimal") {
        NativeFieldJsonTag::Decimal
    } else if ascii_tag_equal_fold(incoming, b"Charset") {
        NativeFieldJsonTag::Charset
    } else if ascii_tag_equal_fold(incoming, b"Collate") {
        NativeFieldJsonTag::Collate
    } else if ascii_tag_equal_fold(incoming, b"Elems") {
        NativeFieldJsonTag::Elems
    } else if ascii_tag_equal_fold(incoming, b"ElemsIsBinaryLit") {
        NativeFieldJsonTag::ElemsIsBinaryLit
    } else if ascii_tag_equal_fold(incoming, b"Array") {
        NativeFieldJsonTag::Array
    } else {
        NativeFieldJsonTag::Unknown
    }
}

const fn ascii_tag_equal_fold(incoming: &str, expected: &[u8]) -> bool {
    let incoming = incoming.as_bytes();
    let mut incoming_index = 0;
    let mut expected_index = 0;

    while expected_index < expected.len() {
        if incoming_index >= incoming.len() {
            return false;
        }

        let folded = match incoming[incoming_index] {
            byte @ b'A'..=b'Z' => {
                incoming_index += 1;
                byte + (b'a' - b'A')
            }
            byte @ b'a'..=b'z' => {
                incoming_index += 1;
                byte
            }
            0xC5 if incoming_index + 1 < incoming.len() && incoming[incoming_index + 1] == 0xBF => {
                incoming_index += 2;
                b's'
            }
            0xE2 if incoming_index + 2 < incoming.len()
                && incoming[incoming_index + 1] == 0x84
                && incoming[incoming_index + 2] == 0xAA =>
            {
                incoming_index += 3;
                b'k'
            }
            _ => return false,
        };

        let expected = expected[expected_index];
        let expected = if expected >= b'A' && expected <= b'Z' {
            expected + (b'a' - b'A')
        } else {
            expected
        };
        if folded != expected {
            return false;
        }
        expected_index += 1;
    }

    incoming_index == incoming.len()
}

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

#[cfg(test)]
#[test]
fn field_json_tag_policy_preserves_named_case_unicode_fold_and_unknown_keys() {
    use NativeFieldJsonTag::*;
    for (incoming, expected) in [
        ("Tp", Tp),
        ("tP", Tp),
        ("FLAG", Flag),
        ("flen", Flen),
        ("DECIMAL", Decimal),
        ("Char\u{17f}et", Charset),
        ("COLLATE", Collate),
        ("elems", Elems),
        ("ELEMSISBINARYLIT", ElemsIsBinaryLit),
        ("array", Array),
        ("TpX", Unknown),
        ("T", Unknown),
        ("\u{212a}", Unknown),
        ("Ｆlag", Unknown),
    ] {
        assert_eq!(native_field_json_tag(incoming), expected);
    }
    assert!(ascii_tag_equal_fold("\u{212a}", b"K"));
    assert!(!ascii_tag_equal_fold("kX", b"K"));
}

#[cfg(test)]
#[test]
fn go_slice_growth_policy_preserves_size_classes_scanned_headers_and_decode_steps() {
    use NativeGoSliceElementLayout::*;
    for (decoded_len, expected) in [(1, 8), (8, 8), (9, 16)] {
        assert_eq!(
            native_go_64_slice_decode_capacity(0, decoded_len, 1, NoPointers),
            expected
        );
    }
    for (decoded_len, expected) in [(1, 1), (2, 2), (3, 4), (5, 8)] {
        assert_eq!(
            native_go_64_slice_decode_capacity(0, decoded_len, 16, PointerBearing),
            expected
        );
    }
    assert_eq!(
        native_go_64_next_slice_capacity(257, 256, 16, NoPointers),
        512
    );
    assert_eq!(
        native_go_64_next_slice_capacity(257, 256, 16, PointerBearing),
        591
    );
    assert_eq!(
        native_go_64_next_slice_capacity(1000, 1, 1, NoPointers),
        1024
    );
}
