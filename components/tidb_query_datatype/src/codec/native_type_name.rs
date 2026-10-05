// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native field-type labels, preserving named versus unknown source identity.

/// A native named variant's MySQL byte, or an actual Unknown variant. Unknown
/// must not be reinterpreted as Known even when their payload bytes coincide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NativeTypeNameCode {
    Known(u8),
    Unknown(u8),
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
