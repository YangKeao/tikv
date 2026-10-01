// Copyright 2017 TiKV Project Authors. Licensed under Apache-2.0.

use super::{JsonRef, JsonType};

const JSON_TYPE_BOOLEAN: &[u8] = b"BOOLEAN";
const JSON_TYPE_NONE: &[u8] = b"NULL";
const JSON_TYPE_INTEGER: &[u8] = b"INTEGER";
const JSON_TYPE_UNSIGNED_INTEGER: &[u8] = b"UNSIGNED INTEGER";
const JSON_TYPE_DOUBLE: &[u8] = b"DOUBLE";
const JSON_TYPE_STRING: &[u8] = b"STRING";
const JSON_TYPE_OBJECT: &[u8] = b"OBJECT";
const JSON_TYPE_ARRAY: &[u8] = b"ARRAY";
const JSON_TYPE_BIT: &[u8] = b"BIT";
const JSON_TYPE_BLOB: &[u8] = b"BLOB";
const JSON_TYPE_OPAQUE: &[u8] = b"OPAQUE";
const JSON_TYPE_DATE: &[u8] = b"DATE";
const JSON_TYPE_DATETIME: &[u8] = b"DATETIME";
const JSON_TYPE_TIME: &[u8] = b"TIME";

impl JsonRef<'_> {
    /// `json_type` is the implementation for
    /// <https://dev.mysql.com/doc/refman/5.7/en/json-attribute-functions.html#function_json-type>
    pub fn json_type(&self) -> &'static [u8] {
        let kind = self.get_type();
        // Keep the wire getters' original demand and validation boundaries:
        // literals read their first byte; opaque values only inspect the type.
        let literal_is_null = kind == JsonType::Literal && self.get_literal().is_none();
        let opaque_type = if kind == JsonType::Opaque {
            self.get_opaque_type().ok().map(|kind| kind as u8)
        } else {
            None
        };
        json_type_name(kind, literal_is_null, opaque_type)
    }
}

// The name selector is shared; callers retain their representation-specific
// literal and opaque validation policies before entering it.
pub(super) fn json_type_name(
    kind: JsonType,
    literal_is_null: bool,
    opaque_type: Option<u8>,
) -> &'static [u8] {
    match kind {
        JsonType::Object => JSON_TYPE_OBJECT,
        JsonType::Array => JSON_TYPE_ARRAY,
        JsonType::I64 => JSON_TYPE_INTEGER,
        JsonType::U64 => JSON_TYPE_UNSIGNED_INTEGER,
        JsonType::Double => JSON_TYPE_DOUBLE,
        JsonType::String => JSON_TYPE_STRING,
        JsonType::Literal if literal_is_null => JSON_TYPE_NONE,
        JsonType::Literal => JSON_TYPE_BOOLEAN,
        JsonType::Opaque => match opaque_type {
            Some(0x0f | 0xf9..=0xfe) => JSON_TYPE_BLOB,
            Some(0x10) => JSON_TYPE_BIT,
            _ => JSON_TYPE_OPAQUE,
        },
        JsonType::Date => JSON_TYPE_DATE,
        JsonType::Datetime | JsonType::Timestamp => JSON_TYPE_DATETIME,
        JsonType::Time => JSON_TYPE_TIME,
    }
}

#[cfg(test)]
mod tests {
    use super::{super::Json, *};

    #[test]
    fn test_type() {
        let test_cases = vec![
            (r#"{"a": "b"}"#, JSON_TYPE_OBJECT),
            (r#"["a", "b"]"#, JSON_TYPE_ARRAY),
            ("-5", JSON_TYPE_INTEGER),
            ("5", JSON_TYPE_INTEGER),
            ("18446744073709551615", JSON_TYPE_UNSIGNED_INTEGER),
            ("18446744073709551616", JSON_TYPE_DOUBLE),
            ("5.6", JSON_TYPE_DOUBLE),
            (r#""hello, world""#, JSON_TYPE_STRING),
            ("true", JSON_TYPE_BOOLEAN),
            ("null", JSON_TYPE_NONE),
        ];

        for (jstr, type_name) in test_cases {
            let json: Json = jstr.parse().unwrap();
            assert_eq!(json.as_ref().json_type(), type_name);
        }
    }
}
