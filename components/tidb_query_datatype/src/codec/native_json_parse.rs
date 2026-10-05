// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native BinaryJSON text parsing and serde-value construction. This is not
//! the expression document parser, nor the typed-node build-then-encode path.
//! Container byte layout is owned by the shared writers; this module retains
//! the original serde traversal, key-preflight and error ordering.
use serde_json::Value;

use super::{
    mysql::json::{NativeBinaryJsonEncodeError, NativeJsonArrayWriter, NativeJsonObjectWriter},
    native_json_construct::{
        NativeJsonConstructError, native_json_encode_number, native_json_from_string,
        native_json_literal,
    },
};

const MAX_JSON_DEPTH: usize = 100;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeJsonParseError {
    EmptyDocument,
    TrailingValues,
    InvalidText,
    InvalidBinary,
    TooDeep,
    KeyTooLong,
}

/// Only the empty-document check trims. The first serde error alone receives
/// the trailing-values classification. After sanitization, ANY retry error is
/// InvalidText, including a trailing-values error newly exposed by the repair.
pub fn native_json_parse(text: &str) -> Result<(u8, Vec<u8>), NativeJsonParseError> {
    if text.trim().is_empty() {
        return Err(NativeJsonParseError::EmptyDocument);
    }
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            if error.to_string().contains("trailing characters") {
                return Err(NativeJsonParseError::TrailingValues);
            }
            let sanitized = replace_lone_surrogate_escapes(text);
            serde_json::from_str(&sanitized).map_err(|_| NativeJsonParseError::InvalidText)?
        }
    };
    native_json_from_value(&value)
}
/// Encode an actual serde tree with the native serde-container ordering.
pub fn native_json_from_value(value: &Value) -> Result<(u8, Vec<u8>), NativeJsonParseError> {
    encode_value(value, 0)
}
fn construct_error(error: NativeJsonConstructError) -> NativeJsonParseError {
    match error {
        NativeJsonConstructError::InvalidText => NativeJsonParseError::InvalidText,
        NativeJsonConstructError::InvalidBinary => NativeJsonParseError::InvalidBinary,
        NativeJsonConstructError::TooDeep => NativeJsonParseError::TooDeep,
        NativeJsonConstructError::KeyTooLong => NativeJsonParseError::KeyTooLong,
    }
}
fn writer_error(error: NativeBinaryJsonEncodeError) -> NativeJsonParseError {
    match error {
        NativeBinaryJsonEncodeError::InvalidBinary => NativeJsonParseError::InvalidBinary,
        NativeBinaryJsonEncodeError::TooDeep => NativeJsonParseError::TooDeep,
        NativeBinaryJsonEncodeError::KeyTooLong => NativeJsonParseError::KeyTooLong,
    }
}
fn encode_value(value: &Value, depth: usize) -> Result<(u8, Vec<u8>), NativeJsonParseError> {
    if depth > MAX_JSON_DEPTH {
        return Err(NativeJsonParseError::TooDeep);
    }
    match value {
        Value::Null => Ok(native_json_literal(0)),
        Value::Bool(true) => Ok(native_json_literal(1)),
        Value::Bool(false) => Ok(native_json_literal(2)),
        Value::Number(number) => native_json_encode_number(number).map_err(construct_error),
        Value::String(text) => Ok(native_json_from_string(text)),
        Value::Array(values) => {
            // Allocate before visiting children. Unlike typed-node encoding,
            // children are encoded and written one at a time.
            let mut writer = NativeJsonArrayWriter::new(values.len());
            for (index, value) in values.iter().enumerate() {
                let (tag, bytes) = encode_value(value, depth + 1)?;
                writer
                    .push_serde(index, tag, &bytes)
                    .map_err(writer_error)?;
            }
            writer.finish().map_err(writer_error)
        }
        Value::Object(values) => {
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
            // ALL keys are checked before ANY child is visited. The typed-node
            // path intentionally does this preflight only after its children.
            let mut writer =
                NativeJsonObjectWriter::new(entries.iter().map(|(key, _)| key.as_str()))
                    .map_err(writer_error)?;
            for (index, (key, value)) in entries.into_iter().enumerate() {
                writer.push_key(index, key).map_err(writer_error)?;
                let (tag, bytes) = encode_value(value, depth + 1)?;
                writer
                    .push_value_serde(index, tag, &bytes)
                    .map_err(writer_error)?;
            }
            writer.finish().map_err(writer_error)
        }
    }
}

// Deliberately the original GLOBAL byte scan, not a string-region-aware JSON
// sanitizer. Escaped backslashes elsewhere can also change on this retry path.
fn replace_lone_surrogate_escapes(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let is_surrogate_escape = bytes[index] == b'\\'
            && text.get(index..index + 2) == Some("\\u")
            && text.get(index + 2..index + 6).is_some_and(|hex| {
                u16::from_str_radix(hex, 16).is_ok_and(|value| (0xd800..=0xdfff).contains(&value))
            });
        if !is_surrogate_escape {
            output.push(bytes[index]);
            index += 1;
            continue;
        }
        let first = u16::from_str_radix(text.get(index + 2..index + 6).expect("checked above"), 16)
            .expect("checked above");
        let paired = if (0xd800..=0xdbff).contains(&first) {
            text.get(index + 6..index + 8)
                .filter(|next| *next == "\\u")
                .and_then(|_| text.get(index + 8..index + 12))
                .and_then(|hex| u16::from_str_radix(hex, 16).ok())
                .filter(|second| (0xdc00..=0xdfff).contains(second))
                .map(|second| {
                    let scalar = 0x10000
                        + ((u32::from(first) - 0xd800) << 10)
                        + (u32::from(second) - 0xdc00);
                    (index + 12, scalar)
                })
        } else {
            None
        };
        match paired {
            Some((next, scalar)) => {
                let ch = char::from_u32(scalar).unwrap_or('\u{fffd}');
                let mut encoded = [0; 4];
                output.extend_from_slice(ch.encode_utf8(&mut encoded).as_bytes());
                index = next;
            }
            None => {
                output.extend_from_slice("\\ufffd".as_bytes());
                index += 6;
            }
        }
    }
    String::from_utf8(output).unwrap_or_else(|_| text.to_owned())
}

#[cfg(test)]
mod tests {
    use NativeJsonParseError as E;

    use super::*;
    #[test]
    fn native_parser_keeps_fixed_bytes_global_surrogates_and_retry_error_classes() {
        for (text, tag, bytes) in [
            ("null", 4, vec![0]),
            ("true", 4, vec![1]),
            ("false", 4, vec![2]),
            ("-1", 9, vec![255; 8]),
            ("18446744073709551615", 10, vec![255; 8]),
            ("1.5", 11, vec![0, 0, 0, 0, 0, 0, 248, 63]),
            (r#""é""#, 12, vec![2, 195, 169]),
            (r#""\uD800""#, 12, vec![3, 239, 191, 189]),
            (r#""\uDFFF""#, 12, vec![3, 239, 191, 189]),
            (r#""\uD83D\uDE00""#, 12, vec![4, 240, 159, 152, 128]),
        ] {
            assert_eq!(native_json_parse(text), Ok((tag, bytes)));
        }
        assert_eq!(
            native_json_parse("[null,true]"),
            Ok((
                3,
                vec![2, 0, 0, 0, 18, 0, 0, 0, 4, 0, 0, 0, 0, 4, 1, 0, 0, 0]
            ))
        );
        assert_eq!(
            native_json_parse(r#"{"b":true,"a":null}"#),
            Ok((
                1,
                vec![
                    2, 0, 0, 0, 32, 0, 0, 0, 30, 0, 0, 0, 1, 0, 31, 0, 0, 0, 1, 0, 4, 0, 0, 0, 0,
                    4, 1, 0, 0, 0, 97, 98
                ]
            ))
        );
        assert_eq!(native_json_parse("\u{2003} \t"), Err(E::EmptyDocument));
        assert_eq!(native_json_parse("\u{2003}null"), Err(E::InvalidText)); // nonempty input is not trimmed for serde
        assert_eq!(native_json_parse("null true"), Err(E::TrailingValues));
        assert_eq!(native_json_parse(r#""\uD800" true"#), Err(E::InvalidText)); // retry trailing error is not reclassified
        assert_eq!(native_json_parse("1e400"), Err(E::InvalidText));
        // A successful first parse does not sanitize an escaped backslash.
        assert_eq!(
            native_json_parse(r#""\\uD800""#),
            Ok((12, vec![6, 92, 117, 68, 56, 48, 48]))
        );
        // A separate malformed surrogate activates the GLOBAL retry scan,
        // which also rewrites that escaped backslash's following sequence.
        assert_eq!(
            native_json_parse(r#"["\\uD800","\uD800"]"#),
            Ok((
                3,
                vec![
                    2, 0, 0, 0, 29, 0, 0, 0, 12, 18, 0, 0, 0, 12, 25, 0, 0, 0, 6, 92, 117, 102,
                    102, 102, 100, 3, 239, 191, 189
                ]
            ))
        );
    }
    #[test]
    fn serde_encoder_keeps_depth_bound_empty_container_exception_and_key_preflight_order() {
        fn nested(mut value: Value, count: usize) -> Value {
            for _ in 0..count {
                value = Value::Array(vec![value]);
            }
            value
        }
        let empty_at_100 = nested(Value::Array(Vec::new()), 100);
        let (tag, bytes) = native_json_from_value(&empty_at_100).unwrap();
        assert_eq!(tag, 3);
        assert_eq!(bytes.len(), 1308);
        assert_eq!(&bytes[..8], &[1, 0, 0, 0, 28, 5, 0, 0]);
        assert_eq!(&bytes[bytes.len() - 8..], &[0, 0, 0, 0, 8, 0, 0, 0]);
        let empty_object_at_100 = nested(Value::Object(serde_json::Map::new()), 100);
        assert_eq!(
            native_json_from_value(&empty_object_at_100)
                .unwrap()
                .1
                .len(),
            1308
        );
        assert_eq!(
            native_json_from_value(&nested(Value::Null, 101)),
            Err(E::TooDeep)
        );
        let mut keys = serde_json::Map::new();
        keys.insert("a".into(), nested(Value::Null, 101));
        keys.insert("z".repeat(65_536), Value::Null);
        assert_eq!(
            native_json_from_value(&Value::Object(keys)),
            Err(E::KeyTooLong)
        ); // ALL keys before the first deep child
        let mut keys = serde_json::Map::new();
        keys.insert("x".repeat(65_536), Value::Null);
        let long_key_object = Value::Object(keys);
        assert_eq!(
            native_json_from_value(&nested(long_key_object.clone(), 100)),
            Err(E::KeyTooLong)
        ); // object admitted; preflight precedes child depth check
        assert_eq!(
            native_json_from_value(&nested(long_key_object, 101)),
            Err(E::TooDeep)
        ); // depth check precedes entering this object
        let text = format!("{}0{}", "[".repeat(101), "]".repeat(101));
        assert_eq!(native_json_parse(&text), Err(E::TooDeep));
        let beyond_serde = format!("{}0{}", "[".repeat(256), "]".repeat(256));
        assert_eq!(native_json_parse(&beyond_serde), Err(E::InvalidText)); // original serde limit, including retry, is not the codec depth error
    }
}
