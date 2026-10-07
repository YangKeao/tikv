// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native field-type labels, preserving named versus unknown source identity.

/// A native named variant's MySQL byte, or an actual Unknown variant. Unknown
/// must not be reinterpreted as Known even when their payload bytes coincide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NativeTypeNameCode {
    Known(u8),
    Unknown(u8),
}

/// Exact `fieldTypeMergeRules` from `pkg/types/field_type.go`.
const MERGE_RULES: [[u8; 29]; 29] = [
    [
        246, 246, 246, 246, 5, 5, 246, 15, 0, 0, 15, 15, 15, 15, 15, 15, 15, 15, 246, 15, 15, 249,
        250, 251, 252, 15, 254, 15, 15,
    ],
    [
        246, 1, 2, 3, 4, 5, 1, 15, 8, 9, 15, 15, 15, 1, 15, 15, 8, 15, 246, 15, 15, 249, 250, 251,
        252, 15, 254, 15, 15,
    ],
    [
        246, 2, 2, 3, 4, 5, 2, 15, 8, 9, 15, 15, 15, 2, 15, 15, 8, 15, 246, 15, 15, 249, 250, 251,
        252, 15, 254, 15, 15,
    ],
    [
        246, 3, 3, 3, 5, 5, 3, 15, 8, 3, 15, 15, 15, 3, 15, 15, 8, 15, 246, 15, 15, 249, 250, 251,
        252, 15, 254, 15, 15,
    ],
    [
        5, 4, 4, 5, 4, 5, 4, 15, 4, 4, 15, 15, 15, 4, 15, 15, 5, 15, 5, 15, 15, 249, 250, 251, 252,
        15, 254, 15, 15,
    ],
    [
        5, 5, 5, 5, 5, 5, 5, 15, 5, 5, 15, 15, 15, 5, 15, 15, 5, 15, 5, 15, 15, 249, 250, 251, 252,
        15, 254, 15, 15,
    ],
    [
        246, 1, 2, 3, 4, 5, 6, 7, 8, 8, 10, 11, 12, 13, 14, 15, 16, 245, 246, 247, 248, 249, 250,
        251, 252, 15, 254, 255, 225,
    ],
    [
        15, 15, 15, 15, 15, 15, 7, 7, 15, 15, 12, 12, 12, 15, 14, 15, 15, 15, 15, 15, 15, 249, 250,
        251, 252, 15, 254, 15, 15,
    ],
    [
        246, 8, 8, 8, 5, 5, 8, 15, 8, 3, 15, 15, 15, 8, 14, 15, 8, 15, 246, 15, 15, 249, 250, 251,
        252, 15, 254, 15, 15,
    ],
    [
        246, 9, 9, 3, 4, 5, 9, 15, 8, 9, 15, 15, 15, 9, 14, 15, 8, 15, 246, 15, 15, 249, 250, 251,
        252, 15, 254, 15, 15,
    ],
    [
        15, 15, 15, 15, 15, 15, 10, 12, 15, 15, 10, 12, 12, 15, 14, 15, 15, 15, 15, 15, 15, 249,
        250, 251, 252, 15, 254, 15, 15,
    ],
    [
        15, 15, 15, 15, 15, 15, 11, 12, 15, 15, 12, 11, 12, 15, 14, 15, 15, 15, 15, 15, 15, 249,
        250, 251, 252, 15, 254, 15, 15,
    ],
    [
        15, 15, 15, 15, 15, 15, 12, 12, 15, 15, 12, 12, 12, 15, 14, 15, 15, 15, 15, 15, 15, 249,
        250, 251, 252, 15, 254, 15, 15,
    ],
    [
        0, 1, 2, 3, 4, 5, 13, 15, 8, 9, 15, 15, 15, 13, 15, 15, 8, 15, 246, 15, 15, 249, 250, 251,
        252, 15, 254, 15, 15,
    ],
    [
        15, 15, 15, 15, 15, 15, 14, 12, 15, 15, 14, 12, 12, 15, 14, 15, 15, 15, 15, 15, 15, 249,
        250, 251, 252, 15, 254, 15, 15,
    ],
    [
        15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 249,
        250, 251, 252, 15, 15, 15, 15,
    ],
    [
        15, 8, 8, 8, 5, 5, 16, 15, 8, 8, 15, 15, 15, 8, 15, 15, 16, 15, 246, 15, 15, 249, 250, 251,
        252, 15, 254, 15, 15,
    ],
    [
        15, 15, 15, 15, 15, 15, 245, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 245, 15, 15, 15, 251,
        251, 251, 251, 15, 254, 15, 15,
    ],
    [
        246, 246, 246, 246, 5, 5, 246, 15, 246, 246, 15, 15, 15, 246, 15, 15, 246, 15, 246, 15, 15,
        249, 250, 251, 252, 15, 254, 15, 15,
    ],
    [
        15, 15, 15, 15, 15, 15, 247, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 249,
        250, 251, 252, 15, 254, 15, 15,
    ],
    [
        15, 15, 15, 15, 15, 15, 248, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 249,
        250, 251, 252, 15, 254, 15, 15,
    ],
    [
        249, 249, 249, 249, 249, 249, 249, 249, 249, 249, 249, 249, 249, 249, 249, 249, 249, 251,
        249, 249, 249, 249, 250, 251, 252, 249, 249, 249, 251,
    ],
    [
        250, 250, 250, 250, 250, 250, 250, 250, 250, 250, 250, 250, 250, 250, 250, 250, 250, 251,
        250, 250, 250, 250, 250, 251, 250, 250, 250, 250, 251,
    ],
    [
        251, 251, 251, 251, 251, 251, 251, 251, 251, 251, 251, 251, 251, 251, 251, 251, 251, 251,
        251, 251, 251, 251, 251, 251, 251, 251, 251, 251, 251,
    ],
    [
        252, 252, 252, 252, 252, 252, 252, 252, 252, 252, 252, 252, 252, 252, 252, 252, 252, 251,
        252, 252, 252, 252, 250, 251, 252, 252, 252, 252, 251,
    ],
    [
        15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 249,
        250, 251, 252, 15, 15, 15, 15,
    ],
    [
        254, 254, 254, 254, 254, 254, 254, 254, 254, 254, 254, 254, 254, 254, 254, 15, 254, 254,
        254, 254, 254, 249, 250, 251, 252, 15, 254, 254, 254,
    ],
    [
        15, 15, 15, 15, 15, 15, 255, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 249,
        250, 251, 252, 15, 254, 255, 15,
    ],
    [
        15, 15, 15, 15, 15, 15, 225, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 251,
        251, 251, 251, 15, 254, 15, 225,
    ],
];

const fn native_type_index(code: NativeTypeNameCode) -> usize {
    match code {
        NativeTypeNameCode::Known(0) => 0,
        NativeTypeNameCode::Known(1) => 1,
        NativeTypeNameCode::Known(2) => 2,
        NativeTypeNameCode::Known(3) => 3,
        NativeTypeNameCode::Known(4) => 4,
        NativeTypeNameCode::Known(5) => 5,
        NativeTypeNameCode::Known(6) => 6,
        NativeTypeNameCode::Known(7) => 7,
        NativeTypeNameCode::Known(8) => 8,
        NativeTypeNameCode::Known(9) => 9,
        NativeTypeNameCode::Known(10) => 10,
        NativeTypeNameCode::Known(11) => 11,
        NativeTypeNameCode::Known(12) => 12,
        NativeTypeNameCode::Known(13) => 13,
        NativeTypeNameCode::Known(14) => 14,
        NativeTypeNameCode::Known(15) => 15,
        NativeTypeNameCode::Known(16) => 16,
        NativeTypeNameCode::Known(245) => 17,
        NativeTypeNameCode::Known(246) => 18,
        NativeTypeNameCode::Known(247) => 19,
        NativeTypeNameCode::Known(248) => 20,
        NativeTypeNameCode::Known(249) => 21,
        NativeTypeNameCode::Known(250) => 22,
        NativeTypeNameCode::Known(251) => 23,
        NativeTypeNameCode::Known(252) => 24,
        NativeTypeNameCode::Known(253) => 25,
        NativeTypeNameCode::Known(254) => 26,
        NativeTypeNameCode::Known(255) => 27,
        NativeTypeNameCode::Known(225) => 28,
        // Go's `fieldTypeIndexes[tp]` is a map lookup without an `ok` check,
        // so every unknown or unregistered byte uses the zero-value index.
        NativeTypeNameCode::Known(_) | NativeTypeNameCode::Unknown(_) => 0,
    }
}

pub const fn native_merge_field_type(
    left: NativeTypeNameCode,
    right: NativeTypeNameCode,
) -> NativeTypeNameCode {
    NativeTypeNameCode::Known(MERGE_RULES[native_type_index(left)][native_type_index(right)])
}

pub const fn native_default_field_length_and_decimal(code: NativeTypeNameCode) -> (i64, i64) {
    use NativeTypeNameCode::Known;
    match code {
        Known(16) => (1, 0),
        Known(1) => (4, 0),
        Known(2) => (6, 0),
        Known(9) => (9, 0),
        Known(3) => (11, 0),
        Known(8) => (20, 0),
        Known(5) => (22, -1),
        Known(4) => (12, -1),
        Known(246) => (10, 0),
        Known(11) => (10, 0),
        Known(10) => (10, 0),
        Known(7) => (19, 0),
        Known(12) => (19, 0),
        Known(13) => (4, 0),
        Known(254) => (1, 0),
        Known(15 | 253) => (5, 0),
        Known(249) => (255, 0),
        Known(252) => (65_535, 0),
        Known(250) => (16_777_215, 0),
        Known(251 | 245) => (4_294_967_295, 0),
        Known(6) => (0, 0),
        Known(247 | 248) => (-1, 0),
        _ => (-1, -1),
    }
}

pub const fn native_default_field_length_and_decimal_for_cast(
    code: NativeTypeNameCode,
) -> (i64, i64) {
    use NativeTypeNameCode::Known;
    match code {
        Known(254) => (0, -1),
        Known(10) => (10, 0),
        Known(12) => (19, 0),
        Known(246) => (10, 0),
        Known(11) => (10, 0),
        Known(8) => (22, 0),
        Known(5) => (22, -1),
        Known(4) => (12, -1),
        Known(245) => (4_194_304, 0),
        _ => (-1, -1),
    }
}

pub const fn native_type_is_fractionable(code: NativeTypeNameCode) -> bool {
    matches!(code, NativeTypeNameCode::Known(12 | 11 | 7))
}

pub const fn native_type_is_time(code: NativeTypeNameCode) -> bool {
    matches!(code, NativeTypeNameCode::Known(12 | 10 | 7))
}

pub const fn native_type_is_float(code: NativeTypeNameCode) -> bool {
    matches!(code, NativeTypeNameCode::Known(4))
}

pub const fn native_type_is_integer(code: NativeTypeNameCode) -> bool {
    matches!(code, NativeTypeNameCode::Known(1 | 2 | 9 | 3 | 8 | 13))
}

pub const fn native_mysql_is_integer_type(code: NativeTypeNameCode) -> bool {
    matches!(code, NativeTypeNameCode::Known(1 | 2 | 9 | 3 | 8))
}

pub const fn native_type_is_stored_as_integer(code: NativeTypeNameCode) -> bool {
    native_type_is_integer(code) || matches!(code, NativeTypeNameCode::Known(12 | 10 | 7 | 11))
}

pub const fn native_type_is_numeric(code: NativeTypeNameCode) -> bool {
    matches!(
        code,
        NativeTypeNameCode::Known(16 | 1 | 9 | 3 | 8 | 246 | 4 | 5 | 2)
    )
}

pub const fn native_type_is_temporal(code: NativeTypeNameCode) -> bool {
    matches!(code, NativeTypeNameCode::Known(11 | 12 | 7 | 10 | 14))
}

pub const fn native_type_is_temporal_with_date(code: NativeTypeNameCode) -> bool {
    native_type_is_time(code)
}

/// Converts a source type label to its native code, including blob/binary
/// aliases.
pub fn native_str_to_type(label: &str) -> NativeTypeNameCode {
    use NativeTypeNameCode::Known;
    let label = label
        .replacen("blob", "text", 1)
        .replacen("binary", "char", 1);
    match label.as_str() {
        "bit" => Known(16),
        "text" => Known(252),
        "date" => Known(10),
        "datetime" => Known(12),
        "unspecified" => Known(0),
        "decimal" => Known(246),
        "double" => Known(5),
        "enum" => Known(247),
        "float" => Known(4),
        "geometry" => Known(255),
        "vector" => Known(225),
        "mediumint" => Known(9),
        "json" => Known(245),
        "int" => Known(3),
        "bigint" => Known(8),
        "longtext" => Known(251),
        "mediumtext" => Known(250),
        "null" => Known(6),
        "set" => Known(248),
        "smallint" => Known(2),
        "char" => Known(254),
        "time" => Known(11),
        "timestamp" => Known(7),
        "tinyint" => Known(1),
        "tinytext" => Known(249),
        "varchar" => Known(15),
        "var_string" => Known(253),
        "year" => Known(13),
        _ => Known(0),
    }
}

/// Returns the source storage-width estimate for field metadata.
pub const fn native_field_storage_length(
    code: NativeTypeNameCode,
    flen: i64,
    decimal: i64,
    var_storage_len: i64,
) -> i64 {
    use NativeTypeNameCode::Known;
    match code {
        Known(1 | 2 | 9 | 3 | 8 | 5 | 4 | 13 | 11 | 10 | 12 | 7 | 247 | 248 | 16) => 8,
        Known(246) => {
            const DIGITS_TO_BYTES: [i64; 10] = [0, 1, 1, 2, 2, 3, 3, 4, 4, 4];
            let integer = flen - decimal;
            integer / 9 * 4
                + DIGITS_TO_BYTES[(integer % 9) as usize]
                + decimal / 9 * 4
                + DIGITS_TO_BYTES[(decimal % 9) as usize]
        }
        _ => var_storage_len,
    }
}

/// Native TypeStr uses no charset alias, even for binary source metadata.
pub fn native_type_str(code: NativeTypeNameCode) -> &'static str {
    native_type_to_str(code, "")
}

/// Native TypeToStr applies aliases only for the exact charset name `binary`.
pub fn native_type_to_str(code: NativeTypeNameCode, charset: &str) -> &'static str {
    let NativeTypeNameCode::Known(code) = code else {
        return "";
    };
    let binary = charset == "binary";
    match code {
        0 => "unspecified",
        1 => "tinyint",
        2 => "smallint",
        3 => "int",
        4 => "float",
        5 => "double",
        6 => {
            if binary {
                "binary"
            } else {
                "null"
            }
        }
        7 => "timestamp",
        8 => "bigint",
        9 => "mediumint",
        10 => "date",
        11 => "time",
        12 => "datetime",
        13 => "year",
        15 => {
            if binary {
                "varbinary"
            } else {
                "varchar"
            }
        }
        16 => "bit",
        0xe1 => "vector",
        0xf5 => "json",
        0xf6 => "decimal",
        0xf7 => "enum",
        0xf8 => "set",
        0xf9 => {
            if binary {
                "tinyblob"
            } else {
                "tinytext"
            }
        }
        0xfa => {
            if binary {
                "mediumblob"
            } else {
                "mediumtext"
            }
        }
        0xfb => {
            if binary {
                "longblob"
            } else {
                "longtext"
            }
        }
        0xfc => {
            if binary {
                "blob"
            } else {
                "text"
            }
        }
        0xfd => "var_string",
        0xfe => {
            if binary {
                "binary"
            } else {
                "char"
            }
        }
        0xff => "geometry",
        // Includes the named NewDate variant, whose original label is empty.
        _ => "",
    }
}

/// Escapes a value using TiDB's native output formatting rules.
pub fn native_output_format(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\0' => output.push_str("\\0"),
            '\'' => output.push_str("''"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            _ => output.push(ch),
        }
    }
    output
}

/// Renders the native equivalent of TiDB's compact field-type description.
pub fn native_compact_field_type<I, S>(
    code: NativeTypeNameCode,
    charset: &str,
    flen: i64,
    decimal: i64,
    zerofill: bool,
    strict_integer_display_width: bool,
    elements: I,
) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    use NativeTypeNameCode::Known;

    let (default_flen, default_decimal) = native_default_field_length_and_decimal(code);
    let decimal_not_default = decimal != default_decimal && decimal != 0 && decimal != -1;
    let display_flen = if flen == -1 { default_flen } else { flen };
    let display_decimal = if decimal == -1 {
        default_decimal
    } else {
        decimal
    };

    let suffix = match code {
        Known(247 | 248) => {
            let mut suffix = String::from("('");
            let mut first = true;
            for element in elements {
                if !first {
                    suffix.push_str("','");
                }
                first = false;
                suffix.push_str(&native_output_format(element.as_ref()));
            }
            suffix.push_str("')");
            suffix
        }
        Known(7 | 12 | 11) if decimal_not_default => format!("({display_decimal})"),
        Known(5 | 4) if decimal_not_default => {
            format!("({display_flen},{display_decimal})")
        }
        Known(246) => format!("({display_flen},{display_decimal})"),
        Known(16 | 15 | 254 | 253) => format!("({display_flen})"),
        Known(1) if !strict_integer_display_width || zerofill || display_flen == 1 => {
            format!("({display_flen})")
        }
        Known(2 | 9 | 3 | 8) if !strict_integer_display_width || zerofill => {
            format!("({display_flen})")
        }
        Known(13) => format!("({flen})"),
        Known(225) if flen != -1 => format!("({flen})"),
        Known(6) => "(0)".to_owned(),
        Known(_) | NativeTypeNameCode::Unknown(_) => String::new(),
    };

    native_type_to_str(code, charset).to_owned() + &suffix
}

pub fn native_restore_as_cast_type(
    code: NativeTypeNameCode,
    is_array: bool,
    flen: i64,
    decimal: i64,
    unsigned: bool,
    has_binary_flag: bool,
    charset: &str,
    collation: &str,
    explicit_charset: bool,
) -> String {
    use NativeTypeNameCode::Known;

    let mut restored = match code {
        Known(253 | 254) => {
            let binary = charset == "binary" && collation == "binary";
            let mut restored = if binary {
                "BINARY".to_owned()
            } else {
                "CHAR".to_owned()
            };
            if flen != -1 {
                restored.push_str(&format!("({flen})"));
            }
            if explicit_charset && !binary {
                if has_binary_flag {
                    restored.push_str(" BINARY");
                }
                if charset != "binary" && charset != "utf8mb4" {
                    restored.push_str(" CHARSET ");
                    restored.push_str(&charset.to_uppercase());
                }
            }
            restored
        }
        Known(10) => "DATE".to_owned(),
        Known(12) => {
            if decimal > 0 {
                format!("DATETIME({decimal})")
            } else {
                "DATETIME".to_owned()
            }
        }
        Known(246) => {
            if flen > 0 && decimal > 0 {
                format!("DECIMAL({flen}, {decimal})")
            } else if flen > 0 {
                format!("DECIMAL({flen})")
            } else {
                "DECIMAL".to_owned()
            }
        }
        Known(11) => {
            if decimal > 0 {
                format!("TIME({decimal})")
            } else {
                "TIME".to_owned()
            }
        }
        Known(8) => {
            if unsigned {
                "UNSIGNED".to_owned()
            } else {
                "SIGNED".to_owned()
            }
        }
        Known(245) => "JSON".to_owned(),
        Known(5) => "DOUBLE".to_owned(),
        Known(4) => "FLOAT".to_owned(),
        Known(13) => "YEAR".to_owned(),
        Known(225) => "VECTOR".to_owned(),
        Known(_) | NativeTypeNameCode::Unknown(_) => String::new(),
    };
    if is_array {
        restored.push_str(" ARRAY");
    }
    restored
}

#[cfg(test)]
#[test]
fn native_type_names_keep_known_unknown_identity_and_exact_charset_aliases() {
    use NativeTypeNameCode::{Known, Unknown};
    let labels = [
        (0, "unspecified", "unspecified"),
        (1, "tinyint", "tinyint"),
        (2, "smallint", "smallint"),
        (3, "int", "int"),
        (4, "float", "float"),
        (5, "double", "double"),
        (6, "null", "binary"),
        (7, "timestamp", "timestamp"),
        (8, "bigint", "bigint"),
        (9, "mediumint", "mediumint"),
        (10, "date", "date"),
        (11, "time", "time"),
        (12, "datetime", "datetime"),
        (13, "year", "year"),
        (14, "", ""),
        (15, "varchar", "varbinary"),
        (16, "bit", "bit"),
        (0xe1, "vector", "vector"),
        (0xf5, "json", "json"),
        (0xf6, "decimal", "decimal"),
        (0xf7, "enum", "enum"),
        (0xf8, "set", "set"),
        (0xf9, "tinytext", "tinyblob"),
        (0xfa, "mediumtext", "mediumblob"),
        (0xfb, "longtext", "longblob"),
        (0xfc, "text", "blob"),
        (0xfd, "var_string", "var_string"),
        (0xfe, "char", "binary"),
        (0xff, "geometry", "geometry"),
    ];
    for raw in u8::MIN..=u8::MAX {
        let (_, plain, binary) = labels
            .iter()
            .find(|(code, ..)| *code == raw)
            .copied()
            .unwrap_or((raw, "", ""));
        assert_eq!(native_type_str(Known(raw)), plain, "{raw}");
        for charset in ["", "binary", "BINARY", "binary\0", "binary ", "utf8mb4"] {
            assert_eq!(
                native_type_to_str(Known(raw), charset),
                if charset == "binary" { binary } else { plain },
                "{raw}/{charset:?}"
            );
            assert_eq!(
                native_type_to_str(Unknown(raw), charset),
                "",
                "{raw}/{charset:?}"
            );
        }
        assert_eq!(native_type_str(Unknown(raw)), "");
    }
}

#[cfg(test)]
#[test]
fn field_code_policy_preserves_default_tables_classifiers_and_unknown_identity() {
    use NativeTypeNameCode::{Known, Unknown};
    for (raw, expected, cast) in [
        (1, (4, 0), (-1, -1)),
        (8, (20, 0), (22, 0)),
        (13, (4, 0), (-1, -1)),
        (245, (4_294_967_295, 0), (4_194_304, 0)),
        (247, (-1, 0), (-1, -1)),
        (251, (4_294_967_295, 0), (-1, -1)),
        (254, (1, 0), (0, -1)),
    ] {
        assert_eq!(
            native_default_field_length_and_decimal(Known(raw)),
            expected
        );
        assert_eq!(
            native_default_field_length_and_decimal_for_cast(Known(raw)),
            cast
        );
    }
    for raw in u8::MIN..=u8::MAX {
        assert_eq!(
            native_default_field_length_and_decimal(Unknown(raw)),
            (-1, -1)
        );
        assert_eq!(
            native_default_field_length_and_decimal_for_cast(Unknown(raw)),
            (-1, -1)
        );
        assert!(!native_type_is_fractionable(Unknown(raw)));
        assert!(!native_type_is_time(Unknown(raw)));
        assert!(!native_type_is_float(Unknown(raw)));
        assert!(!native_type_is_integer(Unknown(raw)));
        assert!(!native_type_is_stored_as_integer(Unknown(raw)));
        assert!(!native_type_is_numeric(Unknown(raw)));
        assert!(!native_type_is_temporal(Unknown(raw)));
        assert!(!native_type_is_temporal_with_date(Unknown(raw)));
    }
    for raw in [7, 11, 12] {
        assert!(native_type_is_fractionable(Known(raw)));
    }
    for raw in [7, 10, 12] {
        assert!(native_type_is_time(Known(raw)));
        assert!(native_type_is_temporal_with_date(Known(raw)));
    }
    assert!(native_type_is_float(Known(4)));
    assert!(native_type_is_integer(Known(13)));
    assert!(native_type_is_stored_as_integer(Known(11)));
    assert!(native_type_is_numeric(Known(246)));
    assert!(native_type_is_temporal(Known(14)));
    assert!(!native_type_is_temporal_with_date(Known(14)));
}

#[cfg(test)]
#[test]
fn field_decimal_meta_mysql_integer_classifier_keeps_named_unknown_identity() {
    use NativeTypeNameCode::{Known, Unknown};
    for raw in [1, 2, 9, 3, 8] {
        assert!(native_mysql_is_integer_type(Known(raw)), "{raw}");
        assert!(!native_mysql_is_integer_type(Unknown(raw)), "{raw}");
    }
    for raw in [0, 4, 5, 13, 16, 246, 253, 254] {
        assert!(!native_mysql_is_integer_type(Known(raw)), "{raw}");
        assert!(!native_mysql_is_integer_type(Unknown(raw)), "{raw}");
    }
}

#[cfg(test)]
#[test]
fn field_merge_table_preserves_matrix_and_zero_index_identity_policy() {
    use NativeTypeNameCode::{Known, Unknown};
    for (left, right, expected) in [
        (Known(1), Known(2), Known(2)),
        (Known(4), Known(3), Known(5)),
        (Known(245), Known(252), Known(251)),
        (Known(14), Known(10), Known(14)),
        (Unknown(17), Unknown(34), Known(246)),
        (Known(17), Known(34), Known(246)),
        (Unknown(1), Known(2), Known(246)),
    ] {
        assert_eq!(
            native_merge_field_type(left, right),
            expected,
            "{left:?}/{right:?}"
        );
    }
    for left in [
        Known(0),
        Known(1),
        Known(245),
        Known(255),
        Unknown(0),
        Unknown(255),
    ] {
        for right in [Known(0), Known(8), Known(254), Unknown(8)] {
            assert!(matches!(native_merge_field_type(left, right), Known(_)));
        }
    }
}

#[cfg(test)]
#[test]
fn field_name_storage_policy_preserves_aliases_identity_widths_and_decimal_packing() {
    use NativeTypeNameCode::{Known, Unknown};
    for (label, expected) in [
        ("blob", Known(252)),
        ("longblob", Known(251)),
        ("binary", Known(254)),
        ("varbinary", Known(15)),
        ("blobbinary", Known(0)),
        ("unknown", Known(0)),
    ] {
        assert_eq!(native_str_to_type(label), expected, "{label}");
    }
    for code in [1, 2, 3, 4, 5, 7, 8, 9, 10, 11, 12, 13, 16, 247, 248] {
        assert_eq!(
            native_field_storage_length(Known(code), 99, 77, -1),
            8,
            "{code}"
        );
    }
    assert_eq!(native_field_storage_length(Known(246), 10, 2, -1), 5);
    assert_eq!(native_field_storage_length(Known(246), 20, 10, -1), 10);
    assert_eq!(native_field_storage_length(Known(15), 10, 2, -1), -1);
    assert_eq!(native_field_storage_length(Unknown(246), 10, 2, -1), -1);
    assert!(
        std::panic::catch_unwind(|| native_field_storage_length(Known(246), 0, 1, -1)).is_err()
    );
}

#[cfg(test)]
#[test]
fn field_cast_render_preserves_charset_precision_signedness_unknown_and_array_grammar() {
    use NativeTypeNameCode::{Known, Unknown};
    assert_eq!(
        native_restore_as_cast_type(
            Known(253),
            false,
            4,
            -1,
            false,
            false,
            "binary",
            "binary",
            true
        ),
        "BINARY(4)"
    );
    assert_eq!(
        native_restore_as_cast_type(
            Known(254),
            false,
            3,
            -1,
            false,
            true,
            "latin1",
            "latin1_bin",
            true
        ),
        "CHAR(3) BINARY CHARSET LATIN1"
    );
    assert_eq!(
        native_restore_as_cast_type(
            Known(254),
            false,
            -1,
            -1,
            false,
            true,
            "utf8mb4",
            "utf8mb4_bin",
            true
        ),
        "CHAR BINARY"
    );
    for (code, flen, decimal, unsigned, expected) in [
        (12, -1, 3, false, "DATETIME(3)"),
        (246, 10, 2, false, "DECIMAL(10, 2)"),
        (246, 10, 0, false, "DECIMAL(10)"),
        (11, -1, 6, false, "TIME(6)"),
        (8, -1, -1, true, "UNSIGNED"),
        (8, -1, -1, false, "SIGNED"),
        (245, -1, -1, false, "JSON"),
    ] {
        assert_eq!(
            native_restore_as_cast_type(
                Known(code),
                false,
                flen,
                decimal,
                unsigned,
                false,
                "",
                "",
                false
            ),
            expected
        );
    }
    assert_eq!(
        native_restore_as_cast_type(
            Unknown(253),
            true,
            4,
            -1,
            false,
            false,
            "binary",
            "binary",
            true
        ),
        " ARRAY"
    );
    assert_eq!(
        native_restore_as_cast_type(Known(225), true, -1, -1, false, false, "", "", false),
        "VECTOR ARRAY"
    );
}

#[cfg(test)]
#[test]
fn field_compact_render_preserves_escaping_widths_precision_aliases_and_unknown_identity() {
    use NativeTypeNameCode::{Known, Unknown};
    assert_eq!(native_output_format("\0'\n\r\\😀"), "\\0''\\n\\r\\😀");
    assert_eq!(
        native_compact_field_type(Known(247), "", -1, -1, false, true, ["a'b", "x\n"]),
        "enum('a''b','x\\n')"
    );
    assert_eq!(
        native_compact_field_type(Known(1), "", 2, 0, false, true, std::iter::empty::<&str>()),
        "tinyint"
    );
    assert_eq!(
        native_compact_field_type(Known(1), "", 2, 0, true, true, std::iter::empty::<&str>()),
        "tinyint(2)"
    );
    assert_eq!(
        native_compact_field_type(Known(5), "", 7, 3, false, true, std::iter::empty::<&str>()),
        "double(7,3)"
    );
    assert_eq!(
        native_compact_field_type(
            Known(246),
            "",
            10,
            2,
            false,
            true,
            std::iter::empty::<&str>()
        ),
        "decimal(10,2)"
    );
    assert_eq!(
        native_compact_field_type(
            Known(15),
            "binary",
            4,
            0,
            false,
            true,
            std::iter::empty::<&str>()
        ),
        "varbinary(4)"
    );
    assert_eq!(
        native_compact_field_type(
            Unknown(1),
            "",
            2,
            0,
            true,
            false,
            std::iter::empty::<&str>()
        ),
        ""
    );
}
