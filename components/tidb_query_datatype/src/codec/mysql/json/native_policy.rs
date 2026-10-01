// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native JSON representation policies. Parsing and binary validation retain
//! their existing boundaries; type names and depth use the shared kernels.

use codec::number::NumberCodec;
use serde_json::Value;

use super::{
    JsonType,
    constants::{LITERAL_LEN, NUMBER_LEN, TYPE_LEN},
    jcodec::{array_metadata_len, object_metadata_len, out_of_line_payload_len},
    json_type::json_type_name,
};

/// The native document/parser and binary-view errors, without SQL rendering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeJsonError {
    EmptyText,
    InvalidText,
    InvalidBinary,
}

/// Parses with the native serde value model and its default recursion limit.
/// Do not substitute binary JSON's numeric visitor or key-length encoding.
pub fn parse_native_json_document(text: &str) -> Result<Value, NativeJsonError> {
    if text.trim().is_empty() {
        return Err(NativeJsonError::EmptyText);
    }
    serde_json::from_str(text).map_err(|_| NativeJsonError::InvalidText)
}

/// Measures the native text-domain binary layout, including the root type byte.
/// This shares the encoder's metadata and inline-literal layout rules without
/// encoding a document, narrowing its keys, or reclassifying its numbers.
/// The contract is ordinary constructible sizes, not usize overflow or OOM
/// equivalence with an allocating binary encoder.
pub fn native_json_storage_size(value: &Value) -> usize {
    native_json_value_size(value) + TYPE_LEN
}

fn native_json_child_payload_size(value: &Value) -> usize {
    out_of_line_payload_len(
        matches!(value, Value::Null | Value::Bool(_)),
        native_json_value_size(value),
    )
}

fn native_json_value_size(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(_) => LITERAL_LEN,
        Value::Number(_) => NUMBER_LEN,
        Value::String(text) => {
            let mut prefix = [0; 10];
            NumberCodec::encode_var_u64(&mut prefix, text.len() as u64) + text.len()
        }
        Value::Array(values) => {
            array_metadata_len(values.len())
                + values
                    .iter()
                    .map(native_json_child_payload_size)
                    .sum::<usize>()
        }
        Value::Object(values) => {
            object_metadata_len(values.len())
                + values.keys().map(|key| key.len()).sum::<usize>()
                + values
                    .values()
                    .map(native_json_child_payload_size)
                    .sum::<usize>()
        }
    }
}

/// Classifies a parsed native value, retaining the signed-boundary preference.
pub fn native_json_type_name(value: &Value) -> &'static [u8] {
    let kind = match value {
        Value::Null | Value::Bool(_) => JsonType::Literal,
        Value::Number(number) if number.is_i64() => JsonType::I64,
        Value::Number(number) if number.is_u64() => JsonType::U64,
        Value::Number(_) => JsonType::Double,
        Value::String(_) => JsonType::String,
        Value::Array(_) => JsonType::Array,
        Value::Object(_) => JsonType::Object,
    };
    json_type_name(kind, value.is_null(), None)
}

/// Classifies the native type-code/payload pair without whole-document checks.
/// Unlike wire literals, only the exact one-byte null payload means NULL.
/// Opaque values retain the native exact-length validation before
/// classification.
pub fn native_binary_json_type_name(
    type_code: u8,
    value: &[u8],
) -> Result<&'static [u8], NativeJsonError> {
    let kind = JsonType::try_from(type_code).map_err(|_| NativeJsonError::InvalidBinary)?;
    let opaque_type = if kind == JsonType::Opaque {
        Some(native_json_opaque(type_code, value)?.0)
    } else {
        None
    };
    Ok(json_type_name(kind, value == [0], opaque_type))
}

/// Borrows a native opaque payload, preserving its original framing checks.
/// The arithmetic and uvarint overflow behavior are intentionally unchanged.
pub fn native_json_opaque(type_code: u8, value: &[u8]) -> Result<(u8, &[u8]), NativeJsonError> {
    if type_code != JsonType::Opaque as u8 {
        return Err(NativeJsonError::InvalidBinary);
    }
    let (&type_code, payload) = value.split_first().ok_or(NativeJsonError::InvalidBinary)?;
    let (length, prefix) = decode_native_json_uvarint(payload)?;
    let bytes = payload
        .get(prefix..prefix + length)
        .ok_or(NativeJsonError::InvalidBinary)?;
    if prefix + length != payload.len() {
        return Err(NativeJsonError::InvalidBinary);
    }
    Ok((type_code, bytes))
}

/// The native decoder, including its original ten-byte and usize-shift domain.
pub fn decode_native_json_uvarint(bytes: &[u8]) -> Result<(usize, usize), NativeJsonError> {
    let mut value = 0_usize;
    for (index, byte) in bytes.iter().copied().enumerate().take(10) {
        value |= usize::from(byte & 0x7f) << (index * 7);
        if byte < 0x80 {
            return Ok((value, index + 1));
        }
    }
    Err(NativeJsonError::InvalidBinary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::mysql::json::{Json, JsonRef, native_json_depth};

    #[test]
    fn native_storage_size_keeps_text_layout_and_long_keys() {
        // Existing native json2::json_storage_size_matches_go_vectors values.
        for (text, expected) in [
            ("null", 2),
            ("true", 2),
            ("1", 9),
            (r#""1""#, 3),
            ("{}", 9),
            (r#"{"a":1}"#, 29),
            (r#"[{"a":{"a":1},"b":2}]"#, 82),
            (r#"{"a": 1000, "b": "wxyz", "c": "[1, 3, 5, 7]"}"#, 71),
        ] {
            let value = parse_native_json_document(text).unwrap();
            assert_eq!(native_json_storage_size(&value), expected, "{text}");
        }
        // Original layout arithmetic: root 1 + header 8 + 3 inline entries * 5.
        let inline = parse_native_json_document("[null,true,false]").unwrap();
        assert_eq!(native_json_storage_size(&inline), 24);
        // Last key wins: root 1 + header 8 + one entry (6+5) + key byte 1.
        let duplicate = parse_native_json_document(r#"{"a":[1,2],"a":null}"#).unwrap();
        assert_eq!(native_json_storage_size(&duplicate), 21);
        let long_key = "k".repeat(65_536);
        let document = format!("{{\"{}\":null}}", long_key);
        let value = parse_native_json_document(&document).unwrap();
        assert_eq!(native_json_storage_size(&value), 65_556);
        // Root 1 + two-byte varuint length + 128 UTF-8 bytes.
        assert_eq!(
            native_json_storage_size(&Value::String("x".repeat(128))),
            131
        );
    }

    #[test]
    fn native_json_type_policies_keep_numeric_and_binary_boundaries() {
        let signed = parse_native_json_document("9223372036854775807").unwrap();
        assert_eq!(native_json_type_name(&signed), b"INTEGER");
        let unsigned = parse_native_json_document("18446744073709551615").unwrap();
        assert_eq!(native_json_type_name(&unsigned), b"UNSIGNED INTEGER");
        assert_eq!(
            native_json_type_name(&parse_native_json_document("1.0").unwrap()),
            b"DOUBLE"
        );
        assert_eq!(
            parse_native_json_document(" \t"),
            Err(NativeJsonError::EmptyText)
        );
        assert_eq!(
            parse_native_json_document("["),
            Err(NativeJsonError::InvalidText)
        );

        assert_eq!(native_binary_json_type_name(0x04, &[0]).unwrap(), b"NULL");
        for payload in [&[][..], &[9][..], &[0, 0][..]] {
            assert_eq!(
                native_binary_json_type_name(0x04, payload).unwrap(),
                b"BOOLEAN"
            );
        }
        assert_eq!(JsonRef::new(JsonType::Literal, &[9]).json_type(), b"NULL");
        assert_eq!(native_binary_json_type_name(0x0e, &[]).unwrap(), b"DATE");
        assert_eq!(
            native_binary_json_type_name(0xff, &[]),
            Err(NativeJsonError::InvalidBinary)
        );
        let opaque = [0xfc, 1, b'x'];
        assert_eq!(
            native_json_opaque(0x0d, &opaque).unwrap(),
            (0xfc, &b"x"[..])
        );
        assert_eq!(
            native_binary_json_type_name(0x0d, &opaque).unwrap(),
            b"BLOB"
        );
        let bad_opaque = [0xfc, 1, b'x', b'y'];
        assert_eq!(
            native_json_opaque(0x0d, &bad_opaque),
            Err(NativeJsonError::InvalidBinary)
        );
        assert_eq!(
            JsonRef::new(JsonType::Opaque, &bad_opaque).json_type(),
            b"BLOB"
        );
        assert_eq!(decode_native_json_uvarint(&[0x81, 0]), Ok((1, 2)));
    }

    #[test]
    fn native_and_wire_depth_share_traversal_without_narrowing_keys() {
        let text = r#"{"a":[1,{"b":[]}],"a":[0]}"#;
        let native = parse_native_json_document(text).unwrap();
        let wire: Json = text.parse().unwrap();
        assert_eq!(native_json_depth(&native).unwrap(), 3);
        assert_eq!(
            native_json_depth(&native).unwrap(),
            wire.as_ref().depth().unwrap()
        );
        let long_key = "x".repeat(65_536);
        let text = format!("{{\"{}\":[{{\"leaf\":18446744073709551615}}]}}", long_key);
        let native = parse_native_json_document(&text).unwrap();
        assert_eq!(
            native.as_object().unwrap().keys().next().unwrap().len(),
            65_536
        );
        assert_eq!(native_json_depth(&native).unwrap(), 4);
        assert_eq!(native_json_depth(&Value::Null).unwrap(), 1);
        assert_eq!(native_json_depth(&Value::Array(Vec::new())).unwrap(), 1);
    }
}
