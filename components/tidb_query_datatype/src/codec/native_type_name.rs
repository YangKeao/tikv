// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native field-type labels, preserving named versus unknown source identity.

/// A native named variant's MySQL byte, or an actual Unknown variant. Unknown
/// must not be reinterpreted as Known even when their payload bytes coincide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NativeTypeNameCode {
    Known(u8),
    Unknown(u8),
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
