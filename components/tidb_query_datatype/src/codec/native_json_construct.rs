// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native typed JSON construction. A noncapturing, one-layer representation
//! projection exposes actual borrowed inputs; recursion, number selection and
//! error ordering stay here. Native serde-container parsing remains separate.
use std::collections::BTreeMap;

use serde_json::Number;

use super::mysql::{
    json::{
        NativeBinaryJsonEncodeError, NativeBinaryJsonError, NativeJsonNode,
        decode_native_binary_json_node, encode_native_binary_json_node,
    },
    time::{NativeTemporalValue, TimeType},
};

/// One actual typed source value. The child slice/map is borrowed, not eagerly
/// converted into another tree. Binary means decode-and-reencode here, unlike
/// the distinct Datum JSON clone operation.
#[derive(Debug)]
pub enum NativeJsonTypedInput<'a, T> {
    Null,
    Bool(bool),
    Int64(i64),
    Uint64(u64),
    Float64(f64),
    Number(&'a str),
    String(&'a str),
    Binary { type_code: u8, value: &'a [u8] },
    Array(&'a [T]),
    Object(&'a BTreeMap<String, T>),
    Opaque { type_code: u8, bytes: &'a [u8] },
    Time(NativeTemporalValue),
    Duration { nanoseconds: i64, fsp: i64 },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeJsonConstructError {
    InvalidText,
    InvalidBinary,
    TooDeep,
    KeyTooLong,
}

/// Literal payload construction, retaining the supplied byte verbatim.
pub fn native_json_literal(value: u8) -> (u8, Vec<u8>) {
    (0x04, vec![value])
}
/// Typed signed identity is not routed through a text number.
pub fn native_json_from_i64(value: i64) -> (u8, Vec<u8>) {
    (0x09, value.to_le_bytes().to_vec())
}
/// Typed unsigned identity survives even for small positive values.
pub fn native_json_from_u64(value: u64) -> (u8, Vec<u8>) {
    (0x0a, value.to_le_bytes().to_vec())
}
fn float64_payload(value: f64) -> (u8, Vec<u8>) {
    (0x0b, value.to_bits().to_le_bytes().to_vec())
}
/// The original serde Number ordering: signed, unsigned, then double.
pub fn native_json_encode_number(
    number: &Number,
) -> Result<(u8, Vec<u8>), NativeJsonConstructError> {
    if let Some(value) = number.as_i64() {
        Ok(native_json_from_i64(value))
    } else if let Some(value) = number.as_u64() {
        Ok(native_json_from_u64(value))
    } else {
        number
            .as_f64()
            .map(float64_payload)
            .ok_or(NativeJsonConstructError::InvalidText)
    }
}
/// Reject nonfinite values at the original Number::from_f64 boundary. This
/// accepts an actual binary64 value; callers with a Float32 datum must not
/// silently narrow its stored f64 before reaching this constructor.
pub fn native_json_from_f64(value: f64) -> Result<(u8, Vec<u8>), NativeJsonConstructError> {
    Number::from_f64(value)
        .ok_or(NativeJsonConstructError::InvalidText)
        .and_then(|number| native_json_encode_number(&number))
}
fn encode_uvarint(mut value: usize, output: &mut Vec<u8>) {
    while value >= 0x80 {
        output.push((value as u8) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}
/// UTF-8 string payload, with the native unsigned-varint byte length.
pub fn native_json_from_string(text: &str) -> (u8, Vec<u8>) {
    let mut bytes = Vec::new();
    encode_uvarint(text.len(), &mut bytes);
    bytes.extend_from_slice(text.as_bytes());
    (0x0c, bytes)
}
/// Source-layout opaque payload; no character decoding or type validation.
pub fn native_json_from_opaque(type_code: u8, value: &[u8]) -> (u8, Vec<u8>) {
    let mut bytes = Vec::with_capacity(2 + value.len());
    bytes.push(type_code);
    encode_uvarint(value.len(), &mut bytes);
    bytes.extend_from_slice(value);
    (0x0d, bytes)
}
/// Raw calendar bits and kind select the native tag. FSP is not encoded, and
/// no constructor normalizes the calendar or independent temporal metadata.
pub fn native_json_from_time(value: NativeTemporalValue) -> (u8, Vec<u8>) {
    let type_code = match value.kind {
        TimeType::Date => 0x0e,
        TimeType::DateTime => 0x0f,
        TimeType::Timestamp => 0x10,
    };
    (type_code, value.raw.to_le_bytes().to_vec())
}
/// The original unchecked FSP cast is part of the stored duration payload.
pub fn native_json_from_duration(nanoseconds: i64, fsp: i64) -> (u8, Vec<u8>) {
    let mut bytes = nanoseconds.to_le_bytes().to_vec();
    bytes.extend_from_slice(&(fsp as u32).to_le_bytes());
    (0x11, bytes)
}
fn decode_error(error: NativeBinaryJsonError) -> NativeJsonConstructError {
    match error {
        NativeBinaryJsonError::InvalidBinary => NativeJsonConstructError::InvalidBinary,
        NativeBinaryJsonError::TooDeep => NativeJsonConstructError::TooDeep,
    }
}
fn encode_error(error: NativeBinaryJsonEncodeError) -> NativeJsonConstructError {
    match error {
        NativeBinaryJsonEncodeError::InvalidBinary => NativeJsonConstructError::InvalidBinary,
        NativeBinaryJsonEncodeError::TooDeep => NativeJsonConstructError::TooDeep,
        NativeBinaryJsonEncodeError::KeyTooLong => NativeJsonConstructError::KeyTooLong,
    }
}
fn typed_to_node<T>(
    value: &T,
    view: for<'a> fn(&'a T) -> NativeJsonTypedInput<'a, T>,
) -> Result<NativeJsonNode<(u8, Vec<u8>)>, NativeJsonConstructError> {
    use NativeJsonTypedInput as I;
    let scalar = |value| Ok(NativeJsonNode::Scalar(value));
    match view(value) {
        I::Null => scalar(native_json_literal(0)),
        I::Bool(true) => scalar(native_json_literal(1)),
        I::Bool(false) => scalar(native_json_literal(2)),
        I::Int64(value) => scalar(native_json_from_i64(value)),
        I::Uint64(value) => scalar(native_json_from_u64(value)),
        I::Float64(value) => native_json_from_f64(value).and_then(scalar),
        I::Number(text) => {
            if let Ok(value) = text.parse::<i64>() {
                scalar(native_json_from_i64(value))
            } else if let Ok(value) = text.parse::<u64>() {
                scalar(native_json_from_u64(value))
            } else {
                native_json_from_f64(
                    text.parse::<f64>()
                        .map_err(|_| NativeJsonConstructError::InvalidText)?,
                )
                .and_then(scalar)
            }
        }
        I::String(text) => {
            let text = text.to_owned();
            scalar(native_json_from_string(&text))
        }
        I::Binary { type_code, value } => {
            decode_native_binary_json_node(type_code, value).map_err(decode_error)
        }
        I::Array(values) => values
            .iter()
            .map(|value| typed_to_node(value, view))
            .collect::<Result<Vec<_>, _>>()
            .map(NativeJsonNode::Array),
        I::Object(values) => values
            .iter()
            .map(|(key, value)| Ok((key.clone(), typed_to_node(value, view)?)))
            .collect::<Result<Vec<_>, NativeJsonConstructError>>()
            .map(NativeJsonNode::Object),
        I::Opaque { type_code, bytes } => {
            let bytes = bytes.to_vec();
            scalar(native_json_from_opaque(type_code, &bytes))
        }
        I::Time(value) => scalar(native_json_from_time(value)),
        I::Duration { nanoseconds, fsp } => scalar(native_json_from_duration(nanoseconds, fsp)),
    }
}
/// Construct the ENTIRE native logical tree before the existing node encoder
/// checks its depth. Moving that check into construction changes which error
/// wins when a deep input also contains an invalid number or embedded binary.
pub fn native_json_from_typed<T>(
    value: &T,
    view: for<'a> fn(&'a T) -> NativeJsonTypedInput<'a, T>,
) -> Result<(u8, Vec<u8>), NativeJsonConstructError> {
    let node = typed_to_node(value, view)?;
    encode_native_binary_json_node(&node, |value| (value.0, value.1.as_slice()))
        .map_err(encode_error)
}

#[cfg(test)]
mod tests {
    use NativeJsonConstructError as E;

    use super::*;
    enum Value {
        Null,
        Bool(bool),
        Int(i64),
        UInt(u64),
        Float(f64),
        Number(String),
        String(String),
        Binary(u8, Vec<u8>),
        Array(Vec<Value>),
        Object(BTreeMap<String, Value>),
        Opaque(u8, Vec<u8>),
        Time(NativeTemporalValue),
        Duration(i64, i64),
    }
    fn view(value: &Value) -> NativeJsonTypedInput<'_, Value> {
        use NativeJsonTypedInput as I;
        match value {
            Value::Null => I::Null,
            Value::Bool(value) => I::Bool(*value),
            Value::Int(value) => I::Int64(*value),
            Value::UInt(value) => I::Uint64(*value),
            Value::Float(value) => I::Float64(*value),
            Value::Number(value) => I::Number(value),
            Value::String(value) => I::String(value),
            Value::Binary(type_code, value) => I::Binary {
                type_code: *type_code,
                value,
            },
            Value::Array(value) => I::Array(value),
            Value::Object(value) => I::Object(value),
            Value::Opaque(type_code, bytes) => I::Opaque {
                type_code: *type_code,
                bytes,
            },
            Value::Time(value) => I::Time(*value),
            Value::Duration(nanoseconds, fsp) => I::Duration {
                nanoseconds: *nanoseconds,
                fsp: *fsp,
            },
        }
    }
    #[test]
    fn typed_constructor_preserves_scalar_identity_raw_metadata_and_container_bytes() {
        let cases = [
            (Value::Null, 0x04, vec![0]),
            (Value::Bool(true), 0x04, vec![1]),
            (Value::Bool(false), 0x04, vec![2]),
            (Value::Int(-1), 0x09, vec![255; 8]),
            (Value::UInt(1), 0x0a, vec![1, 0, 0, 0, 0, 0, 0, 0]),
            (Value::Float(1.5), 0x0b, vec![0, 0, 0, 0, 0, 0, 248, 63]),
            (Value::Float(-0.0), 0x0b, vec![0, 0, 0, 0, 0, 0, 0, 128]),
            (
                Value::Number("+1".into()),
                0x09,
                vec![1, 0, 0, 0, 0, 0, 0, 0],
            ),
            (
                Value::Number("18446744073709551615".into()),
                0x0a,
                vec![255; 8],
            ),
            (
                Value::Number("1.5".into()),
                0x0b,
                vec![0, 0, 0, 0, 0, 0, 248, 63],
            ),
            (Value::String("é".into()), 0x0c, vec![2, 195, 169]),
            (
                Value::Opaque(253, vec![0xff, 0]),
                0x0d,
                vec![253, 2, 255, 0],
            ),
            (Value::Duration(-1, -1), 0x11, vec![255; 12]),
            (
                Value::Binary(0x09, vec![1, 0, 0, 0, 0, 0, 0, 0]),
                0x09,
                vec![1, 0, 0, 0, 0, 0, 0, 0],
            ),
        ];
        for (input, type_code, bytes) in cases {
            assert_eq!(native_json_from_typed(&input, view), Ok((type_code, bytes)));
        }
        for (kind, type_code) in [
            (TimeType::Date, 0x0e),
            (TimeType::DateTime, 0x0f),
            (TimeType::Timestamp, 0x10),
        ] {
            let time = NativeTemporalValue {
                raw: 0x0807_0605_0403_0201,
                kind,
                fsp: u8::MAX,
            };
            assert_eq!(
                native_json_from_typed(&Value::Time(time), view),
                Ok((type_code, vec![1, 2, 3, 4, 5, 6, 7, 8]))
            );
        }
        let array = Value::Array(vec![Value::Null, Value::Bool(true), Value::UInt(1)]);
        assert_eq!(
            native_json_from_typed(&array, view),
            Ok((
                0x03,
                vec![
                    3, 0, 0, 0, 31, 0, 0, 0, 4, 0, 0, 0, 0, 4, 1, 0, 0, 0, 10, 23, 0, 0, 0, 1, 0,
                    0, 0, 0, 0, 0, 0
                ]
            ))
        );
        let object = Value::Object(BTreeMap::from([("a".into(), Value::Null)]));
        assert_eq!(
            native_json_from_typed(&object, view),
            Ok((
                0x01,
                vec![
                    1, 0, 0, 0, 20, 0, 0, 0, 19, 0, 0, 0, 1, 0, 4, 0, 0, 0, 0, 97
                ]
            ))
        );
        let text = "x".repeat(128);
        let mut bytes = vec![128, 1];
        bytes.extend_from_slice(text.as_bytes());
        assert_eq!(native_json_from_string(&text), (0x0c, bytes));
        // The shared double primitive must retain the stored f64, not narrow
        // it merely because a future Datum caller names that source Float32.
        assert_eq!(
            native_json_from_f64(16_777_217.0),
            Ok((0x0b, vec![0, 0, 0, 16, 0, 0, 112, 65]))
        );
    }
    #[test]
    fn typed_constructor_keeps_build_before_encode_errors_and_binary_validation() {
        for number in [" 1", "bad", "1e400"] {
            assert_eq!(
                native_json_from_typed(&Value::Number(number.into()), view),
                Err(E::InvalidText)
            );
        }
        for number in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                native_json_from_typed(&Value::Float(number), view),
                Err(E::InvalidText)
            );
        }
        assert_eq!(
            native_json_from_typed(&Value::Binary(0x09, vec![]), view),
            Err(E::InvalidBinary)
        );
        assert_eq!(
            native_json_from_typed(&Value::Binary(0x01, vec![]), view),
            Err(E::InvalidBinary)
        );
        let mut deep = Value::Null;
        for _ in 0..101 {
            deep = Value::Array(vec![deep]);
        }
        assert_eq!(native_json_from_typed(&deep, view), Err(E::TooDeep));
        let deep_then_bad = Value::Array(vec![deep, Value::Number("bad".into())]);
        assert_eq!(
            native_json_from_typed(&deep_then_bad, view),
            Err(E::InvalidText)
        ); // a premature depth check would incorrectly win
        let mut deep_bad = Value::Binary(0x09, vec![]);
        for _ in 0..101 {
            deep_bad = Value::Array(vec![deep_bad]);
        }
        assert_eq!(
            native_json_from_typed(&deep_bad, view),
            Err(E::InvalidBinary)
        );
        let long_key = "x".repeat(65_536);
        assert_eq!(
            native_json_from_typed(
                &Value::Object(BTreeMap::from([(long_key.clone(), Value::Null)])),
                view
            ),
            Err(E::KeyTooLong)
        );
        assert_eq!(
            native_json_from_typed(
                &Value::Object(BTreeMap::from([(long_key, Value::Number("bad".into()))])),
                view
            ),
            Err(E::InvalidText)
        );
        // Embedded scalar content is validated by the existing native decoder,
        // not cloned as the distinct Datum::Json conversion would do.
        assert_eq!(
            native_json_from_typed(
                &Value::Binary(0x0b, f64::INFINITY.to_le_bytes().to_vec()),
                view
            ),
            Err(E::InvalidBinary)
        );
    }
}
